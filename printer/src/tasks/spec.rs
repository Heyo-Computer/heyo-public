use super::model::{Status, Task};
use super::store;
use anyhow::Result;
use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::Path;

/// One checklist item parsed out of the spec.
#[derive(Debug, Clone)]
pub struct SpecItem {
    pub title: String,
    pub description: String,
    pub initially_done: bool,
}

#[derive(Debug, Default)]
pub struct SyncReport {
    pub created: usize,
    pub existing: usize,
    pub closed: usize,
}

/// Parse a spec file's contents into a flat list of top-level checklist
/// items. The spec format is documented in the README — short version:
///
///   - `- [ ] Title`, `- [x] Title`, `* [ ] Title`, `+ [ ] Title` at column 0
///     are tasks. The line text after the checkbox is the title.
///   - Lines indented by 2+ spaces (or one tab) immediately after a task
///     line are appended to that task's description, with the leading
///     indent stripped.
///   - Blank lines inside a task description are preserved.
///   - Any unindented non-task line ends the current task.
///   - Content above the first task is treated as project preamble and
///     ignored for task creation.
pub fn parse_spec(content: &str) -> Vec<SpecItem> {
    let mut items: Vec<SpecItem> = Vec::new();
    let mut current: Option<SpecItem> = None;

    let flush = |cur: &mut Option<SpecItem>, items: &mut Vec<SpecItem>| {
        if let Some(mut item) = cur.take() {
            item.description = item.description.trim_end().to_string();
            items.push(item);
        }
    };

    for line in content.lines() {
        if let Some((done, title)) = match_top_task(line) {
            flush(&mut current, &mut items);
            current = Some(SpecItem {
                title: title.trim().to_string(),
                description: String::new(),
                initially_done: done,
            });
            continue;
        }

        if let Some(item) = current.as_mut() {
            if line.is_empty() {
                if !item.description.is_empty() {
                    item.description.push('\n');
                }
                continue;
            }
            if let Some(stripped) = line.strip_prefix("  ").or_else(|| line.strip_prefix('\t')) {
                if !item.description.is_empty() && !item.description.ends_with('\n') {
                    item.description.push('\n');
                }
                item.description.push_str(stripped);
                continue;
            }
            // Unindented non-task line — ends the current task.
            flush(&mut current, &mut items);
        }
        // else: line before any task → project preamble, ignored
    }
    flush(&mut current, &mut items);
    items
}

fn match_top_task(line: &str) -> Option<(bool, &str)> {
    const PREFIXES: &[(&str, bool)] = &[
        ("- [ ] ", false),
        ("- [x] ", true),
        ("- [X] ", true),
        ("* [ ] ", false),
        ("* [x] ", true),
        ("* [X] ", true),
        ("+ [ ] ", false),
        ("+ [x] ", true),
        ("+ [X] ", true),
    ];
    for (prefix, done) in PREFIXES {
        if let Some(rest) = line.strip_prefix(prefix) {
            return Some((*done, rest));
        }
    }
    None
}

/// Produce a stable anchor for a (spec_path, title) pair so that re-syncs
/// match items idempotently even if other items are added/removed/renamed.
pub fn anchor_for(spec_path: &Path, title: &str) -> String {
    let mut h = DefaultHasher::new();
    spec_path.to_string_lossy().as_bytes().hash(&mut h);
    title.hash(&mut h);
    format!("{:016x}", h.finish())
}

/// Reconcile parsed spec items with the on-disk task store. Idempotent.
pub fn sync_to_store(items: &[SpecItem], spec_path: &Path, tasks_dir: &Path) -> Result<SyncReport> {
    let existing = store::list_all(tasks_dir)?;
    let by_anchor: HashMap<String, &Task> = existing
        .iter()
        .filter(|t| !t.meta.spec_anchor.is_empty())
        .map(|t| (t.meta.spec_anchor.clone(), t))
        .collect();

    let mut report = SyncReport::default();

    for item in items {
        let anchor = anchor_for(spec_path, &item.title);
        if let Some(task) = by_anchor.get(&anchor) {
            // Existing: only side effect is closing it if the spec marked it [x]
            // and the task store hasn't caught up yet. We never reopen tasks
            // based on the spec — the task store's status wins for already-
            // synced items.
            if item.initially_done && task.meta.status != Status::Done {
                let mut t: Task = (*task).clone();
                t.meta.status = Status::Done;
                t.touch();
                store::write_task(tasks_dir, &t)?;
                report.closed += 1;
            } else {
                report.existing += 1;
            }
            continue;
        }

        // New item: atomically allocate an id and write the task.
        let title = item.title.clone();
        let description = item.description.clone();
        let initially_done = item.initially_done;
        let anchor_for_task = anchor.clone();
        let _ = store::create_with_next_id(tasks_dir, move |id| {
            let mut t = Task::new(id, title);
            if !description.is_empty() {
                t.body = format!("{description}\n");
            }
            t.meta.spec_anchor = anchor_for_task;
            if initially_done {
                t.meta.status = Status::Done;
            }
            t
        })?;
        report.created += 1;
    }

    Ok(report)
}
