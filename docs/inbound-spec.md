# Inbound transport — Discord channels (spec)

**Status: spec — not yet implemented.** Realizes handoff §3.9 (channels; Discord
obeys only the owner id) and D12 (channels relay, never act; inbound is content,
never commands). Inbound is **Discord only**; email stays **outbound-only** (SMTP,
already built) — no inbound email yet. Builds on the `roster relay` hand-off and
the governed action path.

## Goal (concrete)

People talk to the worker in Discord channels. The worker knows **which channel**
it's in and **who is speaking**, has the **full history and any uploaded files**
available, and decides **whether and how to respond** — silence is a valid
outcome. The owner also drives Roster admin (`queue`, `gates`) via **slash
commands**. Every reply is a governed action; every inbound message is content,
never a command.

## Why Discord-only inbound

Discord's bot **dials out** to the gateway, so inbound needs **no public
endpoint** — and the same connection delivers messages *and* slash-command
interactions. Inbound email would need a public webhook endpoint and is
explicitly out of scope for now. Email remains send-only (`connect smtp`).

## The trust rule (D12), for a shared channel

**Inbound is content, never a command.** A message can start or steer work, but
its text is never obeyed as an instruction to the governance layer — the worker
acts only through gated actions. So a hostile message, or an injection inside
one, can grab attention but can never lift a gate, grant a capability, or edit
identity/purpose.

Discord **authenticates every sender** (a user id). That distinction sets only
the **prompt framing**, never enforcement:

- Messages from an **owner id** are framed as the owner's **steer** (direction to
  weigh heavily).
- Everyone else is **content** — information the worker may consider, never a
  command.

In a multi-person channel the worker sees each message's author and the owner-vs-
other distinction, and weighs accordingly — but no sender bypasses the gates.

## Multi-channel, multi-person awareness

One worker watches **many channels**; each channel has **many participants**.
Every inbound message the worker sees carries: channel id + name, author id +
display name, whether the author is an owner, timestamp, text, and attachment
refs. The worker's context makes all of this explicit, so it always knows *where*
it is and *who* it's talking to.

## Waking the worker, and the judgement to respond

Two independent decisions keep a busy channel from spawning endless runs or noise:

1. **When to wake (transport, cheap):** not every message spawns a run. The
   transport wakes the worker on a trigger — a direct DM, an @mention, an owner
   steer, or a short debounce window — and batches the recent messages into one
   task. Idle chatter it isn't addressed in doesn't wake it.
2. **Whether to respond (worker, judgement):** once running, the worker decides
   whether saying or doing anything is warranted. In a group channel, **staying
   silent is a valid outcome** (a run that proposes no action). A DM usually
   warrants a reply; a busy channel often doesn't unless the worker is addressed
   or has something genuinely useful.

## History & uploads on the filesystem

Context stays bounded, but nothing is lost:

- The transport persists each channel's **full message history** to disk
  (`channels/<channel-id>/messages.jsonl`) and downloads **attachments**
  (`channels/<channel-id>/files/…`).
- On a run, the box gets the **last N messages** in context (default ~30), and can
  **read older history and any uploaded file** from the channel store, mounted
  read-only. So the model sees recent conversation upfront and pulls in more —
  older messages, an uploaded document — from the filesystem when it needs to.

## Identity & purpose (supersedes the single charter.md)

The charter splits in two, because one worker now serves several channels:

- **`identity.md`** — the worker's **fixed self**: who it is, its persona, its
  standing rules. The **same across every channel**. (`workers/<name>/identity.md`)
- **`purpose.md`** — **channel-specific** role and goals: what the worker is *for*
  in this channel. (one per channel, mapped by channel config)

A run's context = **identity + the active channel's purpose + briefing + recent
messages/task**. Both files are owner-authored and edited under the D10 gate
(`identity-edit` / `purpose-edit`, hard-gated, like `charter-edit` today). This
replaces the single `charter.md`; see the note added to `charter-spec.md`.

## Replies (outbound, governed)

The worker replies through a governed action — `discord_send(channel_id, text)`
(or reply-in-thread) — whose trusted-side executor holds the **bot token from the
vault**; the box never holds it. `message_user` routes to the owner's DM when
configured. Replies are governed like any action: metered, and gated per the
trust ladder (e.g. a reply in the owner's DM might be auto; posting in a public
channel might gate at first and earn trust).

## Slash commands (owner admin)

The owner drives Roster admin from Discord via **slash commands**, delivered over
the same gateway connection (no public endpoint):

- **Owner-only** — checked against the owner id on the interaction.
- **Safe surface only** — `/queue` (ls · add · show · requeue), `/gates`
  (ls · show · approve · deny), and status. These map to the existing `roster`
  subcommands; nothing arbitrary or destructive is exposed.
- Approving a gate from Discord is the **authenticated owner deciding** — the same
  approval desk, just a different surface, consistent with "no model at the edge."

## The pieces

1. **`roster listen`** — the trusted Discord gateway client (sibling to `serve` /
   `supervise`): dials out, receives messages + slash interactions on configured
   channels, persists history + attachments, files content tasks, and handles
   owner admin commands.
2. **Channel config** (`worker.toml` `[[channel]]`: name, `channel_id`, its
   `purpose.md`, and the owner id(s)).
3. **Channel store** (`channels/<id>/`) — history + uploads, mounted read-only
   into the box.
4. **`discord_send` action + executor** (bot REST); `message_user` → owner DM.
5. **Slash-command handler** — owner-gated, mapping to safe `roster` subcommands.
6. **`roster connect discord`** — bot token + owner id(s) → vault.

## Security invariants

- Inbound content **never commands** the governance layer; every worker action
  stays gated. Sender identity sets framing, not enforcement.
- **Discord authenticates senders**; owner steer is filtered to owner id(s).
- Bot token lives in the **vault**, never in the box; history/uploads are mounted
  **read-only**.
- Slash commands are **owner-only** and expose only safe admin subcommands;
  approving a gate is the authenticated owner deciding.
- **Bounded**: the transport rate-limits/debounces waking the worker; the worker's
  judgement + governance bound what goes out.
- **No inbound email** (spoofable, out of scope) — email is outbound-only.

## Build order (small increments)

1. **Outbound Discord** — `connect discord`, `discord_send` action + executor,
   `message_user` → owner DM. Trivial (plain HTTPS), high value, needed anyway.
2. **Gateway client (`roster listen`)** — dial out, receive on configured
   channels, persist history + attachments, wake-on-trigger + batch, file content
   tasks with full channel/speaker awareness and last-N context.
3. **Identity/purpose split** — `identity.md` + per-channel `purpose.md`, composed
   into the run; `identity-edit`/`purpose-edit` hard-gated actions.
4. **Judgement-to-respond** — the run framing that makes silence a first-class
   outcome and weighs owner-vs-other in a multi-person channel.
5. **Slash commands** — owner admin (`queue`, `gates`) over the gateway.

## Open decisions (recommended defaults)

- **History window N** — start ~30 recent messages upfront, the rest on disk.
- **Owner ids are a set**, configurable per channel or worker-wide (some channels
  have several trusted people).
- **React, don't broadcast (v1)** — the worker responds to activity; unprompted
  proactive chatter in a channel is a later feature (scheduled triggers already
  cover time-based proactivity).
- **identity/purpose replaces `charter.md`** — a rename/split of the shipped
  charter feature; migrate `workers/<name>/charter.md` → `identity.md` and add
  per-channel `purpose.md` when this lands.
