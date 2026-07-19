# Plan: channel semantics — surfaces, logical channels, and the store split

Status: design settled (2026-07-20) — implementation not started
Scope: split the platform-native *surface* from the logical *channel*; add
class-based scoping (`public` / `private` / `dm`) to grant edges; give each
(worker × channel) pair its own durable store next to the worker's global
store; retire "taint" as roster-wide vocabulary in favor of run provenance
plus a clean-room predicate owned by the host-repo connection kind.
Identity linking (a person entity) is explicitly deferred.

## Motivation

Two pressures on the current model:

1. **Scope can name places but not shapes.** A grant edge can list servers
   and channel ids ([connections.md](../connections.md)), but "public
   channels only" or "no DMs" is inexpressible — DM admission is a
   hardcoded invariant, not a policy.
2. **A person is many places; the worker sees many conversations.** The
   operator talks to one worker from the terminal, a Discord DM, and a
   Slack DM, and the worker experiences three unrelated channels with three
   histories and three warm sessions. The inverted view — the person as
   the conversation, their presence points as routes into it — has no
   entity to hang on.

Plus one reframing that fell out of working the design: "taint" reads today
as roster-wide architecture, but its only consumer is the gated-repo
enforcement point. It is the host-repo provider's implementation of its
write contract, and the vocabulary should live where the mechanism does.

## Naming

After this change **"channel" means the logical conversation**. The
platform-native thing — a Discord channel, a Slack DM, a terminal session's
durable id — is a **surface**. The current system is the degenerate case
where every surface is its own singleton channel; that equivalence is what
makes this an evolution, not a migration.

## The model

Entities:

| Entity | What it is | Cardinality notes |
|---|---|---|
| **Provider** | registry entry: auth kind, hosts, container dimension (Discord: `servers`; Slack: workspace), surface-class vocabulary | static; 1—N connections |
| **Connection** | org ↔ service: secret + uses | instantiates one provider |
| **Grant** | worker/org × connection edge carrying scope | one scope governs every use of the edge (unchanged) |
| **Surface** | (provider, native id) + class + container | discovered/classified by the listener; owns the replay cursor |
| **Channel** | group of 1+ surfaces; the unit of conversation | owns history, purpose, trust, memory settings |
| **Worker store** | the worker's global durable space | 1 per worker (unchanged) |
| **Channel store** | conversation-scoped durable space | 0..1 per (worker × channel), lazy |
| **Run / Session / Task** | execution | session: at most 1 live per (worker × channel) |

Relationships that carry the design:

- **Surface N—1 Channel**, membership total: every surface belongs to
  exactly one channel, singleton by default. `roster channel link`
  re-parents surfaces into a shared channel.
- **Stores and warm sessions key on (worker × channel)** — linked surfaces
  share both *by construction*, with no separate sharing mechanism.
- **Runs record surfaces, not just channels**, in provenance: the channel
  is the conversation; the surface is the evidence of where it physically
  happened. That keeps the participant scan and any provenance predicate
  precise after linking.

Derived, deliberately not stored:

- **Admission** — whether a worker exists on a surface — evaluated live
  from grant scopes against the surface's id, container, and class.
- **Authority** — a participant's role (host-op / admin / trusted /
  untrusted) computed per turn from platform attestation plus the
  channel's trust state. No person entity exists; platform identities in
  history and provenance are all there is (the deferred slot: a person
  entity would group identities exactly as a channel groups surfaces).
- **Write contracts** — each connection kind's predicate over run
  provenance; host-repo's clean-room rule is today's only instance.

## Decisions (locked)

1. **Scope gains a class vocabulary; union semantics stay.** The surface
   level of a grant scope may name explicit ids and/or classes (`public`,
   `private`, `dm`); each entry admits surfaces, an empty level admits
   everything the level above admits, a listed container admits its
   surfaces, a listed id works even when its container isn't listed — all
   as today. Classes are provider-declared registry vocabulary, like
   `scope_dims`.
2. **DM admission becomes a default, not an invariant.** A scope that
   names no classes keeps today's behavior (ids admit rooms, DMs pass). A
   scope that names classes is exhaustive: `surfaces = ["public"]` means
   no DMs. Backward compatible with every existing config, and it makes
   "this worker does not do DMs" expressible for the first time — which
   also relieves the pressure on DMs-always-trusted (the mitigation for
   stranger DMs is now scope, not trust surgery).
3. **Classes describe scope, never trust.** A private channel with forty
   people is not a DM. Trust stays exactly as it is: DMs and terminal
   trusted, everything else promoted explicitly via `channel trust`.
