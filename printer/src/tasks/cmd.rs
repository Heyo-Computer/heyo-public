use super::cli::*;
use super::model::{Status, Task, parse_id};
use super::store;
use anyhow::{Result, bail};
use std::io::Read;

pub fn dispatch(args: TaskArgs) -> Result<()> {
    let dir = store::tasks_dir(args.tasks_dir.as_deref())?;
    match args.command {
        TaskCommand::Create(a) => cmd_create(&dir, a),
        TaskCommand::List(a) => cmd_list(&dir, a),
        TaskCommand::Show(a) => cmd_show(&dir, &a.id),
        TaskCommand::Ready => cmd_ready(&dir),
        TaskCommand::Start(a) => cmd_start(&dir, a),
        TaskCommand::Done(a) => cmd_done(&dir, a),
        TaskCommand::Block(a) => cmd_block(&dir, a),
        TaskCommand::Unblock(a) => cmd_unblock(&dir, &a.id),
        TaskCommand::Release(a) => cmd_release(&dir, &a.id),
        TaskCommand::Comment(a) => cmd_comment(&dir, a),
        TaskCommand::Depends(a) => cmd_depends(&dir, a),
    }
}

fn cmd_create(dir: &std::path::Path, args: CreateArgs) -> Result<()> {
    if args.priority < 1 || args.priority > 5 {
        bail!("priority must be between 1 and 5 (got {})", args.priority);
    }
    for dep in &args.depends_on {
        // Validate id format up front; we don't require the dep to exist
        // (matches `compute_ready` which treats missing deps as satisfied),
        // but the id must be parseable.
        parse_id(dep)?;
    }

    let description = match args.description.as_deref() {
        Some("-") => {
            let mut buf = String::new();
            std::io::stdin().read_to_string(&mut buf)?;
            buf.trim_end().to_string()
        }
        Some(s) => s.to_string(),
        None => String::new(),
    };

    let title = args.title;
    let priority = args.priority;
    let labels = args.labels;
    let depends_on = args.depends_on;
    let task = store::create_with_next_id(dir, move |id| {
        let mut t = Task::new(id, title);
        t.meta.priority = priority;
        t.meta.labels = labels;
        t.meta.depends_on = depends_on;
        if !description.is_empty() {
            t.body = format!("{description}\n");
        }
        t
    })?;
    println!("{}  created: {}", task.meta.id, task.meta.title);
    Ok(())
}

fn cmd_list(dir: &std::path::Path, args: ListArgs) -> Result<()> {
    let tasks = store::list_all(dir)?;
    let me = current_user();
    let owner_filter: Option<String> = if args.mine {
        Some(me.clone())
    } else {
        args.owner
    };
    let filtered: Vec<&Task> = tasks
        .iter()
        .filter(|t| args.status.is_none_or(|s| t.meta.status == s))
        .filter(|t| {
            args.label
                .as_ref()
                .is_none_or(|l| t.meta.labels.iter().any(|x| x == l))
        })
        .filter(|t| owner_filter.as_ref().is_none_or(|o| &t.meta.owner == o))
        .collect();
    print_table(&filtered);
    Ok(())
}

fn cmd_show(dir: &std::path::Path, id: &str) -> Result<()> {
    parse_id(id)?;
    let task = store::read_task(dir, id)?;
    println!("id:           {}", task.meta.id);
    println!("title:        {}", task.meta.title);
    println!("status:       {}", task.meta.status);
    println!("priority:     P{}", task.meta.priority);
    println!(
        "owner:        {}",
        if task.meta.owner.is_empty() {
            "—"
        } else {
            &task.meta.owner
        }
    );
    println!("created_at:   {}", task.meta.created_at);
    println!("updated_at:   {}", task.meta.updated_at);
    if !task.meta.labels.is_empty() {
        println!("labels:       {}", task.meta.labels.join(", "));
    }
    if !task.meta.depends_on.is_empty() {
        println!("depends_on:   {}", task.meta.depends_on.join(", "));
    }
    if !task.meta.blocked_reason.is_empty() {
        println!("blocked:      {}", task.meta.blocked_reason);
    }
    if !task.body.trim().is_empty() {
        println!("\n{}", task.body.trim_end());
    }
    Ok(())
}

fn cmd_ready(dir: &std::path::Path) -> Result<()> {
    let tasks = store::list_all(dir)?;
    let ready = store::compute_ready(&tasks);
    print_table(&ready);
    Ok(())
}

fn cmd_start(dir: &std::path::Path, args: StartArgs) -> Result<()> {
    parse_id(&args.id)?;
    let mut task = store::read_task(dir, &args.id)?;
    let new_owner = args.owner.unwrap_or_else(current_user);
    if !task.meta.owner.is_empty() && task.meta.owner != new_owner && !args.force {
        bail!(
            "{} is already claimed by `{}`. Use --force to override or run `printer task release {}` first.",
            task.meta.id,
            task.meta.owner,
            task.meta.id
        );
    }
    if task.meta.status == Status::Done {
        bail!("{} is already done", task.meta.id);
    }
    task.meta.status = Status::InProgress;
    task.meta.owner = new_owner.clone();
    task.meta.blocked_reason.clear();
    task.touch();
    store::write_task(dir, &task)?;
    println!("{} claimed by {}", task.meta.id, new_owner);
    Ok(())
}

