# Interaction memory — scoped memories with governed recall (spec)

**Status: implemented.** Memory is what an imp has learned from its
interactions with people and channels. It is descriptive, scoped, inspectable, and
correctable. It helps the imp resume work and adapt to people without changing
its identity, purpose, capabilities, or gates.

This spec defines three memory scopes:

1. **Imp** — knowledge that applies across the imp's conversations.
2. **Channel** — shared context that applies only in one channel or workstream.
3. **User** — knowledge about one person across their interactions with the
   imp.

In a DM, the user is the only participant and the DM acts as their channel. The
channel and user scopes remain distinct: the channel holds conversation or
workstream context; the user scope holds durable facts and preferences about the
person.

## Memory is not authority

Impyard has three different kinds of context:

- **Identity** (`identity.md`) is the imp's constitution. It is owner-authored,
  changed rarely, and never learned or edited by the imp.
- **Purpose** (`purpose.md`, per channel) is the role assigned to the imp by a
  trusted human. It is directive: "what should I do here?"
- **Memory** is learned context. It is descriptive: "what have I learned that may
  help here?"

Memory is always advisory and untrusted. A memory cannot grant a capability,
lift a gate, change a budget, override identity or purpose, or authorize an
action. There is no automatic promotion path from memory into identity, purpose,
or a procedure. Such changes use their existing trusted paths.

## The three scopes

### Imp memory

Imp memory applies across the imp's runs. Examples include a recurring
interaction lesson or a broadly applicable communication convention.

Imp memory has the widest blast radius. The owner/admin controls its write,
retention, and recall policy. The imp may write or propose imp memories
only when that policy allows it. The recommended default is review for inferred
imp-wide memories.

Imp memory is not a place for standing instructions. A learned procedure may
be proposed for promotion through a trusted admin path, but remains an advisory
observation until then.

### Channel memory

Channel memory is shared context for one channel or workstream. Examples include:

- a decision made by the channel;
- the current workstream decision and open questions;
- local terminology;
- reporting conventions for that channel;
- the agreed reporting format for the workstream.

A channel steward controls channel policy and accepted shared memories. Ordinary
participants may create explicit notes about the current conversation or propose
shared notes. They cannot change the global memory policy or memories belonging
to another channel.

Until channel-steward roles exist, the owner/admin performs steward operations.

### User memory

User memory contains durable context about a person, such as an explicitly stated
communication preference. It follows the user across channels where policy
permits cross-channel recall.

The person controls memories about themselves: they can inspect, correct,
disable, or forget them. A channel steward does not gain access to all of a
person's memories merely because that person is in the channel.

Inferred user memory is more error-prone than explicit memory. Every user note
records whether it came from an explicit statement or an inference. The
recommended default is to remember explicit preferences automatically and
require repeated evidence or review for inferred personal facts.

## DMs

A DM run recalls:

```text
imp memory
+ DM channel memory
+ the DM user's memory
```

The two narrower scopes serve different purposes:

```text
user:discord:123
  "Prefers concise status reports."

channel:discord:456
  "This conversation uses a weekly written status update."
```

Even if an adapter exposes a DM as a user-addressed channel, Impyard preserves
both scope meanings. This prevents every fact mentioned in a DM from becoming a
permanent fact about the user.

## Research knowledge is separate

Interaction memory does not store the imp's research about the world.

- Sources, extracted material, claims, research notes, syntheses, and briefs
  belong in the imp's Git-backed knowledge repository or container `/tmp`.
- Memory holds continuity about people, channels, and their interactions. It is
  never an automatically recalled index of research artifacts.

The full storage and concurrency model is defined in
`docs/knowledge-repo-spec.md`. Nothing automatically promotes or copies content
between interaction memory and the knowledge repository.

## Stored record

Memory is stored per imp, off the box, as an append-only event log:
`memory/<imp>.jsonl`. The box's repo mount remains read-only; the imp never
writes this file directly.