4. **Class enforcement shares the listener's classification.** Id lists
   compile to gateway path predicates as today. Classes can't compile
   statically (an API path doesn't reveal a channel's type), so the
   gateway consults the listener's channel-type map — same daemon, same
   source of truth — and **fails closed** on surfaces the map has never
   seen. The "speak there / act there never drift" guarantee survives in a
   weaker form: both enforcement points read one classification. Document
   this honestly rather than pretend classes compile.
5. **Linking is an operator act on surfaces.** `roster channel link <a>
   <b>` declares, from the authenticated CLI, that two surfaces are one
   conversation. No identity verification exists or is needed — the host
   operator asserts the link. This delivers the inverted user-primary view
   for the person who matters first (the operator) with none of the
   spoofing surface. Automated identity linking, when it arrives, is just
   an automated way of creating these groups.
6. **v1 links 1:1-shaped surfaces only.** A multi-surface channel may
   contain only dm-class and terminal surfaces; group channels are
   refused. This dissolves two hard problems at once — mixed audience
   (merging a public room's history into a DM leaks by construction) and
   mixed trust (every 1:1 surface is already trusted, so uniform trust
   across a group is a theorem, not a rule to enforce). Linking two team
   rooms is a plausible future that must earn its own decision.
7. **Everything conversational re-keys to the channel.** History, purpose,
   trust, `memory-*` settings, warm-session serialization, and the channel
   store all key on the logical channel. Concrete win that falls out: the
   operator messages from Slack, walks to the terminal, and lands in the
   same live warm session. Replay cursors stay per **surface** — they are
   built from platform-native message ids.
8. **Reply routing is decided host-side, per turn; no unified send.**
   Default: reply where the person last spoke. The turn's briefing directs
   the worker — "reply with `slack_send` to id X, in mrkdwn" — so the
   worker experiences one conversation and addresses each reply as told.
   A dialect-agnostic routed send would need markup translation nobody
   wants to maintain; it can come later if the routing rule proves stable.
9. **The store splits into two layers.** The worker-global store stays
   exactly what it is — projects, working repos, task file, notes, memory:
   the worker's professional life, mounted rw everywhere, snapshotted
   ([store.md](../store.md) unchanged). New: a **channel store** per
   (worker × channel), mounted rw at `$HOME/channel/store/` beside the
   existing read-only history mount, holding conversation-scoped material
   — files people shared, work products for that room, standing context.
   Linked surfaces share it by construction (decision 7).
10. **Channel stores are worker-keyed on disk.** They live under
    `data/workers/<name>/channel-stores/<channel>/`, not under
    `data/channels/` — two workers can serve one channel through different
    bots and must not share a filesystem. The asymmetry is principled:
    history is the *host's* record of the channel (channel-keyed); stores
    are the *worker's* space (worker-keyed). Living under the worker's
    data dir also puts channel stores inside the existing
    snapshot/restore machinery.
11. **Conversation material follows the conversation.** Channel history
    and the channel store are provisioned only into runs serving that
    channel — live sessions and tasks that carry the channel's context.
    Tasks filed clean don't get them (otherwise conversation content rides
    into a run holding writable gated clones and the clean-room boundary
    is fiction). This is stated as *channel* semantics, standing on its
    own; it does not reference the clean-room rule, and doesn't need to.
12. **Memory stays in the global store — deliberately.** Moving
    person-memory into channel stores would make cross-room privacy
    enforced rather than behavioral, but it would also make the worker
    unable to recognize anyone across rooms, and the mitigation for that
    is the person entity we deferred. Enforced partitioning without
    identity linking buys privacy at the cost of recognition. Discretion
    stays conduct ([memory.md](../memory.md)); revisit when people are
    first-class.
13. **Unlink is honest: no un-sharing.** Merging stores on link is a copy
    (histories interleave by timestamp; records already carry native ids).
    Unlinking cannot revoke what a worker already read: the shared store
    stays with the remaining group (archived if the group dissolves), and
    a departing surface starts over as a fresh singleton with an empty
    channel store.
14. **"Taint" retires into the host-repo provider.** What roster core
    keeps is **run provenance**: a faithful record of what entered every
    run — mounts, delivered turns, continuation context, participants and
    the surfaces they spoke on. That record cannot be delegated; an
    incomplete record makes every predicate over it a boundary in name
    only. What moves is the predicate: tainted/clean becomes the host-repo
    connection kind's **clean-room eligibility** rule, evaluated against
    provenance at clone-provisioning and push time. The general roster
    principle was never taint — it is "world-effects go through governed
    actions"; gated repos are the special case where judging fails (bulk
    file writes, paraphrase) and provisioning-based information flow
    substitutes. Capabilities were never governed by taint and still
    aren't.
