//! `roster worker chat <name>` — an interactive warm session: run one rpc box
//! and feed it messages from stdin, one per line. For working with (and
//! testing) the multi-message session without a channel in front.

use crate::run::boxed;
use crate::util::BErr;
use rustyline::ExternalPrinter as _;

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
    println!(
        "session ended — transcript: {}",
        crate::paths::run_dir(&run_id)
            .join("stdout.jsonl")
            .display()
    );
    Ok(())
}

/// Talk needs the daemon (gateway, dispatch). When it isn't running, offer to
/// start it — always asking first, never silently — then wait for the port to
/// answer. Non-interactive callers just get the hint.
async fn ensure_server(interactive: bool) -> Result<(), BErr> {
    if crate::cli::server::gateway_up().await {
        return Ok(());
    }
    if !interactive {
        return Err("the server isn't running — start it: roster server start".into());
    }
    eprint!("the server isn't running — start it now? [y/N] ");
    use std::io::Write;
    let _ = std::io::stderr().flush();
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    if !matches!(answer.trim(), "y" | "Y" | "yes") {
        return Err("talk needs the server — start it: roster server start".into());
    }

    let log_path = crate::paths::state_root().join("server.log");
    std::fs::create_dir_all(crate::paths::state_root())?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)?;
    // Its own process group: Ctrl-C in talk must not take the daemon down.
    use std::os::unix::process::CommandExt;
    std::process::Command::new(std::env::current_exe()?)
        .args(["server", "start"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::from(log.try_clone()?))
        .stderr(std::process::Stdio::from(log))
        .process_group(0)
        .spawn()?;
    eprintln!("starting server (log: {}) …", log_path.display());
    for _ in 0..40 {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        if crate::cli::server::gateway_up().await {
            return Ok(());
        }
    }
    Err(format!("the server did not come up — check {}", log_path.display()).into())
}

/// Readline glue for slash-command tab completion: the grammar and live ids
/// come from channel::slash::complete; a unique match gets a trailing space
/// so the operator can keep typing. Everything else (hints, highlighting,
/// validation) stays default.
struct SlashHelper {
    worker: String,
}

impl rustyline::completion::Completer for SlashHelper {
    type Candidate = rustyline::completion::Pair;
    fn complete(
        &self,
        line: &str,
        pos: usize,
        _ctx: &rustyline::Context<'_>,
    ) -> rustyline::Result<(usize, Vec<Self::Candidate>)> {
        let (from, candidates) = crate::channel::slash::complete(line, pos, &self.worker);
        let space = if candidates.len() == 1 { " " } else { "" };
        Ok((
            from,
            candidates
                .into_iter()
                .map(|c| rustyline::completion::Pair {
                    replacement: format!("{c}{space}"),
                    display: c,
                })
                .collect(),
        ))
    }
}
impl rustyline::hint::Hinter for SlashHelper {
    type Hint = String;
}
impl rustyline::highlight::Highlighter for SlashHelper {}
impl rustyline::validate::Validator for SlashHelper {}
impl rustyline::Helper for SlashHelper {}

/// Worker deliveries the operator hasn't seen: strictly newer than the seen
/// marker. Before any marker exists (first run after upgrade), fall back to
/// the old heuristic — everything after the operator's last own message.
fn missed_deliveries<'a>(
    history: &'a [serde_json::Value],
    seen: Option<&str>,
) -> Vec<&'a serde_json::Value> {
    let is_worker =
        |m: &serde_json::Value| m.get("role").and_then(|v| v.as_str()) == Some("worker");
    let Some(seen) = seen.and_then(parse_ts) else {
        let last_human = history
            .iter()
            .rposition(|m| m.get("role").and_then(|v| v.as_str()) == Some("host-op"));
        return history
            .iter()
            .skip(last_human.map(|i| i + 1).unwrap_or(0))
            .filter(|m| is_worker(m))
            .collect();
    };
    history
        .iter()
        .filter(|m| {
            is_worker(m)
                && m.get("ts")
                    .and_then(|v| v.as_str())
                    .and_then(parse_ts)
                    .is_some_and(|ts| ts > seen)
        })
        .collect()
}

