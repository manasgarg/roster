# Roster

**A Control Plane for Digital Workers**

[![ci](https://github.com/manasgarg/roster/actions/workflows/ci.yml/badge.svg)](https://github.com/manasgarg/roster/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/digital-roster.svg)](https://crates.io/crates/digital-roster)
[![license](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)
[![rust](https://img.shields.io/badge/rust-2021-orange.svg)](https://www.rust-lang.org)
[![status](https://img.shields.io/badge/status-alpha-yellow.svg)](#status)

Roster runs **workers**: AI colleagues that keep working when you're not
watching — researching, tracking topics, drafting messages, doing code tasks.
You talk to them in your terminal, Discord, or Slack.

Each worker runs in a container with no credentials and no route to the
internet, except through one gateway you control. Nothing is allowed by
default, every decision is logged, and anything with real consequences — an
email, a chat post, a pull request — waits for your approval until the worker
has earned trust for that kind of action.

## What you need

- **Rust** and **Docker**
- An **Anthropic or OpenAI account** — the worker rents the model; you log in
  on first start

## Run it in the terminal

```bash
cargo install digital-roster
roster server start
```

The first start creates the config folders, pulls the container image workers
run in, and walks you through the model login. Leave it running — this is the
daemon.

In a second terminal:

```bash
roster talk
```

That opens a chat with a worker (a fresh install creates one called `elf`).
Type `/help` inside the chat for admin commands. A few useful ones from the
shell:

```bash
roster worker task add elf "find three recent papers on X and summarize them"
roster server approvals ls     # actions waiting for your approval
roster server runs ls          # every session that has run
```

## Put it on Discord

Create a bot in the Discord developer portal, invite it to your server, and
copy its bot token. Then:

```bash
roster connection add discord --worker elf
```

The wizard takes the token, stores it in the vault (it never enters the
worker's container), and binds the worker to the bot. Restart
`roster server start` and the worker connects, listens, and answers.

Channels start untrusted — the worker's replies wait at the approval desk
until you promote a channel:

```bash
roster server channel trust <channel-id>
```

DMs are always trusted. Details in [docs/channels.md](docs/channels.md).

## Status

Alpha, and honest about it: it runs every day, but it's young and the details
still move.

## Docs

Everything else lives in [docs/](docs/README.md): the architecture and
security model, connecting workers to services like GitHub, budgets, work and
scheduling, Slack, memory, and knowledge.

## Credits

Roster builds on ideas proven elsewhere first:

- [OpenClaw](https://github.com/openclaw/openclaw) — showed what an
  always-on personal AI assistant living in your chat apps can be.
- [NanoClaw](https://github.com/nanocoai/nanoclaw) — agents in OS-enforced
  containers, with a core small enough to actually read.
- [OneCLI](https://github.com/onecli/onecli) — the credential-gateway
  pattern: agents carry placeholders, real keys are injected in transit.

## License

Apache 2.0. See [LICENSE](LICENSE).