Deployments upgraded from the older `notes/<imp>.jsonl` path continue to read
that legacy log. New events use `memory/`, and owner compaction completes the
physical migration. Existing records with the retired `research` kind remain
inspectable but are not recalled and cannot be newly created.

A created note has at least:

```json
{
  "op": "remember",
  "id": "note_01J...",
  "ts": "2026-07-10T12:00:00Z",
  "scope": "user",
  "scope_id": "discord:123",
  "kind": "preference",
  "note": "Prefers replies of one or two sentences.",
  "basis": "explicit",
  "source": {
    "channel_id": "discord:456",
    "message_id": "discord:789",
    "author_id": "discord:123"
  },
  "expires_at": null
}
```

Required fields and meanings:

- `scope` is `imp`, `channel`, or `user`.
- `scope_id` is absent for imp memory and is a provider-qualified stable ID
  for channel and user memory.
- `kind` is `preference`, `fact`, `decision`, or `interaction`.
- `basis` is `explicit` or `inferred`.
- `source` identifies the interaction that established the memory and may point
  to its channel, message, author, or run.
- `expires_at` supports temporary memory and retention policy.

Display names are optional hints for people reviewing notes. They are never used
as identity keys.

Corrections, disabling, pinning, and forgetting are new events referring to the
original note. They do not silently rewrite history. A `forget` tombstone removes
the note from recall immediately; compaction can physically erase deleted content
when required by retention or privacy policy.

## Memory actions

The box exposes governed actions rather than filesystem access:

- `remember(note, scope?, scope_id?, kind?, basis?, source?)`
- `forget(note_id)`
- `correct(note_id, replacement)`
- `disable(note_id)` / `enable(note_id)`
- `pin(note_id)` / `unpin(note_id)`

The trusted executor derives the imp, current channel, actor, and available
subjects from the run envelope. Tool arguments do not grant access. For example,
a model cannot gain access to another user's memory by supplying their ID.

Safe defaults reduce tool-call ceremony:

- An explicit preference about the current speaker defaults to user scope.
- Shared conversation state defaults to the current channel.
- Imp scope is never inferred merely because no user or channel was supplied;
  it must be selected deliberately and allowed by admin policy.

Every action is journaled and audited. Auto-approved note creation is permitted
only within the configured write policy. Wider or sensitive writes may require
review.

## Authority and tuning

Authority follows blast radius. Admin rules are hard limits; narrower scopes may
be more restrictive but cannot relax them.

### Owner/admin controls

The owner/admin controls the memory system and imp-wide behavior:

- allowed and prohibited memory kinds;
- sensitive-data rules;
- whether inferred memories are allowed or require review;
- imp-scope writes and recall;
- cross-channel user recall;
- maximum note size, count, retention, and recall token budget;
- ranking and consolidation mechanisms;
- audit, storage, compaction, and deletion policy;
- the set of channel-level options participants may change.

### Channel-steward controls

Within the admin limits, a channel steward controls:

- whether memory is enabled for the channel;
- which allowed note kinds are active;
- whether inferred channel notes are automatic or reviewed;
- shorter retention and smaller recall budgets;
- accepted, pinned, or corrected shared notes;
- channel interaction and reporting conventions;
- the channel's recall profile.

### Participant controls

An ordinary participant may:

- explicitly ask the imp to remember something about themselves;
- choose whether that memory is user-wide or only shared in the current channel;
- inspect, correct, disable, or forget memories about themselves;
- opt out of inferred personal memory or cross-channel recall;
- create or propose channel notes when channel policy permits;
- ask which memories were used in a response.

A participant cannot change another person's memory, another channel's memory,
imp-wide policy, or the imp's identity, purpose, grants, gates, or budgets.

### Precedence

```text
admin hard limits
  → imp defaults
    → channel policy
      → participant choices about themselves
        → imp inference
```

For privacy and safety, the more restrictive rule wins. For two advisory notes
about the same subject, explicit beats inferred, a current scoped decision beats
a broader default, and a correction beats the older note.

## Recall into runs

The compiled context is split so stable instructions form a cache-friendly
prefix and volatile observations stay at the end:

```text
system: Identity → Runtime policy → Purpose → Runtime scope
input:  Memory → Briefing → Task or current message
```

Memory is placed after authoritative context, clearly labeled as untrusted,
advisory observations. The compiled context and selected note IDs are logged so
"what did the imp see?" is answerable. The exact rendering and cache
boundaries are defined in `docs/context-compiler-spec.md`.

Recall is contextual:

- A DM receives imp + DM channel + DM user memory.
- A group channel receives imp + channel memory. Relevant user memories may be
  included only when allowed for shared-context recall; private user notes remain
  out of group context.
- A one-shot channel task receives imp + that channel's memory.
- An imp-only task receives imp memory. Naming a user or channel in task text
  never authorizes access to that scope.

Recall is bounded from v1. The context compiler enforces admin and channel
character and note limits, excludes expired/disabled/forgotten notes, removes
duplicates, and ranks eligible notes. The initial ranking favors:

1. pinned notes;
2. explicit notes over inferred notes;
3. notes matching the current channel, user, and task;
4. current notes over stale notes;
5. recent notes when other signals are equal.

The system records candidates, selected notes, and exclusion reasons. Owners can
inspect the full trace; channel stewards see their channel's trace; participants
can see the memories about themselves or shared with the channel, subject to the
privacy policy.

## Security and privacy invariants

- Memory never participates in authorization or policy enforcement.
- Memory is rendered as quoted data, not instructions.
- A note cannot alter identity, purpose, capabilities, gates, or budgets.
- The host authorizes memory actions from trusted run metadata, not model-supplied
  subject IDs.
- Imp-wide and cross-channel recall follow admin policy.
- Channel membership does not grant access to another person's private memory.
- Secrets, credentials, and prohibited sensitive data are rejected by the write
  policy.
- Recall has hard size limits, so repeated note creation cannot exhaust the model
  context.
- Every write, correction, recall, and deletion is attributable and auditable.
- Owners and users can prune memory, and a forgotten note stops being recalled
  immediately.

These boundaries limit exposure and influence; they do not make model-visible
memory confidential from other content in the same run. Private personal memory
therefore stays out of group-channel prompts.

## Owner and participant interfaces

The owner CLI supports at least:

```text
impyard memory ls [--imp <id>] [--scope imp|channel|user] [--scope-id <id>]
impyard memory show <id>
impyard memory rm <id>
impyard memory correct <id>
impyard memory pin <id>
impyard memory explain <run-id>
```

Equivalent participant operations may be requested conversationally:

```text
"Remember that I prefer short status reports."
"Only remember that for this channel."
"What do you remember about me?"
"That note is wrong."
"Forget that."
"Do not use memories about me across channels."
```

The host still enforces scope and actor authorization. Natural-language content
does not bypass the governed action path.

## Implementation

1. **Scoped core (built)** — append-only event schema; imp/channel/user scopes;
   `remember` and `forget`; provenance; host-side authorization; bounded
   contextual recall; owner `ls`, `show`, `rm`, and recall trace.
2. **Correction and participant control (built)** — correct/disable/pin events;
   conversational inspect/correct/forget; explicit versus inferred policy;
   channel-steward and user authorization.
3. **Tuning and maintenance (partly built)** — configurable limits, channel
   overrides, expiry and retention, duplicate detection, compaction, and recall
   traces are built. Automated consolidation proposals remain future work.

Remove the imp's `propose_identity_edit` tool as part of the scoped core. The
existing admin/owner identity-edit path remains unchanged.

## Recommended defaults

- Explicit user preferences: automatic, user-correctable.
- Inferred user facts: review or repeated evidence; never sensitive traits.
- Channel observations: automatic only for low-risk kinds; channel decisions are
  explicit or steward-approved.
- Imp-wide inferred memory: review.
- Cross-channel user recall: off unless the user and admin policy permit it.
- Group recall of private user memory: off.
- Recall: bounded and logged from the first version.
- Research material never enters interaction memory; it stays in the knowledge
  repository or container temporary files.