/// Rfc3339 fractional seconds vary in length, so compare parsed instants,
/// never strings.
fn parse_ts(value: &str) -> Option<time::OffsetDateTime> {
    time::OffsetDateTime::parse(value, &time::format_description::well_known::Rfc3339).ok()
}

fn seen_marker(channel_id: &str) -> Option<String> {
    std::fs::read_to_string(crate::paths::talk_seen_file(channel_id))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// The terminal displayed everything up to now — advance the replay cursor.
fn mark_seen(channel_id: &str) {
    let path = crate::paths::talk_seen_file(channel_id);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&path, crate::util::now_rfc3339());
}

/// The terminal line discipline, snapshotted before the editor thread starts:
/// that thread may sit inside readline's raw mode when the session ends
/// elsewhere (idle timeout), and exiting through it would leave the shell raw.
fn stdin_termios() -> Option<libc::termios> {
    unsafe {
        let mut t: libc::termios = std::mem::zeroed();
        (libc::tcgetattr(libc::STDIN_FILENO, &mut t) == 0).then_some(t)
    }
}

/// Drain a channel of display text into a writer — the writer differs by
/// surface (readline's above-prompt printer, or plain stdout when piped).
fn spawn_sink<F: FnMut(String) + Send + 'static>(
    mut rx: tokio::sync::mpsc::Receiver<String>,
    mut write: F,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(text) = rx.recv().await {
            write(text);
        }
    })
}

/// One warm session behind the terminal conversation — the Discord model: a
/// message wakes a session, quiet winds it down, and the next message wakes
/// another. The watcher note tells the operator the session ended without
/// ending the conversation.
fn spawn_session(
    worker: &str,
    idle: u64,
    context: crate::worker::memory::RunContext,
    reply_tx: tokio::sync::mpsc::Sender<String>,
    info_tx: tokio::sync::mpsc::Sender<String>,
) -> (
    tokio::sync::mpsc::Sender<boxed::SessionMessage>,
    tokio::task::JoinHandle<()>,
) {
    let (tx, rx) = tokio::sync::mpsc::channel::<boxed::SessionMessage>(32);
    let run_id = boxed::new_run_id();
    let worker = worker.to_string();
    let handle = tokio::spawn(async move {
        let note = match boxed::run_session(
            &worker,
            &run_id,
            crate::worker::context::RunSurface::TermSession,
            context,
            rx,
            idle,
            Some(reply_tx),
        )
        .await
        {
            Ok(()) => format!("(session {run_id} wound down — the conversation stays open)"),
            Err(e) => format!("(session {run_id} failed: {e})"),
        };
        let _ = info_tx.send(note).await;
    });
    (tx, handle)
}

