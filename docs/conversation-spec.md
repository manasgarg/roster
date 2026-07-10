# Conversation behavior — response modes & box sessions (spec)

**Status: spec — not yet implemented.** Refines the Discord inbound experience so
conversations feel natural and stay affordable: the worker responds when it
should, an admin tunes how eager it is per channel, and a burst of messages
reuses one warm box instead of a cold run each. Governance is unchanged —
messages stay content-never-commands and every action stays gated.

## 1. Per-channel response mode

A channel carries a **response mode**, set by an admin (alongside its trust
designation):

- **`all`** (default) — every message wakes the worker.
- **`mention`** — only an @mention (or a DM) wakes it. For busy/general channels
  where the worker should speak only when addressed.

Set via `/channel mode all|mention` (admin) or `roster channel mode <id> <mode>`.

**Wake rule:** wake the worker iff it's a **DM**, or it was **@mentioned**, or the
channel mode is **`all`**. (In `mention` mode, ambient messages are still
persisted to history — they just don't spawn work.)

## 2 & 3. Respond vs judge (1:1 vs group)

Being woken isn't the same as replying. Once running, the framing depends on how
many people are in the conversation:

- **DM, or a channel with ≤1 human besides the worker** → effectively 1:1:
  **respond** (it's a direct back-and-forth).
- **Multiple humans** → the worker **judges** whether to participate — reply only
  when addressed or genuinely useful; **staying silent is a clean outcome**.

Participant count is inferred from the distinct human authors in recent history
(no extra Discord intents needed). So a channel set to `all` with just you and the
worker feels like a DM; the same channel with a crowd makes the worker judicious.

## 4. Box sessions — keep the box warm

Today each message is a **cold box run** (docker + pi + model context from
scratch). In an `all`-mode or active channel, that's slow and wasteful when
messages arrive in bursts. Instead:

- A worker keeps **one live box per active channel** for a short **idle window**
  (~60s) after it responds.
- A new message for that channel is **delivered to the live box** (through a
  mailbox on its stdin) rather than spawning a new one — same warm pi session, no
  docker or model cold-start.
- After the idle window with no new message, the box **exits**.

**Mechanism.** pi has `--mode rpc` — JSON-RPC over stdin/stdout for multi-turn
interaction. A per-session runner owns the pi child: it feeds each delivered
message as an rpc request and reads events (a `discord_send` tool call still
routes through the gateway, governed, exactly as now). This is the truest form of
"stays alive." Fallbacks, in order of decreasing warmth, if the rpc protocol is
heavier than expected:

1. **RPC live box** (recommended) — one warm pi process per session.
2. **Warm-container loop** — keep the container alive; re-run `pi --continue`
   inside it per message (skips docker cold-start; pi restarts, session resumes).
3. **Session-resume only** — cold box per message, but `--session-id`/`--continue`
   preserves conversation context. Simplest; keeps nothing warm.

## How they fit

`all` mode makes the worker a participant; the **live box** is what makes that
affordable — a burst of messages is handled by one warm box that judges and
replies, then idles out. The two are meant to ship together.

## Governance & lifecycle (unchanged where it matters)

- A session box has the same **lockdown, identity token, and read-only mounts**;
  delivering a message to it is delivering **content**, never a command. Every
  action it takes (reply, purpose edit) is judged/gated as today.
- A session is bounded by the **idle window** and the run **ceiling**; budgets
  still apply, and the hard-stop still cuts model credentials on an empty ledger.
- **Conversation routes to sessions; other work stays task/queue-based.** A chat
  message goes to the live session (or starts one); scheduled/proactive/code work
  and continuations remain one-shot tasks via `supervise`. The two coexist — the
  session manager is keyed by (worker, channel).

## Build order

1. **Response modes + 1:1/group framing** (points 1–3). A channel `mode` setting
   (store + `/channel mode` + `roster channel mode`), the wake rule, and the
   respond-vs-judge framing from the participant count. No box change — testable
   immediately.
2. **Box sessions** (point 4). The session manager (per worker+channel): route a
   message to a live box or start one; the pi `--mode rpc` runner + mailbox; the
   idle timeout. The larger change; settle the mechanism (RPC vs warm-container)
   at the start.

## Open decisions (recommended defaults)

- **Default mode `all`** (the worker participates; `mention` is the restriction) —
  per the "respond unless restricted" intent. It costs more runs; the live box
  mitigates, and a busy channel can be set to `mention`.
- **Idle window ~60s**; **ceiling** still caps a session's total life.
- **Session mechanism**: RPC live box first; fall back to the warm-container loop
  if pi's rpc protocol is more than a thin request/event stream.