fn cmd_done(dir: &std::path::Path, args: DoneArgs) -> Result<()> {
    parse_id(&args.id)?;
    let mut task = store::read_task(dir, &args.id)?;
    task.meta.status = Status::Done;
    task.meta.blocked_reason.clear();
    if let Some(note) = args.note {
        append_note(&mut task, &note);
    }
    task.touch();
    store::write_task(dir, &task)?;
    println!("{} done", task.meta.id);
    Ok(())
}

fn cmd_block(dir: &std::path::Path, args: BlockArgs) -> Result<()> {
    parse_id(&args.id)?;
    let mut task = store::read_task(dir, &args.id)?;
    task.meta.status = Status::Blocked;
    task.meta.blocked_reason = args.reason;
    task.touch();
    store::write_task(dir, &task)?;
    println!("{} blocked", task.meta.id);
    Ok(())
}

fn cmd_unblock(dir: &std::path::Path, id: &str) -> Result<()> {
    parse_id(id)?;
    let mut task = store::read_task(dir, id)?;
    if task.meta.status != Status::Blocked {
        bail!("{} is not blocked (status = {})", id, task.meta.status);
    }
    task.meta.status = Status::Open;
    task.meta.blocked_reason.clear();
    task.touch();
    store::write_task(dir, &task)?;
    println!("{} unblocked", task.meta.id);
    Ok(())
}

fn cmd_release(dir: &std::path::Path, id: &str) -> Result<()> {
    parse_id(id)?;
    let mut task = store::read_task(dir, id)?;
    let prev_owner = task.meta.owner.clone();
    task.meta.owner.clear();
    if task.meta.status == Status::InProgress {
        task.meta.status = Status::Open;
    }
    task.touch();
    store::write_task(dir, &task)?;
    println!(
        "{} released (was owned by {})",
        task.meta.id,
        if prev_owner.is_empty() {
            "—".to_string()
        } else {
            prev_owner
        }
    );
    Ok(())
}

fn cmd_comment(dir: &std::path::Path, args: CommentArgs) -> Result<()> {
    parse_id(&args.id)?;
    let mut task = store::read_task(dir, &args.id)?;
    append_note(&mut task, &args.text);
    task.touch();
    store::write_task(dir, &task)?;
    println!("{} comment appended", task.meta.id);
    Ok(())
}

fn cmd_depends(dir: &std::path::Path, args: DependsArgs) -> Result<()> {
    parse_id(&args.id)?;
    if args.add.is_empty() && args.remove.is_empty() {
        bail!("nothing to do; pass --add and/or --remove");
    }
    for d in args.add.iter().chain(args.remove.iter()) {
        parse_id(d)?;
    }
    let mut task = store::read_task(dir, &args.id)?;
    for d in &args.remove {
        task.meta.depends_on.retain(|x| x != d);
    }
    for d in &args.add {
        if d == &task.meta.id {
            bail!("a task cannot depend on itself");
        }
        if !task.meta.depends_on.iter().any(|x| x == d) {
            task.meta.depends_on.push(d.clone());
        }
    }
    task.meta.depends_on.sort();
    task.meta.depends_on.dedup();
    task.touch();
    store::write_task(dir, &task)?;
    println!(
        "{} depends_on = [{}]",
        task.meta.id,
        task.meta.depends_on.join(", ")
    );
    Ok(())
}

fn append_note(task: &mut Task, text: &str) {
    let stamp = super::model::now_iso();
    let user = current_user();
    let line = format!("- {stamp} [{user}] {text}\n");
    if task.body.contains("## Notes") {
        // Append at the very end. The body convention is that ## Notes is the
        // final section, so we don't try to be clever about insertion points.
        if !task.body.ends_with('\n') {
            task.body.push('\n');
        }
        task.body.push_str(&line);
    } else {
        if !task.body.is_empty() && !task.body.ends_with('\n') {
            task.body.push('\n');
        }
        if !task.body.is_empty() {
            task.body.push('\n');
        }
        task.body.push_str("## Notes\n\n");
        task.body.push_str(&line);
    }
}

fn current_user() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "unknown".to_string())
}

fn print_table(tasks: &[&Task]) {
    if tasks.is_empty() {
        println!("(no tasks)");
        return;
    }
    let id_w = tasks
        .iter()
        .map(|t| t.meta.id.len())
        .max()
        .unwrap_or(5)
        .max(2);
    let status_w = tasks
        .iter()
        .map(|t| t.meta.status.as_str().len())
        .max()
        .unwrap_or(6);
    let owner_w = tasks
        .iter()
        .map(|t| t.meta.owner.len().max(1))
        .max()
        .unwrap_or(5)
        .max(5);
    println!(
        "{:<id_w$}  {:<status_w$}  P  {:<owner_w$}  TITLE",
        "ID",
        "STATUS",
        "OWNER",
        id_w = id_w,
        status_w = status_w,
        owner_w = owner_w,
    );
    for t in tasks {
        let owner = if t.meta.owner.is_empty() {
            "—"
        } else {
            &t.meta.owner
        };
        println!(
            "{:<id_w$}  {:<status_w$}  {}  {:<owner_w$}  {}",
            t.meta.id,
            t.meta.status.as_str(),
            t.meta.priority,
            owner,
            t.meta.title,
            id_w = id_w,
            status_w = status_w,
            owner_w = owner_w,
        );
    }
}
