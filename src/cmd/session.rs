//! `roster session --worker <name>` — dev harness for the rpc session box: run
//! one warm box and feed it messages from stdin, one per line. For local testing
//! of the multi-message session before it's wired to Discord.

use crate::cmd::run_box;

type BErr = Box<dyn std::error::Error>;

pub async fn run(args: &[String]) -> Result<(), BErr> {
    let mut worker = String::new();
    let mut idle = 20u64;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--worker" => {
                worker = args.get(i + 1).cloned().ok_or("--worker wants a name")?;
                i += 2;
            }
            "--idle" => {
                idle = args.get(i + 1).and_then(|s| s.parse().ok()).ok_or("--idle wants seconds")?;
                i += 2;
            }
            other => return Err(format!("unknown session flag \"{other}\"").into()),
        }
    }
    if worker.is_empty() {
        return Err("session needs --worker <name>".into());
    }

    let (tx, rx) = tokio::sync::mpsc::channel::<run_box::SessionMessage>(32);
    let reader = tokio::spawn(async move {
        use tokio::io::AsyncBufReadExt;
        let mut lines = tokio::io::BufReader::new(tokio::io::stdin()).lines();
        while let Ok(Some(l)) = lines.next_line().await {
            if l.trim().is_empty() {
                continue;
            }
            let msg = run_box::SessionMessage { text: l, context: crate::memory::RunContext::default() };
            if tx.send(msg).await.is_err() {
                break;
            }
        }
        // stdin EOF → drop tx → the session drains and idles out.
    });

    let run_id = run_box::new_run_id();
    let system = "You are a concise assistant in a test session. Answer each message briefly.";
    run_box::run_session(&worker, &run_id, system, rx, idle).await?;
    reader.abort();
    println!("session ended — transcript: runs/{run_id}/stdout.jsonl");
    Ok(())
}
