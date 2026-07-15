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
        None,
    )
    .await?;
    reader.abort();
    println!("session ended — transcript: state/runs/{run_id}/stdout.jsonl");
    Ok(())
}

/// `roster talk <worker>` — the terminal as a first-class channel. Same warm
/// session as chat, but with the full channel model: a durable channel id,
/// recorded history, a purpose, channel/user memory scopes, and DM-grade
/// trust (it is the operator's own shell). Replies print straight to the
/// terminal — the session scope tells the model its message text IS the reply.
pub async fn talk(worker: &str, idle: u64) -> Result<(), BErr> {
    crate::worker::require_worker(worker)?;
    let user = std::env::var("USER").unwrap_or_else(|_| "operator".into());
    let channel_id = format!("term-{user}-{worker}");
    // The operator's own shell is trusted like a DM (1:1, sought-out).
    crate::channel::discord::set_channel_trust(&channel_id, true);

    let context = crate::worker::memory::RunContext {
        provider: "term".into(),
        channel_id: Some(channel_id.clone()),
        user_id: Some(user.clone()),
        message_id: None,
        role: "host-op".into(),
        is_dm: true,
        inbound: false,
    };

    let (tx, rx) = tokio::sync::mpsc::channel::<boxed::SessionMessage>(32);
    let (reply_tx, mut reply_rx) = tokio::sync::mpsc::channel::<String>(32);

    let printer = {
        let worker = worker.to_string();
        tokio::spawn(async move {
            while let Some(text) = reply_rx.recv().await {
                println!("\n{worker}> {text}\n");
            }
        })
    };

    let reader = {
        let context = context.clone();
        let channel_id = channel_id.clone();
        let user = user.clone();
        tokio::spawn(async move {
            use tokio::io::AsyncBufReadExt;
            let mut lines = tokio::io::BufReader::new(tokio::io::stdin()).lines();
            while let Ok(Some(l)) = lines.next_line().await {
                if l.trim().is_empty() {
                    continue;
                }
                // Only the human side is persisted, like the other channels
                // (continuity inside a session comes from the session itself).
                crate::channel::discord::persist_message(
                    &channel_id,
                    &serde_json::json!({
                        "ts": crate::util::now_rfc3339(),
                        "author_id": user, "author": user, "role": "host-op",
                        "content": l, "attachments": [],
                    }),
                );
                let msg = boxed::SessionMessage {
                    text: l,
                    author_label: user.clone(),
                    context: context.clone(),
                };
                if tx.send(msg).await.is_err() {
                    break;
                }
            }
            // stdin EOF -> drop tx -> the session finishes its turns and ends.
        })
    };

    eprintln!("talking to {worker} — terminal channel {channel_id} (trusted); Ctrl-D ends");
    let run_id = boxed::new_run_id();
    boxed::run_session(
        worker,
        &run_id,
        crate::worker::context::RunSurface::TermSession,
        context,
        rx,
        idle,
        Some(reply_tx),
    )
    .await?;
    reader.abort();
    let _ = printer.await;
    Ok(())
}

