# Channels: Discord and Slack

A channel is the org's conversational presence: the worker shows up in Discord
or Slack, listens, and answers — with every action it takes still passing
through the same gate as everything else. The listener is host-side
infrastructure; its bot credential lives in the vault and never enters a
box.

Talking *in* Slack (this page) and calling the Slack *API* as a capability
are two **uses** of one `slack` connection
([connections.md](connections.md)) — set up either or both from one login.

## Setup

```bash
roster connection add discord --worker yuko     # or: slack
```

Discord takes the bot token. Slack takes two: the bot token (`xoxb-…`) and
an app-level token (`xapp-…`, scope `connections:write`) for Socket Mode.
The wizard offers to write the binding into the worker's spec (that's what
`--worker` answers); it lands as:

```toml
# workers/yuko/worker.toml
[channels]
discord = "discord"     # the vault secret its bot uses
slack   = "slack"
```

`server start` runs one listener per binding. Validation refuses two workers
sharing one credential — one bot serving two workers would double-file every
message. `--no-listen` skips listeners entirely (the sanctioned way to
boot-test without double-connecting a live bot).

Both listeners **dial out** — Discord's gateway WebSocket, Slack's Socket
Mode — so nothing ever listens on the internet. Both reconnect with backoff
and never take the rest of the daemon down.

**Nothing said while the server was down is lost.** Both listeners keep a
per-channel replay cursor (the newest message id/ts handled); on every
fresh connect they fetch what arrived after the cursor over REST and run
each message down the exact same path as a live event — same wake rules,
same history, stamped with the platform's own send time. Channels the
listener has never seen get baselined on first connect, so only the very
first message in a brand-new channel during downtime waits for the next
message there. One Slack caveat: catch-up recovers top-level messages;
replies posted into an existing thread while the server was down are not
recovered.

## Scoping the bot to a server or channel

The worker's `[grant.<worker>]` edge on the Discord connection limits
where it exists: list `servers` (guild ids) and/or `channels` (channel
ids) — `roster connection grant discord yuko --restrict servers=…` writes
it — and the listener treats everything outside the scope as if it didn't
exist: not answered, not persisted, no commands registered there — while
the gateway restricts API calls to the same scope
([connections.md](connections.md) has the format and the enforcement
details). Either dimension admits a surface; DMs always pass. The edit is
live: scope changes apply without a listener restart.

## Who can do what

Authority is derived from the platform, which authenticates every user —
plus your channel designations. From most to least:

- **Host operator** — whoever holds the shell and the `roster` CLI.
- **Admin** — a user the platform marks as admin (Discord: the
  Administrator permission or guild owner; Slack: workspace admin/owner).
- **Trusted participant** — anyone in a **DM** (DMs are always trusted), or
  a non-admin in a channel you've marked trusted.
- **Untrusted participant** — everyone else. They can talk to the worker, and
  nothing more.

| Operation | Admin | Trusted | Untrusted |
|---|:--:|:--:|:--:|
| Talk (messages are content) | ✓ | ✓ | ✓ |
| Approve / deny gates | ✓ | ✓ | ✗ |
| Edit the channel's purpose | ✓ | ✓ | ✗ |
| Mark channels trusted/untrusted | ✓ | ✗ | ✗ |
| Edit the worker's identity | ✓ | ✗ | ✗ |

Channels start **untrusted**; promotion is explicit:

```bash
roster server channel trust 123456789
```

Trust changes two things: participants may administer (approve gates, edit
purpose), and the worker's replies send without gating. In an untrusted
channel, every reply is held at the approval desk first.

**Messages are content, never commands — from anyone, including admins.**
Message text directs the worker's attention; it never commands the governance
layer, and every action the worker takes in response is still judged and
gated. An injection in a message can't escalate anything. Administration
happens through authenticated surfaces only: slash commands, role-checked
by the platform, and the CLI.

## When the worker responds

Two independent decisions keep a busy channel from becoming noise:

**Waking** is per-channel policy: in `all` mode (the default) every message
wakes the worker; in `mention` mode only a DM or @mention does, while ambient
messages are still recorded as history.

```bash
roster server channel set 123456789 mode mention
```

**Responding** is the worker's judgment. A DM, or a channel that's effectively
1:1, reads as a direct back-and-forth — respond. With multiple humans
present (inferred from distinct authors in recent history), the worker weighs
whether it's addressed or genuinely useful — and staying silent is a clean
outcome, not a failure.

## Warm sessions

A burst of messages doesn't cost a cold container each. The first message in
a channel starts a live box; further messages are fed to it turn by turn
over the engine's RPC mode, serialized in order; after ~90 seconds idle it
exits cleanly. Same lockdown, same identity token, same governed actions —
a delivered message is content, exactly like a queued task's prompt.
Budgets still apply per call; there's no wall-clock ceiling on a session,
because the idle window bounds it instead.

A fresh session doesn't start blind: its first turn carries the channel's
most recent messages (default 25, `[context] history_max_messages` /
`history_max_chars`), snapshotted at wake time and labeled as content — so
the worker knows what was said before it woke, including ambient messages
that never woke it in `mention` mode. The block rides the turn input, never
the system prompt, so the prompt-cache prefix stays stable; the full record
is always mounted read-only at `$HOME/channel` for deeper reading.

`roster worker chat <name>` gives you the same warm session on your terminal
(idle default 20s).

## Replies, purposes, and identity

Replies go out as governed actions (`discord_send`, `slack_send` — the
Slack one speaks mrkdwn). Trusted channel: auto-send. Untrusted: gate.

Each channel carries a **purpose** — the worker's standing role there — at
`data/channels/<id>/purpose.md`, composed into every run in that channel.
Trusted participants set it directly (`/purpose set`, or just describe the
role in conversation and let the worker propose the refinement — auto-applied
in trusted channels, gated in untrusted ones). The worker's **identity** is
channel-independent and more privileged: admins edit it, and a worker-side
proposal always gates, with no trust promotion possible.

## Slash commands (Discord)

The authenticated admin surface, delivered over the same outbound gateway
connection and role-checked per command:

- **Trusted+**: `/gates ls|approve|deny`, `/queue ls`, `/purpose show|set`
- **Admin**: `/channel trust|untrust|mode`, `/identity show`

Slack has no slash commands yet — administer via the CLI, or
conversationally in trusted channels.

## The terminal

`roster talk <worker>` makes your own terminal a channel with the same
model as the platforms above — not a side door. It opens the durable
channel `term-<user>-<worker>`: trusted automatically (it's the operator's
own shell, the definition of a sought-out 1:1), history recorded under
`data/channels/`, a purpose the worker may propose, and warm-session turns
with the same lockdown and governance. The one difference is delivery:
there is no send tool — the worker's reply text prints directly in your
terminal. And like every conversation, gated repos are read-only; durable
repo work goes through `file_task`.

## History on disk

The listener records everything under `data/channels/<id>/`: the full
message history (`messages.jsonl`), downloaded attachments (`files/`), and
the purpose file. A run in that channel gets recent messages in context and
the channel directory mounted read-only for anything older — and only *its*
channel; no run can read another channel's history.

Per-channel memory behavior (whether the worker remembers, which kinds, how
much it recalls here) is tuned with the `server channel set memory-*` keys —
see [memory.md](memory.md). Channel settings can only make org policy
stricter, never looser.
