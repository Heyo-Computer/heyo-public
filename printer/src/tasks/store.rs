use super::model::{Status, Task, format_id, from_file_string, to_file_string};
use anyhow::{Context, Result, anyhow};
use std::collections::HashMap;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// Resolve the tasks directory. If `override_dir` is set, use that. Otherwise
/// `<cwd>/.printer/tasks`. Creates the directory if missing.
pub fn tasks_dir(override_dir: Option<&Path>) -> Result<PathBuf> {
    let dir = match override_dir {
        Some(p) => p.to_path_buf(),
        None => std::env::current_dir()?.join(".printer").join("tasks"),
    };
    fs::create_dir_all(&dir).with_context(|| format!("creating tasks dir {}", dir.display()))?;
    Ok(dir)
}

pub fn task_path(dir: &Path, id: &str) -> PathBuf {
    dir.join(format!("{id}.md"))
}

pub fn read_task(dir: &Path, id: &str) -> Result<Task> {
    let path = task_path(dir, id);
    let raw = fs::read_to_string(&path)
        .with_context(|| format!("reading task {} ({})", id, path.display()))?;
    from_file_string(&raw)
}

/// Write a task atomically: write to `T-NNN.md.tmp` then rename over the
/// original. Renames are atomic on POSIX.
pub fn write_task(dir: &Path, task: &Task) -> Result<()> {
    let path = task_path(dir, &task.meta.id);
    let tmp = dir.join(format!("{}.md.tmp", task.meta.id));
    let content = to_file_string(task)?;
    {
        let mut f =
            fs::File::create(&tmp).with_context(|| format!("creating temp {}", tmp.display()))?;
        f.write_all(content.as_bytes())?;
        f.sync_all()?;
    }
    fs::rename(&tmp, &path)
        .with_context(|| format!("renaming {} -> {}", tmp.display(), path.display()))?;
    Ok(())
}

/// Allocate a fresh id and atomically create the task file with full content
/// in one shot. The id is chosen by claiming the first un-taken `T-NNN` slot
/// via O_EXCL, so concurrent invocations never collide. The caller passes a
/// closure that takes the chosen id and produces the in-memory Task; the
/// closure is only invoked once we've successfully claimed an id.
pub fn create_with_next_id<F>(dir: &Path, build_task: F) -> Result<Task>
where
    F: FnOnce(String) -> Task,
{
    let mut start = max_existing_id(dir)? + 1;
    loop {
        let id = format_id(start);
        let path = task_path(dir, &id);
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(mut file) => {
                let task = build_task(id);
                let content = to_file_string(&task)?;
                if let Err(e) = file
                    .write_all(content.as_bytes())
                    .and_then(|_| file.sync_all())
                {
                    // Clean up the placeholder so the id can be re-used.
                    let _ = fs::remove_file(&path);
                    return Err(anyhow!("writing task file {}: {e}", path.display()));
                }
                return Ok(task);
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                start += 1;
                continue;
            }
            Err(e) => return Err(anyhow!("creating task file {}: {e}", path.display())),
        }
    }
}

fn max_existing_id(dir: &Path) -> Result<u32> {
    let mut max = 0;
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if let Some(stem) = name.strip_suffix(".md")
            && let Some(num) = stem.strip_prefix("T-")
            && let Ok(n) = num.parse::<u32>()
        {
            max = max.max(n);
        }
    }
    Ok(max)
}

pub fn list_all(dir: &Path) -> Result<Vec<Task>> {
    let mut tasks = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.ends_with(".md") || !name.starts_with("T-") {
            continue;
        }
        let id = &name[..name.len() - 3];
        match read_task(dir, id) {
            Ok(t) => tasks.push(t),
            Err(e) => eprintln!("warning: skipping unreadable task {id}: {e}"),
        }
    }
    tasks.sort_by(|a, b| a.meta.id.cmp(&b.meta.id));
    Ok(tasks)
}

/// Compute the ready queue: open tasks whose every depends_on points to
/// either a Done task or a missing id (treated as satisfied so deletions
/// don't permanently block chains).
pub fn compute_ready(tasks: &[Task]) -> Vec<&Task> {
    let by_id: HashMap<&str, &Task> = tasks.iter().map(|t| (t.meta.id.as_str(), t)).collect();
    let mut ready: Vec<&Task> = tasks
        .iter()
        .filter(|t| t.meta.status == Status::Open)
        .filter(|t| {
            t.meta
                .depends_on
                .iter()
                .all(|dep| match by_id.get(dep.as_str()) {
                    Some(dep_task) => dep_task.meta.status == Status::Done,
                    None => true,
                })
        })
        .collect();
    ready.sort_by(|a, b| {
        a.meta
            .priority
            .cmp(&b.meta.priority)
            .then_with(|| a.meta.id.cmp(&b.meta.id))
    });
    ready
}
