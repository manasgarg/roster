# Knowledge

What a worker learns about the world lives in a private **git repository** —
one per worker, owned by the trusted side. Every writable run works in a
real clone on its own branch and lands changes through a gated push; the
host validates each push and advances `main` **fast-forward only**. A box
can never write a canonical ref, so history cannot be rewritten from inside
a box, and `git log main` is the audit trail of how the worker's knowledge
grew — authored, commit by commit, in the worker's own name.

Knowledge is world-space: vendor facts, paper summaries, research records.
It is deliberately separate from [interaction memory](memory.md)
(person-space), and the border between the two is enforced by provenance —
see *The boundary* below.

## The repository

`roster worker init` creates a bare repo at
`data/workers/<name>/knowledge/repo.git`. The layout is **the worker's
own**: there are no reserved directories, no filename conventions, no
special files — the worker adds, edits, moves, and prunes as its knowledge
deserves, and is told to organize the repository the way it would want to
find things again.

It's ordinary git. Inspect it any time:

```bash
git -C "$(roster worker knowledge yuko)" log --oneline
git clone "$(roster worker knowledge yuko)" /tmp/yuko-knowledge
```

## How runs write it

Each run gets a **real clone** (`.git` included) mounted at
`/opt/roster/knowledge`, checked out on a branch named `run/<run-id>`, with
the canonical bare repo bind-mounted **read-only** at
`/opt/roster/knowledge.git` as `origin` — live and fetchable, but a ref
write from the box is a filesystem error. The worker commits as it goes and
lands its branch with the `knowledge_push` tool:

1. The box tool bundles `origin/main..HEAD` (a git bundle is an inert pack —
   the host never runs git against box-written repository state, because a
   box-written `.git/config` is an execution vector) and proposes the
   `knowledge-push` action with the branch head.
2. The host receives the bundle into a quarantine clone, `fsck`s it, and
   validates the pushed range: regular non-executable files only, size
   limits, no secret-looking content, no conversation participants, and a
   deletion count against `max_deletions_ungated` — a bulk wipe waits for a
   human gate (`confirm_bulk_delete`), everything else lands unattended.
3. In the worker's serialized integration lane, `main` advances to the
   pushed head — fast-forward only, compare-and-swap on the old value.

If `main` moved first (another run landed), the push is refused with
`stale: main is now <sha>` — the worker runs `git fetch origin && git
rebase origin/main`, resolves any conflicts itself, and pushes again. All
divergence is resolved in the box, by the agent, with full context; the
host never merges content. The refusal arrives as the action result while
the run is alive to react — a push is never silently dropped.

**The backstop.** A run that ends with unpushed work — crash, ceiling,
or it simply forgot — has its worktree snapshotted host-side (by hashing
files, never by reading the box's `.git`) onto `refs/quarantine/run-<id>`
in the canonical repo. The next run's briefing points at it; parked refs
expire after 14 days. Landing beats parking: exits never integrate, only
pushes do.

`roster server runs show <run>` displays a run's knowledge access, base
commit, push state, and landed commit.

## Granting the push

`knowledge_push` is an ordinary [action](actions-and-trust.md): bind it in
`org.toml` and it flows through the same propose → trust → execute (or
gate) machinery as everything else.

```toml
[[action]]
name     = "knowledge-push"
executor = "knowledge"
trust    = "auto"            # routine pushes land unattended

[[trust]]
intent = "knowledge-push"
match  = { confirm_bulk_delete = "yes" }
level  = "gate"              # bulk deletions wait for a human
```

An over-limit deletion is refused with instructions to re-propose with
`confirm_bulk_delete = "yes"` — and that payload shape always gates, so the
speed bump cannot be talked around: an unconfirmed wipe is refused, a
confirmed one waits at the desk.

## The boundary: person-space never leaks into world-space

Memory and knowledge have different governance for good reason, and the
leak is asymmetric: knowledge read in a conversation is harmless;
conversation content written into knowledge is a consent bypass into a
store that is deliberately hard to erase. No text filter closes that —
paraphrase defeats every scanner. So the boundary is an information-flow
property, enforced by provisioning:

**Knowledge may only be pushed by runs that never contained person-data.**

Every run is either **tainted** — it carries interaction content: channel
sessions, relay tasks, continuations with channel context, anything with
memory recall — or **clean**: trigger-fired, admin-filed, self-filed, or
ad-hoc, with a prompt and no participants.

| Run | Knowledge clone | Memory recall |
|---|---|---|
| tainted | read-only, push refused | yes |
| clean | writable, push granted | none |

The model never chooses its privilege; the run's provenance does. The
enforcement point is the ref write — `knowledge_push` refuses a tainted
run — backed by the read-only mount. Reading is fine — world→person is
the safe direction.

**The bridge.** A conversation that surfaces something worth durable
research files a task (`file_task`) instead of writing knowledge. The filed
task is worker-only by construction, so it runs clean with a writable
clone. That makes the task prompt the entire residual leak surface — one
paragraph per crossing, journaled and scannable — instead of a wide border
of bulk file writes.

**The scan.** The host knows exactly who was in the filing run, so it
checks `file_task` prompts from tainted runs (and every changed file at
push time) against those participants: their ids, display names, mention
syntax, email addresses. A hit is denied with a legible reason — "names a
conversation participant; that belongs in memory" — and journaled. Stated
honestly: paraphrase still passes the scan. The scan polices the choke
point; the hard guarantees are the read-only clone and the refused push.

The default policy is `write_from = "clean-room"` in `org.toml
[knowledge]`; `"any-run"` reverts to scan-only behavior for deployments
that accept the tradeoff. Limits (file size, repo size, the deletion gate)
are in [configuration.md](configuration.md); per-worker overlays can
tighten them but never loosen.

## Current limitations

- No owner command for redacting content that made it into `main`'s
  history — that's git surgery by hand today (deletion in a later commit
  removes it from the tree, not from history; that's the audit trail
  working as intended, but it means secrets need surgery).
- No automatic remote mirror or backup of the knowledge repos — they're in
  the data root, which is your backup set.
- Source bytes aren't archived: records cite URLs, which is provenance, not
  proof the source still says what it said.