15. **Two things fall out of 14, adopt both.** Per-connection policy:
    `write_from = "clean-room" | "any-run"` belongs on the individual
    host-repo connection file, with the org.toml `[knowledge]` setting as
    the default — a strict research-kb and a permissive scratch repo can
    coexist. Scan scoping: the participant scan exists solely to police
    the `file_task` bridge into clean runs with writable clones, so it
    visibly belongs to the host-repo provider and does not engage for
    workers with no gated-repo grant.

## Non-goals

- **Identity linking / person entity.** Deferred wholesale. Everything
  here is shaped so a person entity later grafts on without moving
  anything: it groups platform identities exactly as a channel groups
  surfaces. Authority-vs-conversation for weakly-authenticated providers
  (email `From:`, WhatsApp) is deferred with it.
- **Linking group channels** (decision 6) and any mixed-trust
  reconciliation rule.
- **A unified send tool / cross-provider markup translation** (decision 8).
- **Moving memory into channel stores** (decision 12).
- **New channel providers** (email/WhatsApp as listening surfaces). The
  surface model is built to receive them — a mailbox thread or a WhatsApp
  conversation is a dm-class surface — but this plan adds none.
- **Slack workspace dimension.** One Socket Mode credential is one
  workspace today; the container dimension for Slack stays vacuous until
  multi-workspace is real.

## Compatibility and migration

- **No config migration.** Existing scopes name no classes, so decision 2
  keeps their semantics bit-for-bit, DM admission included.
- **No data migration at upgrade.** Every existing surface is implicitly
  its own singleton channel; `data/channels/<id>/` remains valid as the
  singleton case. Merging happens only when an operator links, and is a
  one-time interleave copy under the new channel's directory.
- **Namespace fix riding along.** `data/channels/<id>/` currently uses raw
  platform ids with no provider prefix — collisions avoided only by
  accident of id formats (Slack `C…`, Discord numeric, `term-…`). Logical
  channel directories get a real namespace, with provider-tagged streams
  inside for multi-surface channels.
- **run-context.json grows a surface field** (provider + native id) beside
  the existing channel/user attribution. Old records stay readable: a
  record with no surface field is a singleton-era record whose channel
  *is* its surface.

## Sequencing sketch

Each stage lands alone and leaves the system coherent:

1. **Provenance rename** (decision 14–15): re-home the predicate and scan
   into the host-repo path, add per-connection `write_from`, update docs.
   Pure reframe — no behavior change beyond scan-skip and per-connection
   policy.
2. **Scope classes** (decisions 1–4): registry vocabulary, grant parsing,
   listener admission, gateway consultation of the type map.
3. **The channel entity** (decisions 5–7, 13): singleton channels made
   explicit, `channel link`, re-keying of history/purpose/trust/sessions,
   merge-on-link.
4. **Channel stores** (decisions 9–11): mount, snapshot coverage,
   provisioning rule, briefing update.
5. **Reply routing** (decision 8): per-turn reply directive in the
   briefing.

## Open questions

- **Naming a linked channel.** `channel link` needs an id for the merged
  channel (`--name manas`?) and a rule for which member's purpose seeds
  the merged one (operator picks; worker may propose a rewrite).
- **What the briefing tells the worker about surfaces.** The worker sees
  one conversation, but turns arrive dialect-tagged; how much of the
  surface structure the briefing exposes (vs. hides behind the per-turn
  reply directive) needs a pass over the context builder.
- **Class of a never-seen surface at admission time.** Decision 4 fails
  closed at the gateway; the listener side needs the same rule stated for
  surfaces created mid-session (a brand-new private thread, a first-time
  DM).

## Docs impact (when implemented)

- [channels.md](../channels.md) — surface/channel vocabulary, classes,
  linking, reply routing, the channel store mount.
- [connections.md](../connections.md) — class vocabulary in "Scoping a
  grant"; enforcement note per decision 4.
- [store.md](../store.md) / [memory.md](../memory.md) — the two-layer
  store; memory explicitly staying global; "taint rule" references become
  run provenance.
- [repos.md](../repos.md) — owns the clean-room rule end to end (rename
  from "taint"), per-connection `write_from`, scan scoping.
- [security.md](../security.md) — provenance as the core record;
  clean-room as a host-repo contract over it.
