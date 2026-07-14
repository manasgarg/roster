# Getting started

This walks a fresh machine to a working imp: the daemon running, one imp
scaffolded, a task executed through the gateway, and a service connected.
The short version lives in the top-level README; this is the same path with
the reasoning attached.

## Prerequisites

- **Rust** (to install the binary) and **Docker** (imps run in containers).
- A model account: Impyard drives pi, whose providers are OpenAI
  (openai-codex) or Anthropic.

## Install

```bash
cargo install impyard                # or `cargo build --release` from a clone
docker build -t impyard-box -f box/Dockerfile .   # the container image
                                                  # (clone the repo for box/)
```

The image bakes in the agent engine and the toolbelt — build it once, and
again whenever you update. `--build-arg TIER2=1` adds pandoc, imagemagick,
and ffmpeg if your imps do document or media work.

## Initialize the deployment

```bash
impyard init
```

Creates the three roots — config (`~/.config/impyard`), data
(`~/.local/share/impyard`), state (`~/.local/state/impyard`) — and a
starter `org.toml`. Idempotent; it never overwrites anything. See
[layout.md](layout.md) for what lives where, and consider
`git init ~/.config/impyard` — your governance config deserves a history.

## Give the gateway a model credential

The box never holds a real key, so the gateway must be able to authenticate
model calls in transit. Two pieces:

```bash
impyard credential add openai-codex     # or: anthropic
```

runs the provider's login flow and stores the credential in the vault. Then
grant the model hosts in `org.toml`, with injection:

```toml
[[grant]]
name    = "model-api"
match   = { host = ["chatgpt.com", "api.anthropic.com"], port = 443 }
verdict = "allow"
inject  = { credential = "openai-codex" }
```

That grant is the *only* egress your imps have so far — everything else is
still default-deny.

## Scaffold an imp

```bash
impyard imp init yuko
```

Creates `imps/yuko/` in config (its spec and `identity.md`) and its
knowledge repository in data. Edit `identity.md` — who this imp is, its
job, its standing rules. Identity shapes behavior; it can never grant
capability (that's what grants, actions, and trust are for).

## Validate, start, run

```bash
impyard server validate    # parse + check all config, print every error
impyard server start &     # the one daemon: gateway + dispatch + listeners

impyard imp run yuko "find three recent papers on X and summarize them"
impyard server runs ls     # everything that has run
impyard server runs show <run>      # what it did, saw, proposed, cost
```

Config is read live — edit, `validate`, and the next read picks it up. Only
a binary upgrade needs a restart.

## Connect a service

```bash
impyard connection add github --imp yuko     # log in once; that's the setup
# or any token-authenticated API:
impyard connection add acme --host api.acme.com --imp yuko
```

One command produces the whole chain: credential in the vault, egress grant
with injection, sentinel env var in the box. Read-only by default; granting
writes is a deliberate edit. See [connections.md](connections.md).

## Let it propose actions

Actions with consequences (email, chat posts, pull requests) must be
granted before an imp can even propose them:

```toml
[[action]]
name     = "email-send"
executor = "email"
```

Proposals then wait at the approval desk:

```bash
impyard server gates ls
impyard server gates show g-8f3a     # the exact bytes that would go out
impyard server gates approve g-8f3a
```

As an imp builds a track record you can let routine things through
automatically — see [actions-and-trust.md](actions-and-trust.md).

## Put it in a chat

```bash
impyard credential add discord       # or: slack
```

then bind it in `imps/yuko/imp.toml`:

```toml
[channels]
discord = "discord"
```

Restart `server start` and the imp shows up, listens, and answers — every
action still governed. Channels start untrusted (replies gate);
`impyard server channel trust <id>` promotes one. See
[channels.md](channels.md).

## Give it standing work

```toml
# imps/yuko/imp.toml
[[trigger]]
schedule = "every 6h"
prompt   = "sweep the feeds; file tasks for anything worth a deep dive"
```

Trigger-filed work is proactive and budget-gated; work you file always
runs. Add budgets in `org.toml` before the fleet grows —
[gateway.md](gateway.md) has the model.

## Where to go next

[architecture.md](architecture.md) for the big picture,
[security.md](security.md) for what the lockdown actually guarantees, and
[configuration.md](configuration.md) for every key you can set.
