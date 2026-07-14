# Impyard

**Rent the intelligence. Own the governance.**

[![license](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)
[![rust](https://img.shields.io/badge/rust-2021-orange.svg)](https://www.rust-lang.org)
[![status](https://img.shields.io/badge/status-alpha-yellow.svg)](#where-this-is)

Impyard runs **imps**: software colleagues that keep working when you're not
watching. An imp researches things, keeps an eye on topics you care about,
drafts messages, tidies what it has learned. It talks to you in Discord or
Slack, and it picks up jobs from a queue.

The model doing the thinking is rented — today's best one, swapped out
whenever a better one shows up. Everything that decides what an imp is
*allowed to do* stays yours: plain files on your machine, in your git history,
running under your control. That's the whole idea. You rent the brain; you own
the leash.

## How an imp is kept honest

An imp lives in a container with **no passwords and no way out to the
internet** — except one door you control.

That door is a gateway. Every request the imp makes goes through it, and the
gateway decides: allowed, or not. Nothing is allowed by default; you say what
is. It also keeps count, so an imp can't quietly run up a huge bill, and it
writes down every decision it made, forever.

Passwords never go inside the container. The imp carries a fake one. When a
request is allowed to reach, say, GitHub, the gateway swaps in the real token
on the way out. So there is nothing worth stealing inside the box — even if
the imp is tricked, and even if the code it runs is malicious.

Anything with real consequences — sending an email, posting a message, opening
a pull request, changing what the imp is — doesn't just happen. The imp
*proposes* it, and a human approves. As an imp builds a track record, you can
let it stop asking for the routine things. Trust is earned, one action type at
a time.

Two other things worth knowing:

- **What it learns about the world** is kept in a git repository. The imp
  writes notes; the trusted side checks them and makes the commit. History
  can't be rewritten by the imp.
- **What it remembers about people** is kept separately, and people can see it,
  correct it, and ask it to forget. An imp can never write person-notes into
  its world-knowledge, because a conversation gives it read-only access there.
  A separate, clean run does the writing.

## Try it

You need Rust and Docker.

```bash
cargo build                                       # the impyard binary
docker build -t impyard-box -f box/Dockerfile .   # the container imps run in

impyard init                     # create your config, data, and state folders
impyard imp init yuko            # scaffold an imp
impyard server validate          # check your config; it's read live, no deploy step
impyard server start &           # the one daemon: gateway + queue + chat listeners

impyard imp run yuko "find three recent papers on X and summarize them"
impyard imp ls                   # your imps, at a glance
impyard server gates ls          # anything waiting for your approval
impyard server runs ls           # everything that has run, ever
```

To let an imp use a service (GitHub, Slack, Notion, …):

```bash
impyard server connect github --imp yuko   # log in once; that's the whole setup
```

To put an imp in a chat, connect Discord or Slack the same way and add one line
to its config. It shows up, listens, and answers — with every action it takes
still going through the same gate.

## Where this is

Alpha, and honest about it: it runs every day, but it's young and the details
still move. Built in small steps, each one tested live before the next starts.

Working today: the locked-down container and the gateway in front of it;
default-deny rules, credential injection, budgets, and a permanent audit log;
a task queue with schedules and follow-ups; the approval desk and the
earned-trust ladder; Discord and Slack conversations with warm sessions;
per-person memory with consent; git-backed knowledge; and code tasks that end
in a pull request you approve.

## How it's built

One rule shapes the whole codebase: **the trusted side and the untrusted side
are different languages, so they can never blur together.**

- Everything trusted is **Rust** — a single `impyard` binary. It holds the
  passwords, makes the decisions, and writes the audit log.
- Everything inside the container is **TypeScript** — the agent engine and its
  tools. It's assumed to be compromised, and given nothing worth taking.
- The imp's container is deliberately **capable**: real tools, a real
  toolchain, `gh` and friends. Being useful is not the same as being
  trusted — capability lives inside, authority lives at the edge.

Your config and data never live with the code. Config goes in
`~/.config/impyard`, durable data in `~/.local/share/impyard`, throwaway state
in `~/.local/state/impyard`. Edit a config file and it takes effect on the next
read — there is nothing to deploy. If you break it, everything fails closed
rather than open.

The source is organized the way the system actually works: `gateway/` (the one
door), `credential/` (passwords, never in the box), `action/` (propose, approve,
execute), `work/` (the queue), `run/` (starting containers), `imp/` (identity,
memory, knowledge), `channel/` (Discord, Slack), and `cli/`.

## Docs

The design, the decisions and why they were made, and the build plan:
[docs/impyard-handoff.md](docs/impyard-handoff.md). The rest of `docs/` goes
deep on individual pieces — the gateway, budgets, memory, knowledge, the
memory/knowledge boundary, connections, and the CLI.

## License

Apache 2.0. See [LICENSE](LICENSE).
