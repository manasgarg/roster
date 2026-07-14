//! `roster worker chat <name>` — an interactive warm session: run one rpc box
//! and feed it messages from stdin, one per line. For working with (and
//! testing) the multi-message session without a channel in front.

use crate::run::boxed;
use crate::util::BErr;

pub async fn chat(worker: &str, idle: u64) -> Result<(), BErr> {
    crate::worker::require_worker(worker)?;

    let (tx, rx) = tokio::sync::mpsc::channel::<boxed::SessionMessage>(32);
    let reader = tokio::spawn(async move {
        use tokio::io::AsyncBufReadExt;
        let mut lines = tokio::io::BufReader::new(tokio::io::stdin()).lines();
        while let Ok(Some(l)) = lines.next_line().await {
            if l.trim().is_empty() {
                continue;
            }
            let msg = boxed::SessionMessage {
                text: l,
                author_label: "stdin".into(),
                context: crate::worker::memory::RunContext::default(),
            };
            if tx.send(msg).await.is_err() {
                break;
            }
        }
        // stdin EOF → drop tx → the session drains and idles out.
    });

    let run_id = boxed::new_run_id();
    boxed::run_session(
        worker,
        &run_id,
        crate::worker::context::RunSurface::DirectBox,
        crate::worker::memory::RunContext::default(),
        rx,
        idle,
    )
    .await?;
    reader.abort();
    println!("session ended — transcript: state/runs/{run_id}/stdout.jsonl");
    Ok(())
}
