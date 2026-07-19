# Memory

A worker's memory of people and conversations lives in its own
[store](store.md), under `store/memory/` — plain files the worker reads,
writes, and organizes itself. The host keeps no memory machinery: nothing
is recalled into prompts by the host, no note store is host-owned, and
there are no memory actions. The briefing teaches the practice instead:
consult your memory when a person or place rings familiar, record what
deserves keeping, organize it however serves you, and carry person-facts
with discretion — what someone tells you in a private conversation is not
material for another room.

Two structural facts shape how memory behaves:

- **The store travels with the worker, not the channel.** Every run
  mounts the same `store/`, so what a worker learns about someone is
  available wherever it serves them next. Cross-channel discretion is
  conduct (identity.md and the briefing set the expectation), not a
  storage boundary.
- **The person-space boundary survives where it is enforceable.** Gated
  repos ([repos.md](repos.md)) still refuse pushes from runs that carried
  interaction content, and the participant scan still checks pushed files
  and task prompts. Memory in the store is deliberately outside that
  system — it is *for* person-facts.

What people can see and change: the store is on the host
(`data/workers/<name>/store/`), so the operator can read and edit memory
files directly, and the snapshot rotation (`roster worker restore`)
covers them like everything else in the store.

Runs still record **provenance** — which provider, channel, and user a
run served (`run-context.json` in the run dir). That powers the taint
rule and the participant scan; it is attribution, not memory.

Deployments upgraded from the host-memory era: `roster migrate` seeds
`store/memory/memory.jsonl` with a copy of the old host-owned
`memory.jsonl`, which otherwise stays untouched on disk as an inert
historical file.
