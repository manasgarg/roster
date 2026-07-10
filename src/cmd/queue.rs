//! `roster queue` — file, list, and inspect tasks for the supervisor to dispatch.
//!
//!   roster queue add --worker <name> [--ceiling <m>] [--proactive] "<prompt>"
//!   roster queue ls
//!   roster queue show <id>

use crate::queue;

type BErr = Box<dyn std::error::Error>;

pub fn run(args: &[String]) -> Result<(), BErr> {
    match args.first().map(String::as_str).unwrap_or("ls") {
        "add" => add(&args[1..]),
        "ls" | "list" => ls(),
        "show" => show(args.get(1).ok_or("usage: roster queue show <id>")?),
        "requeue" => requeue(args.get(1).ok_or("usage: roster queue requeue <id>")?),
        other => Err(format!("unknown queue subcommand \"{other}\" (try: add, ls, show, requeue)").into()),
    }
}

/// Put a stuck or finished task back to `waiting` so the supervisor runs it
/// again. Refuses if a box for it is still live (would double-run).
fn requeue(id: &str) -> Result<(), BErr> {
    let mut t = queue::find(id).ok_or_else(|| format!("no such task {id}"))?;
    if t.state == "waiting" {
        println!("task {id} is already waiting");
        return Ok(());
    }
    if let Some(run) = &t.run_id {
        if crate::cmd::run_box::box_alive(run) {
            return Err(format!(
                "task {id} still has a live box ({run}) — let it finish, or `docker kill {}` first",
                crate::cmd::run_box::container_name(run)
            )
            .into());
        }
    }
    let prev = t.state.clone();
    t.run_id = None;
    queue::set_state(&mut t, "waiting").map_err(|e| e.to_string())?;
    println!("requeued {id}: {prev} → waiting");
    Ok(())
}

fn show(id: &str) -> Result<(), BErr> {
    let t = queue::find(id).ok_or_else(|| format!("no such task {id}"))?;
    println!("task     {}", t.id);
    println!("worker   {}", t.worker);
    println!("state    {}", t.state);
    println!("origin   {}{}", t.origin, if t.proactive { " (proactive)" } else { "" });
    println!("created  {}", t.created_at);
    println!("updated  {}", t.updated_at);
    println!("ceiling  {} min", t.ceiling_min);
    if let Some(run) = &t.run_id {
        println!("run      {run}   (transcript: runs/{run}/stdout.jsonl)");
    }
    if let Some(repo) = &t.repo {
        println!("repo     {repo} @ {}", t.base.as_deref().unwrap_or("main"));
    }
    let gates = crate::gate::pending_for_task(&t.id);
    if !gates.is_empty() {
        println!("gates    {} pending: {}", gates.len(), gates.iter().map(|g| g.id.as_str()).collect::<Vec<_>>().join(", "));
    }
    if !t.context.is_null() {
        println!("context  {}", serde_json::to_string_pretty(&t.context)?);
    }
    println!("\nprompt:\n{}", t.prompt);
    Ok(())
}

fn add(args: &[String]) -> Result<(), BErr> {
    let mut worker = String::new();
    let mut ceiling = 30.0;
    let mut proactive = false;
    let mut repo: Option<String> = None;
    let mut base = "main".to_string();
    let mut rest: Vec<String> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--worker" => {
                worker = args.get(i + 1).cloned().ok_or("--worker wants a name")?;
                i += 2;
            }
            "--ceiling" => {
                ceiling = args.get(i + 1).and_then(|s| s.parse().ok()).ok_or("--ceiling wants a number of minutes")?;
                i += 2;
            }
            "--repo" => {
                // Absolute path so the worktree resolves from the supervisor's cwd.
                let p = args.get(i + 1).cloned().ok_or("--repo wants a git repo path")?;
                repo = Some(std::fs::canonicalize(&p).map(|c| c.display().to_string()).unwrap_or(p));
                i += 2;
            }
            "--base" => {
                base = args.get(i + 1).cloned().ok_or("--base wants a ref")?;
                i += 2;
            }
            "--proactive" => {
                proactive = true;
                i += 1;
            }
            _ => {
                rest.push(args[i].clone());
                i += 1;
            }
        }
    }
    if worker.is_empty() {
        return Err("queue add needs --worker <name>".into());
    }
    let prompt = rest.join(" ");
    if prompt.trim().is_empty() {
        return Err("queue add needs a prompt".into());
    }
    let base = repo.as_ref().map(|_| base);
    let kind = if repo.is_some() { "code" } else if proactive { "proactive" } else { "owner-filed" };
    let t = queue::create(&worker, &prompt, "manual", proactive, ceiling, serde_json::Value::Null, repo, base)
        .map_err(|e| e.to_string())?;
    println!("queued {} for {} ({kind})", t.id, t.worker);
    Ok(())
}

fn ls() -> Result<(), BErr> {
    let tasks = queue::list_all();
    if tasks.is_empty() {
        println!("queue is empty");
        return Ok(());
    }
    println!("{:<12}  {:<10}  {:<12}  {:<12}  {}", "TASK", "WORKER", "STATE", "ORIGIN", "PROMPT");
    for t in tasks {
        let prompt = if t.prompt.len() > 48 { format!("{}…", &t.prompt[..48]) } else { t.prompt.clone() };
        println!("{:<12}  {:<10}  {:<12}  {:<12}  {}", t.id, t.worker, t.state, t.origin, prompt);
    }
    Ok(())
}