/// `roster talk <worker>` — the terminal as a first-class channel. One
/// long-running conversation with the Discord interaction model behind it:
/// a durable channel id, recorded history, a purpose, channel/user memory
/// scopes, DM-grade trust (it is the operator's own shell), and warm
/// sessions that wake on a message and wind down when quiet — without ever
/// closing the terminal. The conversation scrolls above a readline prompt
/// pinned at the bottom (arrow keys, history); replies print above whatever
/// the operator is mid-typing. `/…` lines are the same slash commands as
/// the chat channels (channel::slash), answered by the host, never sent to
/// the model.
pub async fn talk(worker: &str, idle: u64) -> Result<(), BErr> {
    crate::worker::require_worker(worker)?;
    let interactive = unsafe { libc::isatty(libc::STDIN_FILENO) } == 1;
    ensure_server(interactive).await?;
    let user = std::env::var("USER").unwrap_or_else(|_| "operator".into());
    let channel_id = format!("term-{user}-{worker}");
    // The operator's own shell is trusted like a DM (1:1, sought-out).
    if let Err(e) = crate::channel::discord::set_channel_trust(&channel_id, true) {
        eprintln!("talk: could not mark {channel_id} trusted: {e}");
    }

    let context = crate::worker::memory::RunContext {
        provider: "term".into(),
        channel_id: Some(channel_id.clone()),
        user_id: Some(user.clone()),
        message_id: None,
        thread_ts: None,
        role: "host-op".into(),
        is_dm: true,
        inbound: false,
    };

    let (reply_tx, reply_rx) = tokio::sync::mpsc::channel::<String>(32);

    eprintln!(
        "talking to {worker} — terminal channel {channel_id} (trusted); quiet sessions wind down and wake on your next message; /help for commands, Ctrl-D leaves"
    );

    // Task runs deliver results to this channel while nobody is watching
    // (term_send). Surface whatever arrived since the terminal last displayed
    // this channel — a durable cursor, so a report read but not replied to
    // doesn't replay on every open.
    let history = crate::channel::discord::recent_messages(&channel_id, 200);
    let missed = missed_deliveries(&history, seen_marker(&channel_id).as_deref());
    if !missed.is_empty() {
        println!("while you were away:");
        for m in missed {
            println!(
                "{worker}> {}\n",
                m.get("content").and_then(|v| v.as_str()).unwrap_or("")
            );
        }
    }
    mark_seen(&channel_id);

    let saved_termios = stdin_termios();
    let (line_tx, mut line_rx) = tokio::sync::mpsc::channel::<String>(32);
    let (info_tx, info_rx) = tokio::sync::mpsc::channel::<String>(32);

    let (reply_sink, info_sink) = if interactive {
        let mut rl =
            rustyline::Editor::<SlashHelper, rustyline::history::DefaultHistory>::with_config(
                rustyline::Config::builder()
                    .completion_type(rustyline::CompletionType::List)
                    .build(),
            )?;
        rl.set_helper(Some(SlashHelper {
            worker: worker.to_string(),
        }));
        let _ = rl.load_history(&crate::paths::talk_history_file());
        let mut reply_printer = rl.create_external_printer()?;
        let mut info_printer = rl.create_external_printer()?;

        // The editor owns stdin on its own thread (readline blocks);
        // submitted lines flow to the async handler below. Ctrl-D/Ctrl-C
        // drop line_tx, which drains the handler, drops tx, and lets the
        // session end. Not joined: it may still be blocked in readline
        // when the session ends elsewhere.
        std::thread::spawn(move || {
            while let Ok(line) = rl.readline("you> ") {
                if line.trim().is_empty() {
                    continue;
                }
                let _ = rl.add_history_entry(line.as_str());
                if line_tx.blocking_send(line).is_err() {
                    break;
                }
            }
            let _ = rl.save_history(&crate::paths::talk_history_file());
        });

        // Output inserts above the prompt; readline redraws half-typed input.
        let worker_name = worker.to_string();
        (
            spawn_sink(reply_rx, move |text| {
                let _ = reply_printer.print(format!("{worker_name}> {text}\n"));
            }),
            spawn_sink(info_rx, move |text| {
                let _ = info_printer.print(format!("{text}\n"));
            }),
        )
    } else {
        // Piped stdin (scripts): plain line IO, no editor, no prompt.
        tokio::spawn(async move {
            use tokio::io::AsyncBufReadExt;
            let mut lines = tokio::io::BufReader::new(tokio::io::stdin()).lines();
            while let Ok(Some(l)) = lines.next_line().await {
                if l.trim().is_empty() {
                    continue;
                }
                if line_tx.send(l).await.is_err() {
                    break;
                }
            }
        });
        let worker_name = worker.to_string();
        (
            spawn_sink(reply_rx, move |text| println!("{worker_name}> {text}")),
            spawn_sink(info_rx, |text| println!("{text}")),
        )
    };

    // Live deliveries: task runs term_send into this channel's history while
    // the conversation is open. Tail the file so results surface as they
    // land, not just at the next session open. (Worker-role entries come
    // only from term_send — live session replies are never persisted — so
    // nothing prints twice.)
    let delivery_watch = {
        let channel_id = channel_id.clone();
        let reply_tx = reply_tx.clone();
        tokio::spawn(async move {
            let path = crate::paths::channel_dir(&channel_id).join("messages.jsonl");
            let mut seen = std::fs::read_to_string(&path)
                .map(|s| s.lines().count())
                .unwrap_or(0);
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                let Ok(text) = std::fs::read_to_string(&path) else {
                    continue;
                };
                let lines: Vec<&str> = text.lines().collect();
                if lines.len() < seen {
                    seen = lines.len(); // rotated/truncated: adopt
                    continue;
                }
                let mut delivered = false;
                for l in &lines[seen..] {
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(l) {
                        if v.get("role").and_then(|r| r.as_str()) == Some("worker") {
                            if let Some(content) = v.get("content").and_then(|c| c.as_str()) {
                                let _ = reply_tx.send(content.to_string()).await;
                                delivered = true;
                            }
                        }
                    }
                }
                if delivered {
                    // Displayed live — advance the replay cursor so the next
                    // `roster talk` doesn't show it again.
                    mark_seen(&channel_id);
                }
                seen = lines.len();
            }
        })
    };

    // One conversation, many sessions. A message routes to the live session
    // if there is one; otherwise it wakes a fresh one — exactly how the
    // Discord listener treats a channel. Session death never ends this loop;
    // only the operator leaving (Ctrl-D) does.
    let mut live: Option<(
        tokio::sync::mpsc::Sender<boxed::SessionMessage>,
        tokio::task::JoinHandle<()>,
    )> = None;

    while let Some(l) = line_rx.recv().await {
        // Slash commands are operator↔host — answered here, never
        // persisted, never sent to the model.
        if l.trim_start().starts_with('/') {
            let reply = match crate::channel::slash::parse(l.trim()) {
                Ok(call) => {
                    crate::channel::slash::run(
                        worker,
                        &call,
                        &channel_id,
                        &context,
                        "host-op",
                        &format!("term:{user}"),
                    )
                    .await
                }
                Err(e) => e,
            };
            let _ = info_tx.send(reply).await;
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
        let mut msg = Some(boxed::SessionMessage {
            text: l,
            author_label: user.clone(),
            context: context.clone(),
        });
        if let Some((tx, _)) = live.as_ref() {
            if let Err(back) = tx.send(msg.take().unwrap()).await {
                // The session wound down (its watcher said so); the message
                // comes back out of the failed send and wakes the next one.
                msg = Some(back.0);
                live = None;
            }
        }
        if let Some(m) = msg {
            let _ = info_tx.send(format!("(waking {worker}…)")).await;
            let (tx, handle) = spawn_session(
                worker,
                idle,
                context.clone(),
                reply_tx.clone(),
                info_tx.clone(),
            );
            if tx.send(m).await.is_err() {
                let _ = info_tx
                    .send("(message not delivered — the session failed to start; try again)".into())
                    .await;
            }
            live = Some((tx, handle));
        }
    }

    // Ctrl-D / EOF: let a live session finish its turns and wind down.
    if let Some((tx, handle)) = live.take() {
        drop(tx);
        let _ = handle.await;
    }
    delivery_watch.abort();
    drop(reply_tx);
    drop(info_tx);
    let _ = reply_sink.await;
    let _ = info_sink.await;
    // The editor thread may still hold readline's raw mode; hand the shell
    // back its line discipline before leaving through it.
    if let Some(t) = saved_termios {
        unsafe {
            libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &t);
        }
    }
    eprintln!("\nleft the conversation — resume: roster talk {worker}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn msg(role: &str, ts: &str, content: &str) -> serde_json::Value {
        json!({ "role": role, "ts": ts, "content": content })
    }

    #[test]
    fn replay_shows_only_deliveries_newer_than_the_cursor() {
        let history = vec![
            msg("host-op", "2026-07-17T04:00:00Z", "do the thing"),
            msg("worker", "2026-07-17T04:10:00.5Z", "report A"),
            msg("worker", "2026-07-17T04:12:00Z", "report B"),
        ];
        // Cursor after A (shorter fractional form on purpose): only B replays.
        let missed = missed_deliveries(&history, Some("2026-07-17T04:11:00.25Z"));
        assert_eq!(missed.len(), 1);
        assert_eq!(missed[0]["content"], "report B");
        // Cursor after everything: a read-but-unanswered report never replays.
        assert!(missed_deliveries(&history, Some("2026-07-17T04:13:00Z")).is_empty());
        // No cursor yet (first run after upgrade): the old last-host-op heuristic.
        let missed = missed_deliveries(&history, None);
        assert_eq!(missed.len(), 2);
    }
}
