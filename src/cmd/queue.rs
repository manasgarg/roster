//! `roster queue` — file and list tasks for the supervisor to dispatch.
//!
//!   roster queue add --worker <name> [--ceiling <m>] [--proactive] "<prompt>"
//!   roster queue ls

use crate::queue;

type BErr = Box<dyn std::error::Error>;

pub fn run(args: &[String]) -> Result<(), BErr> {
    match args.first().map(String::as_str).unwrap_or("ls") {
        "add" => add(&args[1..]),
        "ls" | "list" => ls(),
        other => Err(format!("unknown queue subcommand \"{other}\" (try: add, ls)").into()),
    }
}

fn add(args: &[String]) -> Result<(), BErr> {
    let mut worker = String::new();
    let mut ceiling = 30.0;
    let mut proactive = false;
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
    let t = queue::create(&worker, &prompt, "manual", proactive, ceiling, serde_json::Value::Null)
        .map_err(|e| e.to_string())?;
    println!("queued {} for {} ({})", t.id, t.worker, if proactive { "proactive" } else { "owner-filed" });
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
