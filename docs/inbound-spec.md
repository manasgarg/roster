# Inbound transport — Discord channels (spec)

**Status: spec — not yet implemented.** Inbound is **Discord only**; email stays
**outbound-only** (SMTP, built). Realizes handoff §3.9 — **generalized from a
single owner id to Discord roles** — and D12 (messages are content, never
commands). Builds on the `roster relay` hand-off and the governed action path.

## Goal (concrete)

Server admins add the worker to Discord channels and administer it; it discovers
those channels and watches them. People talk to it in channels and DMs; it knows
**who** each person is (their role) and **what** each channel is for, reads the
**full history and any uploaded files**, and decides **whether to respond**.
Trusted participants approve its gates and run admin from Discord; untrusted
participants can only talk. Every reply is a governed action; every message is
content, never a command.

## Roles & authority

Authority is **derived from Discord** — which authenticates every user and their
server role — plus **admin-set channel designations**, not a hardcoded owner id.
This generalizes §3.9's single-owner model. Highest authority first:

- **Host operator** — whoever runs `serve`/`supervise`/`listen` and holds the
  shell. Ultimate authority; owns the vault, the bot, the workers. (The `roster`
  CLI is theirs.)
- **Server admin** — a Discord user with admin permission in the worker's server.
  Full worker administration, *including* editing **identity**, marking channels
  trusted/untrusted, and which channels the worker is in.
- **Trusted participant** — a non-admin in a **trusted** channel, or **anyone in a
  DM** (DMs are always trusted). Can administer everything *except identity*:
  approve/deny gates, manage the queue, edit the channel's **purpose**. And talk.
- **Untrusted participant** — a non-admin in an **untrusted** channel. Can **talk**
  to the worker (content) and nothing more — no gate approval, no admin.

| Operation | Host op | Server admin | Trusted | Untrusted |
|---|:--:|:--:|:--:|:--:|
| Talk (messages = content) | ✓ | ✓ | ✓ | ✓ |
| Approve / deny gates | ✓ | ✓ | ✓ | ✗ |
| Manage queue | ✓ | ✓ | ✓ | ✗ |
| Edit channel **purpose** | ✓ | ✓ | ✓ | ✗ |
| Edit worker **identity** | ✓ | ✓ | ✗ | ✗ |
| Mark channel trusted/untrusted; set channels | ✓ | ✓ | ✗ | ✗ |

## Two surfaces, one rule

- **Messages** (channel or DM) are **content, never commands** (D12). No matter
  who sends them — even a server admin — the message *text* never commands the
  governance layer; it directs the worker's attention and behavior, and every
  action the worker takes is still gated. An injection in a message can't escalate.
- **Slash commands** are **authenticated, role-checked admin**. Discord verifies
  the caller's id and role; the command runs only if the caller meets the
  operation's required role. This is how humans administer — an explicit command,
  not chat.
