//! `roster worker chat <name>` — an interactive warm session: run one rpc box
//! and feed it messages from stdin, one per line. For working with (and
//! testing) the multi-message session without a channel in front.

use super::BErr;
use crate::cmd::run_box;

pub async fn chat(worker: &str, idle: u64) -> Result<(), BErr> {
    super::require_worker(worker)?;

    let (tx, rx) = tokio::sync::mpsc::channel::<run_box::SessionMessage>(32);
    let reader = tokio::spawn(async move {
        use tokio::io::AsyncBufReadExt;
        let mut lines = tokio::io::BufReader::new(tokio::io::stdin()).lines();
        while let Ok(Some(l)) = lines.next_line().await {
            if l.trim().is_empty() {
                continue;
            }
            let msg = run_box::SessionMessage {
                text: l,
                author_label: "stdin".into(),
                context: crate::memory::RunContext::default(),
            };
            if tx.send(msg).await.is_err() {
                break;
            }
        }
        // stdin EOF → drop tx → the session drains and idles out.
    });

    let run_id = run_box::new_run_id();
    run_box::run_session(
        worker,
        &run_id,
        crate::context::RunSurface::DirectBox,
        crate::memory::RunContext::default(),
        rx,
        idle,
    )
    .await?;
    reader.abort();
    println!("session ended — transcript: state/runs/{run_id}/stdout.jsonl");
    Ok(())
}
