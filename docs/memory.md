# Interaction memory

Memory is what an imp has learned from its interactions with people and
channels: preferences, decisions, conventions, context. It is descriptive,
scoped, inspectable, and correctable — and it is never authority. A memory
cannot grant a capability, lift a gate, change a budget, or override
identity or purpose. It only informs what the imp *tries*; the gateway
still decides what happens.

Research about the world is not memory — that's the
[knowledge repository](knowledge.md). Memory holds continuity about people
and conversations; the two stores never feed each other automatically.

## Three scopes

- **Imp** — applies across all the imp's conversations (a recurring lesson,
  a communication convention). Widest blast radius, so the strictest
  defaults: imp-wide writes gate for review unless policy allows them.
- **Channel** — shared context for one channel or workstream: decisions
  made there, local terminology, the agreed reporting format.
- **User** — durable context about one person: "prefers one-line status
  updates." The person controls memories about themselves — they can
  inspect, correct, disable, or forget them, and opt out of inference or
  cross-channel recall entirely.

A DM recalls all three (imp + that DM's channel memory + that user's
memory); the channel and user scopes stay distinct even there, so not every
fact mentioned in a DM becomes a permanent fact about the person.

Every note records its **kind** (`preference`, `fact`, `decision`,
`interaction`) and its **basis** — `explicit` (they asked) or `inferred`
(the imp concluded). Explicit beats inferred everywhere: explicit
preferences about the current speaker save automatically; inferred personal
facts require review or policy opt-in.

## How it's stored and governed

Per imp, host-side, as an append-only event log
(`data/imps/<name>/memory.jsonl`). The box never writes it — memory
operations are governed actions (`remember`, `forget`, `correct`, pin,
disable…), and the trusted side derives the imp, channel, and actor from
the run's own context. Tool arguments grant nothing: a model cannot reach
another user's memory by supplying their id, and secrets are rejected at
write time.

Corrections, pins, disables, and forgets are new events referencing the
original — history is never silently rewritten. A forgotten note leaves
recall immediately; `impyard imp memory compact` physically erases dead
content when retention or privacy requires it.

```bash
impyard imp memory ls yuko --scope user --scope-id discord:123
impyard imp memory show yuko note_01J…
impyard imp memory correct yuko note_01J… "prefers weekly, not daily"
impyard imp memory rm|pin|unpin|disable|enable yuko note_01J…
```

People can do the same conversationally — "what do you remember about me?",
"that's wrong", "forget that", "don't use memories about me across
channels" — and the host enforces the same authorization underneath.

## Recall

Memory enters a run as clearly-labeled, advisory, quoted data — after the
authoritative context, inside the budgeted prompt the
[context compiler](context.md) assembles. Recall is contextual:

- imp-only task → imp memory;
- channel task or group turn → imp + that channel's memory (private user
  notes stay out of group contexts by default);
- DM turn → imp + DM channel + that user's memory.

Naming a user or channel in task text never authorizes access to that
scope. Runs with *no* interaction context (clean knowledge-writing runs)
get no recall at all — that's the memory/knowledge boundary
([knowledge.md](knowledge.md)).

Recall is bounded (by default 20 notes, 6 000 characters, and the compiled
prompt's remaining budget) and ranked deterministically: pinned first, then
explicit over inferred, then newer. Every recall writes a trace — which
notes were candidates, which were selected, why the rest were excluded:

```bash
impyard server runs recall <run-id>
```

## Tuning

Org-wide policy lives in `org.toml [memory]` (enable, allowed kinds, note
and recall limits, retention, whether inferred/imp-wide/cross-channel
behaviors are allowed), with per-imp `[memory]` overlays. Per channel,
trusted admins tune the same dials down — never up:

```bash
impyard server channel set <id> memory off
impyard server channel set <id> memory-inferred review
impyard server channel set <id> memory-kinds preference,decision
impyard server channel set <id> memory-retention 30
```

The precedence is simple: admin limits over imp defaults over channel
policy over a participant's choices about themselves over inference — and
for privacy, the stricter rule always wins.

Full key reference in [configuration.md](configuration.md).
