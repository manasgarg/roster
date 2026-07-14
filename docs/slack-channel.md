# Slack as a channel (spec, 2026-07-14)

**Status: implemented through increment 4 (warm sessions)** — Socket Mode
client in `src/channel/slack.rs`, platform dimension in the listener plan and
locks, `slack_send` action/executor with trusted-channel auto-send, Slack
session scope (mrkdwn), `message_user` Slack-DM fallback. Deferred as
specced: interactions/slash commands, webhook mode, multi-workspace,
reaction acks.

**Two different "Slack integrations" — one already exists.** An imp calling
the Slack *API* as a capability (read a channel's history, look up a user) is
a **service connection** and shipped with docs/connections.md — it needs only
a catalog entry. This spec is the other thing: **Slack as a channel** — the
org's conversational presence, like Discord today. Host-consumed
infrastructure: a listener turns inbound messages into tasks or warm-session
turns; replies flow out as governed actions; the credential never enters a
box.

**Naming.** `slack` becomes the channel provider (like `discord`); the
service-connection catalog entry is renamed `slack-api` (hosts slack.com, env
`SLACK_TOKEN`). Talking *in* Slack and *calling* Slack are different intents
and get different names.

## Connection mode: Socket Mode

Slack offers two inbound transports. **Events API webhooks** require a public
HTTPS endpoint — rejected: impyard's posture is outbound-only dialing (the
Discord listener dials a websocket out; nothing listens on the internet).
**Socket Mode** matches: `apps.connections.open` returns a wss URL, events
arrive as envelopes over the websocket, each acked within ~3s. Reconnect on
`disconnect` envelopes with the same backoff supervision Discord uses.

Credential (one vault entry, `auth = "slack"` flow prompting for both):

- **bot token** (`xoxb-…`) — Web API calls: post, DM open, auth.test
- **app-level token** (`xapp-…`, scope `connections:write`) — Socket Mode

Setup lives in Slack's app config (a manifest we document): bot scopes
`chat:write`, `im:history`, `im:write`, `channels:history`, `users:read`,
event subscriptions `message.im`, `message.channels`, `app_mention`.

## Mapping the Discord architecture

| Discord today | Slack |
|---|---|
| gateway ws + IDENTIFY | Socket Mode ws via `apps.connections.open` |
| READY → bot id | `auth.test` → bot user id |
| GUILD_CREATE channel census | `conversations.list` at connect (log line parity) |
| MESSAGE_CREATE | `events_api` envelope: `message` / `app_mention` |
| REST `post_message` | `chat.postMessage` (+ `thread_ts` when replying in-thread) |
| `open_dm` | `conversations.open` |
| typing indicator | none in Socket Mode — skipped |
| role bitfield → admin/member | `users.info` `is_admin`/`is_owner` → admin; else member |
| `[channels] discord = "cred"` | `[channels] slack = "cred"` |
| `discord_send` action/executor | `slack_send` action/executor, same trust semantics |

**What is reused untouched** (it is channel-id keyed and platform-agnostic):
trust designation, response mode, memory settings, purpose, history recording
under `data/channels/<id>/` — Slack ids (`C…`, `D…`) drop straight in. Same
authority rules: a DM party is trusted; a workspace member in an untrusted
channel is content-only; `server channel trust <id>` is the escalation.
One credential serves one imp (same validation as Discord — a second
imp on the same bot would double-file every message).

**What is new code:**

- `channel/slack.rs` (~the size of discord.rs): Socket Mode client (connect,
  ack, reconnect), event → `handle_message` (same shape: record history,
  route to warm session or file a relay task), Web API helpers.
- `listen.rs`: the listener plan gains the platform dimension —
  `[channels]` entries become `(imp, platform, credential)`; supervision
  is unchanged.
- `action/mod.rs`: `slack_send` executor + trusted-channel auto-send (the
  `discord_channel_trusted` check generalizes: same settings store).
- `box/extensions/actions.ts`: `slack_send` tool mirroring `discord_send`
  (compose full message; may auto-send in trusted channels, else gates).
- `imp/context.rs`: the Discord session scope text generalizes to a
  channel-session scope parameterized by platform + send-tool name, plus one
  Slack-specific line: output is mrkdwn (`*bold*`, `<url|label>` links), not
  Markdown.
- `message_user` executor: today it falls back to a Discord DM; it learns to
  try whichever channel credential the imp is bound to.

**Deliberately deferred:** slash commands and interactions (Discord's
`/purpose` etc. — purpose edits stay CLI/trusted-message only for Slack v1);
Events API webhook mode; multi-workspace installs; emoji-reaction acks.

## Increments

1. **Provider + credential** (S): `auth = "slack"` connect flow (two tokens),
   `slack-api` rename in the catalog.
2. **Inbound, tasks only** (M): Socket Mode client, history recording, relay
   task filing. An imp can be written to in Slack; it answers via gates.
3. **`slack_send` + trusted channels** (S): executor, action grant, trust
   auto-send. Conversational loop closes.
4. **Warm sessions** (M): route `message` events into live sessions like
   Discord's `route_to_session`, including the channel-session scope text.
5. Later: interactions/slash commands, reaction acks.

## Invariants preserved

The listener is trusted host-side code with direct egress (like the Discord
websocket and SMTP — the gateway is an HTTP proxy and doesn't carry these);
its credential lives in the vault and never enters a box. Inbound Slack
content is content, never authority — same untrusted framing as Discord
relay tasks. Replies in untrusted channels gate; trust is a channel-level
admin designation, not a Slack-side property.