- **Gates** hold the *worker's* proposed actions until a human of the right role
  approves. (A human editing purpose/identity via slash command is direct,
  authenticated admin — not a gate. Gates are for the worker's own proposals, D10.)

## Channel discovery

A server admin adds the worker to a channel by granting the bot access in Discord
(channel permissions / role). The worker **discovers the channels it can see** on
connect and on permission changes, and starts watching them — no channel list in
config. New channels are **untrusted by default**; an admin marks one trusted with
a slash command. **DMs are always trusted.** A channel's **purpose** starts empty
until a trusted participant sets it.

## Multi-channel, multi-person awareness

One worker watches many channels; each has many participants. Every inbound
message the worker sees carries: channel id + name, the author's id + display name
**and resolved role** (admin / trusted / untrusted), whether it's a DM, timestamp,
text, and attachment refs. So the worker always knows *where* it is, *who* is
speaking, and *what authority* they carry.

## Waking the worker, and the judgement to respond

Two independent decisions keep a busy channel from spawning endless runs or noise:

1. **When to wake (transport, cheap):** not every message spawns a run. The
   transport wakes the worker on a trigger — a DM, an @mention, an admin/trusted
   steer, or a short debounce — and batches the recent messages into one task.
2. **Whether to respond (worker, judgement):** once running, the worker decides
   whether saying or doing anything is warranted — **silence is a valid outcome**
   (a run that proposes no action). DMs usually warrant a reply; a busy channel
   often doesn't unless it's addressed or the worker has something useful.

## History & uploads on the filesystem

- The transport persists each channel's **full message history**
  (`channels/<channel-id>/messages.jsonl`) and downloads **attachments**
  (`channels/<channel-id>/files/…`).
- On a run, the box gets the **last N messages** upfront (default ~30) and can
  **read older history and any uploaded file** from the channel store, mounted
  **read-only**. Recent conversation is in context; the rest is on disk on demand.

## Identity & purpose (supersedes the single charter.md)

- **`identity.md`** — the worker's **fixed self**, the same across every channel.
  Edited only by a **server admin** (or host op).
- **`purpose.md`** — **channel-specific** role/goals, one per channel. Edited by
  **trusted participants**+.

A run's context = **identity + the active channel's purpose + briefing + recent
messages/task**. Edit paths:

- **Identity** — a **human** admin edits via `/identity`; a **worker** proposal is
  **always hard-gated** (D10, worker-wide, high blast radius).
- **Purpose** — set **conversationally**, not just by command: when a trusted
  participant describes the worker's standing role in a channel, the worker
  refines it with `propose_purpose_edit`, which **auto-applies in a trusted
  channel** (its participants could `/purpose set` directly anyway) and **gates in
  an untrusted one**. Humans can still set it directly via `/purpose set`.

This replaces `charter.md`; see the note in `charter-spec.md`.

## Replies (outbound, governed)

The worker replies through a governed action — `discord_send(channel_id, text)`
(or reply-in-thread) — whose executor holds the **bot token from the vault**; the
box never holds it. `message_user` routes to a DM. Replies are governed like any
action: metered and gated per the trust ladder (a DM reply might be auto; a first
post in a public channel might gate and earn trust).

## Slash commands (the admin surface)

Delivered over the same gateway connection — **no public endpoint**. Each command
declares a **required role**; the handler resolves the caller's role (Discord
admin perm + the channel's trust designation, DM = trusted) and refuses if it's
insufficient.

- **Trusted+**: `/gates ls|show|approve|deny`, `/queue ls|add|show|requeue`,
  `/purpose set|show`.
- **Admin only**: `/identity set|show`, `/channel trust|untrust`, worker
  enable/disable per channel.

These map to the existing `roster` subcommands. Approving a gate is the
authenticated caller (trusted+) deciding — the approval desk, role-checked, still
"no model at the edge."

## The pieces

1. **`roster listen`** — the Discord gateway client (dials out): discovers
   channels, watches messages + slash interactions, persists history/uploads,
   resolves roles, files content tasks, executes admin commands.
2. **Role resolution** — Discord admin permission + the channel's trust
   designation (+ DM = trusted) → host-op / admin / trusted / untrusted.
3. **Channel store** (`channels/<id>/`) — history, uploads, `purpose.md`, and the
   trust designation; mounted read-only into the box.
4. **`discord_send` action + executor** (bot REST); `message_user` → DM.
5. **Slash-command handler** — role-checked, maps to safe `roster` subcommands.
6. **`roster connect discord`** — bot token → vault. (Owner ids are replaced by
   live Discord role resolution.)

## Security invariants

- **Messages are content, never commands** — from anyone, including admins.
  Enforcement is unchanged by who's talking.
- **Slash commands and gate approvals are role-checked** against Discord-
  authenticated identity + channel trust; **untrusted participants can talk but
  never administer or approve**.
- **Identity is the most privileged edit** — server admin / host op only;
  worker-proposed identity/purpose edits stay **hard-gated** (D10).
- Bot token lives in the **vault**, never in the box; history/uploads are mounted
  **read-only**.
- Channels are **untrusted by default**; DMs trusted; a newly discovered channel
  starts untrusted.
- **No inbound email** — outbound-only.

## Build order (small increments)

1. **Outbound Discord** — `connect discord`, `discord_send` action + executor,
   `message_user` → DM. Trivial, high value, needed anyway.
2. **Gateway client + discovery + roles** — `roster listen`: dial out, discover
   channels, resolve roles, persist history + uploads, wake-on-trigger + batch,
   file content tasks with full channel/speaker/role awareness and last-N context.
3. **Slash commands** — role-checked admin (`gates`, `queue`, `purpose`; admin:
   `identity`, `channel trust`).
4. **Identity/purpose split** — `identity.md` + per-channel `purpose.md`; human
   edits via slash command, worker proposals hard-gated.
5. **Judgement-to-respond** — the run framing that makes silence first-class and
   weighs each speaker's role.

## Open decisions (recommended defaults)

- **"Server admin" = the Discord Administrator permission** (simple) to start;
  a configurable role can come later.
- **One primary server per worker** for identity authority; a worker spanning
  servers is out of scope for now.
- **Untrusted-by-default channels**; admins opt a channel into trusted. DMs are
  always trusted (1:1, sought-out).
- **History window N ≈ 30** upfront; the rest on disk.
- **Trusted participants cannot edit identity** — worker-wide, high blast radius;
  reserved to server admin / host op.
