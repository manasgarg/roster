# Memory

A worker's memory of people and conversations lives in its own
[store](store.md), under `store/memory/` — plain files the worker reads,
writes, and organizes itself. There are no memory actions and no
host-owned note store; the host's one piece of machinery is **recall**: a
bounded window into the worker's own `store/memory/memory.jsonl`,
compiled into every run's input as an advisory block (pinned notes first,
then newest; `[memory]` in org.toml bounds it —
[configuration.md](configuration.md)). A note whose record carries
`forgotten`, `disabled`, or `op: "forget"` drops out of recall; a later
record with the same `id` supersedes the earlier one. The policy text
teaches the practice: consult your memory when a person or place rings
familiar, record what deserves keeping, organize it however serves you,
and carry person-facts with discretion — what someone tells you in a
private conversation is not material for another room.

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

The worker's raw run history (transcripts, prompts, outcomes) is also
readable at `$HOME/self/runs/` — memory is what the worker distills;
`self/runs/` is the undigested record it can always go back to.

Runs still record **provenance** — which provider, channel, and user a
run served (`run-context.json` in the run dir). That record is the input
to the clean-room rule and the participant scan ([repos.md](repos.md));
it is attribution, not memory.
