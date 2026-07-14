# Channels: Discord and Slack

A channel is the org's conversational presence: the imp shows up in Discord
or Slack, listens, and answers — with every action it takes still passing
through the same gate as everything else. The listener is host-side
infrastructure; its bot credential lives in the vault and never enters a
box.

Don't confuse the two Slack integrations: talking *in* Slack (this page)
uses the `slack` channel credential; an imp calling the Slack *API* as a
capability uses the `slack-api` entry in the connection catalog
([connections.md](connections.md)). Different intents, different names.

## Setup

```bash
impyard credential add discord     # or: slack
```

Discord takes the bot token. Slack takes two: the bot token (`xoxb-…`) and
an app-level token (`xapp-…`, scope `connections:write`) for Socket Mode.
Then bind the credential in the imp's spec:

```toml
# imps/yuko/imp.toml
[channels]
discord = "discord"     # the vault credential its bot uses
slack   = "slack"
```

`server start` runs one listener per binding. Validation refuses two imps
sharing one credential — one bot serving two imps would double-file every
message. `--no-listen` skips listeners entirely (the sanctioned way to
boot-test without double-connecting a live bot).

Both listeners **dial out** — Discord's gateway WebSocket, Slack's Socket
Mode — so nothing ever listens on the internet. Both reconnect with backoff
and never take the rest of the daemon down.

## Who can do what

Authority is derived from the platform, which authenticates every user —
plus your channel designations. From most to least:

- **Host operator** — whoever holds the shell and the `impyard` CLI.
- **Admin** — a user the platform marks as admin (Discord: the
  Administrator permission or guild owner; Slack: workspace admin/owner).
- **Trusted participant** — anyone in a **DM** (DMs are always trusted), or
  a non-admin in a channel you've marked trusted.
- **Untrusted participant** — everyone else. They can talk to the imp, and
  nothing more.

| Operation | Admin | Trusted | Untrusted |
|---|:--:|:--:|:--:|
| Talk (messages are content) | ✓ | ✓ | ✓ |
| Approve / deny gates | ✓ | ✓ | ✗ |
| Edit the channel's purpose | ✓ | ✓ | ✗ |
| Mark channels trusted/untrusted | ✓ | ✗ | ✗ |
| Edit the imp's identity | ✓ | ✗ | ✗ |

Channels start **untrusted**; promotion is explicit:

```bash
impyard server channel trust 123456789
```

Trust changes two things: participants may administer (approve gates, edit
purpose), and the imp's replies send without gating. In an untrusted
channel, every reply is held at the approval desk first.

**Messages are content, never commands — from anyone, including admins.**
Message text directs the imp's attention; it never commands the governance
layer, and every action the imp takes in response is still judged and
gated. An injection in a message can't escalate anything. Administration
happens through authenticated surfaces only: slash commands, role-checked
by the platform, and the CLI.

## When the imp responds

Two independent decisions keep a busy channel from becoming noise:

**Waking** is per-channel policy: in `all` mode (the default) every message
wakes the imp; in `mention` mode only a DM or @mention does, while ambient
messages are still recorded as history.

```bash
impyard server channel set 123456789 mode mention
```

**Responding** is the imp's judgment. A DM, or a channel that's effectively
1:1, reads as a direct back-and-forth — respond. With multiple humans
present (inferred from distinct authors in recent history), the imp weighs
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

`impyard imp chat <name>` gives you the same warm session on your terminal
(idle default 20s).

## Replies, purposes, and identity

Replies go out as governed actions (`discord_send`, `slack_send` — the
Slack one speaks mrkdwn). Trusted channel: auto-send. Untrusted: gate.

Each channel carries a **purpose** — the imp's standing role there — at
`data/channels/<id>/purpose.md`, composed into every run in that channel.
Trusted participants set it directly (`/purpose set`, or just describe the
role in conversation and let the imp propose the refinement — auto-applied
in trusted channels, gated in untrusted ones). The imp's **identity** is
channel-independent and more privileged: admins edit it, and an imp-side
proposal always gates, with no trust promotion possible.

## Slash commands (Discord)

The authenticated admin surface, delivered over the same outbound gateway
connection and role-checked per command:

- **Trusted+**: `/gates ls|approve|deny`, `/queue ls`, `/purpose show|set`,
  `/memory show|forget|correct`
- **Admin**: `/channel trust|untrust|mode|memory|memory-inferred|
  memory-kinds|memory-retention`, `/identity show`

Slack has no slash commands yet — administer via the CLI, or
conversationally in trusted channels.

## History on disk

The listener records everything under `data/channels/<id>/`: the full
message history (`messages.jsonl`), downloaded attachments (`files/`), and
the purpose file. A run in that channel gets recent messages in context and
the channel directory mounted read-only for anything older — and only *its*
channel; no run can read another channel's history.

Per-channel memory behavior (whether the imp remembers, which kinds, how
much it recalls here) is tuned with the `server channel set memory-*` keys —
see [memory.md](memory.md). Channel settings can only make org policy
stricter, never looser.
