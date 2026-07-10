# Inbound transport — channels (spec)

**Status: spec — not yet implemented.** Realizes handoff §3.9 (channels; Discord
obeys only the owner id) and D12 (channels relay, never act; inbound is content,
never commands — spoofable). Builds on the `roster relay` hand-off that already
turns a message into a content-framed task, and on the governed action path for
replies.

## Goal (concrete)

```
# owner DMs the Discord bot, or emails yuko@mg.yourdomain.com:
"can you check if bun 1.2 shipped and let me know?"
```

The message becomes a governed **task** for Yuko. Yuko researches (governed web
tools), then **replies through the same channel** via a governed action. The
owner never touches the box; the message never becomes an ungoverned command.

## The trust rule (D12) — the crux

**Inbound is content, never a command.** A message can *start* work, but its text
is never obeyed as an instruction to the governance layer. The worker acts only
through governed actions, every one judged/gated as usual — so a hostile inbound
message (or a prompt injection inside it) can direct the model's attention but
can never lift a gate, grant a capability, or edit the charter.

Sender trust differs by channel, and affects only the **prompt framing** (how
much the worker should defer), never the enforcement:

- **Discord authenticates the sender** (a user id). Messages from
  `DISCORD_OWNER_ID` are the owner's **steer** — framed as direction to follow.
  Other senders → content, or ignored (configurable per channel).
- **Email sender is spoofable**, so **all** inbound email is content — never
  trusted as a command, even if `From` claims to be the owner (D12).

Either way the worker's actions stay gated; "trusted steer" only means the model
treats the owner's words as direction, not that anything bypasses governance.

## The shape

Channels are **bidirectional and per-worker**:

- **Inbound:** transport receives a message → authorizes the sender → files a
  task (the `relay` framing: content, routed to the mapped worker).
- **Outbound:** the worker replies via a governed action (`message_user` /
  `discord_send`) whose trusted-side executor sends through the channel API.

The channel credential (bot token, webhook signing key) lives in the vault, off
the box — the box only ever *proposes* a reply, like any other action.

## The pieces

1. **`roster listen`** — the trusted inbound process (sibling to `serve` /
   `supervise`): a Discord gateway client + an HTTP endpoint for email webhooks.
   Long-running; shares the on-disk queue/vault.
2. **Channel config** (`worker.toml` `[[channel]]`): maps a source → this worker.
   ```toml
   [[channel]] kind = "email"   address = "yuko@mg.yourdomain.com"
   [[channel]] kind = "discord" channel_id = "…"   # DM/channel the worker watches
   ```
3. **Inbound email** — a Mailgun route POSTs inbound mail to `roster listen`'s
   endpoint. **Verify the Mailgun HMAC signature** (a forged POST must not inject
   tasks), then file a content task for the mapped worker.
4. **Inbound Discord** — a bot connects *out* to Discord's gateway (WebSocket),
   identifies with the bot token + message-content intent, and receives messages
   on configured channels. `DISCORD_OWNER_ID` → steer; others → content/ignore.
5. **Outbound reply** — `message_user` delivers to the owner's channel (a Discord
   DM) when one is configured, else the local inbox; a `discord_send` action posts
   to a specific channel. Bot token injected from the vault by the executor.
6. **Credentials** — `roster connect discord` stores the bot token + owner id;
   the Mailgun inbound signing key joins the vault. (Outbound email already works
   via `connect smtp`.)

## Security invariants

- Inbound content **never commands** the governance layer; every worker action
  stays gated. Sender trust changes framing, not enforcement.
- **Webhook authenticity is verified** (Mailgun HMAC); Discord messages are
  authenticated by Discord and filtered to `DISCORD_OWNER_ID` for steer.
- Bot token / signing key live in the **vault**, never in the box.
- **Email is never a command** (spoofable, D12) — always content.
- **Bounded**: inbound is rate-limited per sender/window so a flood can't spawn
  unbounded tasks; proactive-style budget still applies at dispatch.

## Build order (small increments)

1. **Outbound Discord** — `roster connect discord`, a `discord_send` action +
   executor (bot REST), and `message_user` routing to a Discord DM when
   configured. Trivial (plain HTTPS), high value, and needed for two-way anyway.
2. **Inbound email** — the `roster listen` HTTP endpoint + Mailgun-signature
   verification → file a content task. Simplest inbound *protocol* (HTTP), but
   needs a publicly reachable endpoint (a tunnel/reverse proxy).
3. **Inbound Discord** — the hand-rolled gateway WebSocket client (§3.9). The
   most code, but needs **no public endpoint** (the bot dials out) — often the
   more practical self-hosted path despite the WS work.

## Open decisions (recommended defaults)

- **Lead with outbound Discord, then pick an inbound by your infra.** The two
  inbound paths trade off cleanly: email is a simpler protocol but needs a public
  endpoint; Discord needs no public endpoint but a WebSocket client. No need to
  build both.
- **File a task, don't steer a live session (v1).** Delivering owner steer to an
  already-running session at turn boundaries (§3.2 `mailbox-poll`) is a later
  refinement; v1 files a task (or appends to the worker's mailbox) uniformly.
- **One worker per source.** A channel maps to exactly one worker; multi-worker
  routing (mentions, an inbox that dispatches) can come later.
- **Reuse `relay`.** The transport calls the same content-framing + task-filing
  path as `roster relay`, which stays as the manual/testing entry point.
