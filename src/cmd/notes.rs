//! `roster memory` — owner/admin inspection and repair of scoped memory.

use crate::memory::{self, MemoryScope};

type BErr = Box<dyn std::error::Error>;

pub fn run(args: &[String]) -> Result<(), BErr> {
    let command = args.first().map(String::as_str).unwrap_or("ls");
    match command {
        "ls" | "list" => ls(&args[1..]),
        "show" => show(&args[1..]),
        "rm" | "forget" => mutate("forget", &args[1..], None),
        "disable" => mutate("disable", &args[1..], None),
        "enable" => mutate("enable", &args[1..], None),
        "pin" => mutate("pin", &args[1..], None),
        "unpin" => mutate("unpin", &args[1..], None),
        "correct" => correct(&args[1..]),
        "compact" => compact(&args[1..]),
        "explain" => explain(&args[1..]),
        other => Err(format!(
            "unknown memory subcommand \"{other}\" (try: ls, show, rm, correct, disable, enable, pin, unpin, compact, explain)"
        )
        .into()),
    }
}

fn compact(args: &[String]) -> Result<(), BErr> {
    let worker = worker_arg(args)?;
    let kept = memory::compact(&worker).map_err(|e| -> BErr { e.into() })?;
    println!("compacted {worker} memory; kept {kept} live notes");
    Ok(())
}

fn worker_arg(args: &[String]) -> Result<String, BErr> {
    let pos = args
        .iter()
        .position(|a| a == "--worker")
        .ok_or("memory command needs --worker <name>")?;
    args.get(pos + 1)
        .cloned()
        .ok_or_else(|| "--worker wants a name".into())
}

fn positional(args: &[String]) -> Vec<&str> {
    let mut out = Vec::new();
    let mut skip = false;
    for arg in args {
        if skip {
            skip = false;
        } else if arg == "--worker" || arg == "--scope" || arg == "--scope-id" {
            skip = true;
        } else {
            out.push(arg.as_str());
        }
    }
    out
}

fn flag_value<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1))
        .map(String::as_str)
}

fn ls(args: &[String]) -> Result<(), BErr> {
    let worker = worker_arg(args)?;
    let scope = match flag_value(args, "--scope") {
        None => None,
        Some("worker") => Some(MemoryScope::Worker),
        Some("channel") => Some(MemoryScope::Channel),
        Some("user") => Some(MemoryScope::User),
        Some(other) => return Err(format!("unknown memory scope \"{other}\"").into()),
    };
    let scope_id = flag_value(args, "--scope-id");
    let notes: Vec<_> = memory::list(&worker)
        .into_iter()
        .filter(|n| scope.as_ref().map(|s| &n.scope == s).unwrap_or(true))
        .filter(|n| {
            scope_id
                .map(|id| n.scope_id.as_deref() == Some(id))
                .unwrap_or(true)
        })
        .collect();
    if notes.is_empty() {
        println!("no memories for {worker}");
        return Ok(());
    }
    println!(
        "{:<18}  {:<9}  {:<20}  {:<11}  {:<10}  NOTE",
        "ID", "SCOPE", "SUBJECT", "KIND", "STATUS"
    );
    for n in notes {
        let mut text = n.note.replace('\n', " ");
        if text.chars().count() > 70 {
            text = text.chars().take(69).collect::<String>() + "…";
        }
        println!(
            "{:<18}  {:<9}  {:<20}  {:<11}  {:<10}  {}{}",
            n.id,
            n.scope.as_str(),
            n.scope_id.as_deref().unwrap_or("-"),
            n.kind,
            n.status(),
            if n.pinned { "pinned: " } else { "" },
            text
        );
    }
    Ok(())
}

fn show(args: &[String]) -> Result<(), BErr> {
    let worker = worker_arg(args)?;
    let id = positional(args)
        .first()
        .copied()
        .ok_or("usage: roster memory show <id> --worker <name>")?;
    let note = memory::find(&worker, id).ok_or_else(|| format!("no such memory {id}"))?;
    println!("{}", serde_json::to_string_pretty(&note)?);
    Ok(())
}

fn mutate(op: &str, args: &[String], replacement: Option<&str>) -> Result<(), BErr> {
    let worker = worker_arg(args)?;
    let parts = positional(args);
    let id = parts
        .first()
        .copied()
        .ok_or_else(|| format!("usage: roster memory {op} <id> --worker <name>"))?;
    memory::admin_mutate(&worker, op, id, replacement).map_err(|e| -> BErr { e.into() })?;
    println!("memory {id} → {op}");
    Ok(())
}

fn correct(args: &[String]) -> Result<(), BErr> {
    let parts = positional(args);
    let replacement = parts.iter().skip(1).copied().collect::<Vec<_>>().join(" ");
    if parts.is_empty() || replacement.trim().is_empty() {
        return Err("usage: roster memory correct <id> --worker <name> <replacement>".into());
    }
    mutate("correct", args, Some(&replacement))
}

fn explain(args: &[String]) -> Result<(), BErr> {
    let run_id = args.first().ok_or("usage: roster memory explain <run-id>")?;
    let trace = memory::recall_trace(run_id);
    if trace.is_empty() {
        println!("no memory recall trace for run {run_id}");
    } else {
        for event in trace {
            println!("{}", serde_json::to_string_pretty(&event)?);
        }
    }
    Ok(())
}
