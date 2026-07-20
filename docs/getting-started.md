# Getting started

This walks a fresh machine to a working worker: the daemon running, one worker
scaffolded, a task executed through the gateway, and a service connected.
The short version lives in the top-level README; this is the same path with
the reasoning attached.

## Prerequisites

- **Rust** (to install the binary) and **Docker** (workers run in containers).
- A model account: Roster drives pi, whose providers are OpenAI
  (openai-codex) or Anthropic.

## Install

```bash
cargo install digital-roster         # installs the `roster` binary
```

That's the whole install. The container image workers run in
(`ghcr.io/manasgarg/roster-box`) bakes in the agent engine and the
toolbelt; `roster server start` pulls it automatically and re-pulls on
every restart, so it stays current on its own. (To iterate on the image
itself, build it from a checkout and point `[engine] image` in `org.toml`
at your tag — see [box.md](box.md).)

## Initialize the deployment

```bash
roster init
```

Creates the three roots — config (`~/.config/roster`), data
(`~/.local/share/roster`), state (`~/.local/state/roster`) — and a
starter `org.toml`. Idempotent; it never overwrites anything. See
[layout.md](layout.md) for what lives where, and consider
`git init ~/.config/roster` — your governance config deserves a history.

## Give the gateway a model credential

The box never holds a real key, so the gateway must be able to authenticate
model calls in transit. Two pieces:

```bash
roster connection add openai-codex     # or: anthropic
```

runs the provider's login flow, stores the credential in the vault, and
scaffolds the model grant — a two-line connection file
(`connections/openai-codex.toml`) that compiles into an allow-and-inject
rule for the provider's model hosts, org-wide. Edit or delete that file to
change access; a hand-written `[[grant]]` in `org.toml` that injects the
credential takes its place. (`roster server start` offers the login on its
own when the vault holds no LLM credential: it can import an existing pi
login — always asking first, never silently — or run the login inline.
Import-and-own: after an import, roster's gateway owns the token refresh,
and pi simply re-logs-in the next time it needs to.)

That model grant is the *only* egress your workers have so far — everything
else is still default-deny.

## Scaffold a worker

```bash
roster worker init dobby
```

Creates `workers/dobby/` in config (its spec and `identity.md`) and its
knowledge repository in data. Edit `identity.md` — who this worker is, its
job, its standing rules. Identity shapes behavior; it can never grant
capability (that's what grants, actions, and trust are for).

## Validate, start, run

```bash
roster server validate    # parse + check all config, print every error
roster server start &     # the one daemon: gateway + dispatch + listeners

roster worker run dobby "find three recent papers on X and summarize them"
roster server runs ls     # everything that has run
roster server runs show <run>      # what it did, saw, proposed, cost
```

Config is read live — edit, `validate`, and the next read picks it up. Only
a binary upgrade needs a restart.

For an always-on deployment, run the daemon as a systemd user service
instead of a shell job — it survives logouts and comes back after a crash:

```ini
# ~/.config/systemd/user/roster-server.service
[Unit]
Description=roster server — gateway, task dispatch, channel listeners
After=network-online.target

[Service]
ExecStart=%h/.cargo/bin/roster server start
Restart=on-failure
RestartSec=5

[Install]
WantedBy=default.target
```

```bash
systemctl --user daemon-reload
systemctl --user enable --now roster-server
loginctl enable-linger $USER      # keep it running when you log out
journalctl --user -u roster-server -f
```

## Connect a service

```bash
roster connection add github --worker dobby     # log in once; that's the setup
# or any token-authenticated API:
roster connection add acme --host api.acme.com --worker dobby
```

One command produces the whole chain: credential in the vault, egress grant
with injection, sentinel env var in the box. Read-only by default; granting
writes is a deliberate edit. See [connections.md](connections.md).

## Let it propose actions

Actions with consequences (email, chat posts, pull requests) must be
granted before a worker can even propose them:

```toml
[[action]]
name     = "email-send"
executor = "email"
```

Proposals then wait at the approval desk:

```bash
roster server approvals ls
roster server approvals show g-8f3a     # the exact bytes that would go out
roster server approvals approve g-8f3a
```

As a worker builds a track record you can let routine things through
automatically — see [actions-and-trust.md](actions-and-trust.md).

## Put it in a chat

```bash
roster connection add discord --worker dobby       # or: slack
```

The wizard writes the binding into `workers/dobby/worker.toml`:

```toml
[channels]
discord = "discord"
```

Restart `server start` and the worker shows up, listens, and answers — every
action still governed. Channels start untrusted (replies gate);
`roster channel trust <id>` promotes one. See
[channels.md](channels.md).

## Give it standing work

Write the standing purpose into the worker's identity ("sweep the feeds;
keep the digest current") and let the machinery carry it: every worker has
a **heartbeat** (default every 30m, tuned per worker with
`heartbeat = "1h"` in its spec) that wakes it to curate its own task list —
where it schedules one-shot tasks and cron recurring templates for itself
(see [work.md](work.md)). Its self-initiated work is proactive and
budget-gated; work you file always runs. Add budgets in `org.toml` before
the fleet grows — [gateway.md](gateway.md) has the model.

## Where to go next

[architecture.md](architecture.md) for the big picture,
[security.md](security.md) for what the lockdown actually guarantees, and
[configuration.md](configuration.md) for every key you can set.
