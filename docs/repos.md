# Repos: gated host repositories

A **host-repo connection** grants a worker a git repository that lives on
the host. Like every capability, it is added to the roster first and then
made available per worker or org-wide ([connections.md](connections.md)):

```toml
# connections/research-kb.toml
kind    = "host-repo"
path    = "/home/you/research-kb.git"   # a bare repository
write   = "gated"                        # or "ro" (the default)
workers = ["yuko"]
```

A `ro` repo simply mounts read-only at `$HOME/mnt/<name>`. A **gated**
repo is the interesting one: every writable run works in a real clone on
its own branch and lands changes through a validated push; the host
verifies each push and advances `main` **fast-forward only**. A box can
never write a canonical ref, so history cannot be rewritten from inside a
box, and `git log main` is the audit trail — authored, commit by commit,
in the worker's own name.

**The knowledge repo.** Each worker's long-standing knowledge repository
(`data/workers/<name>/knowledge/repo.git`, created by `roster worker
init`) is simply a gated repo under an implicit connection named
`knowledge` — no connection file needed; a file that names a `knowledge`
host-repo connection for the worker takes over. Everything below applies
to it and to any other gated repo alike.

## How runs write a gated repo

Each run gets a **real clone** (`.git` included) at `$HOME/mnt/<name>`,
checked out on a branch named `run/<run-id>`, with the canonical repo
bind-mounted **read-only** at `$HOME/mnt/.origins/<name>` as `origin` —
live and fetchable, but a ref write from the box is a filesystem error.
`ROSTER_REPOS_JSON` in the box environment lists every checkout and its
mode. The worker commits as it goes and lands its branch with the
`repo_push` tool (naming the connection when several are writable):

1. The box tool bundles `origin/main..HEAD` (a git bundle is an inert
   pack — the host never runs git against box-written repository state,
   because a box-written `.git/config` is an execution vector) and
   proposes the `repo-push` action with the branch head.
2. The host receives the bundle into a quarantine clone, `fsck`s it, and
   validates the pushed range: regular non-executable files only, size
   limits, no secret-looking content, no conversation participants, and a
   deletion count against `max_deletions_ungated` — a bulk wipe waits for
   a human gate (`confirm_bulk_delete`), everything else lands unattended.
3. In the repo's serialized integration lane (one lane per repository, no
   matter how many workers push to it), `main` advances to the pushed
   head — fast-forward only, compare-and-swap on the old value.

If `main` moved first (another run landed), the push is refused with
`stale: main is now <sha>` — the worker runs `git fetch origin && git
rebase origin/main`, resolves any conflicts itself, and pushes again. All
divergence is resolved in the box, by the agent, with full context; the
host never merges content. The refusal arrives as the action result while
the run is alive to react — a push is never silently dropped.

**The backstop.** A run that ends with unpushed work — crash, ceiling,
or it simply forgot — has its worktree snapshotted host-side (by hashing
files, never by reading the box's `.git`) onto `refs/quarantine/run-<id>`
in that repo. The next run's briefing points at it; parked refs expire
after 14 days. Landing beats parking: exits never integrate, only pushes
do.

`roster server runs show <run>` displays each repo checkout's mode, base
commit, push state, and landed commit.

## Granting the push

`repo_push` is an ordinary [action](actions-and-trust.md): bind it in
`org.toml` and it flows through the same propose → trust → execute (or
gate) machinery as everything else. (`knowledge-push` remains a valid
grant name for the implicit knowledge repo; `repo-push` covers every
gated repo, with the connection named in the payload.)

```toml
[[action]]
name     = "repo-push"
executor = "knowledge"
trust    = "auto"            # routine pushes land unattended

[[trust]]
intent = "repo-push"
match  = { confirm_bulk_delete = "yes" }
level  = "gate"              # bulk deletions wait for a human
```

An over-limit deletion is refused with instructions to re-propose with
`confirm_bulk_delete = "yes"` — and that payload shape always gates, so the
speed bump cannot be talked around: an unconfirmed wipe is refused, a
confirmed one waits at the desk.

## The boundary: person-space never leaks into world-space

Gated repos and interaction memory have different governance for good
reason, and the leak is asymmetric: repo content read in a conversation
is harmless; conversation content pushed into a shared repo is a consent
bypass into a store that is deliberately hard to erase. No text filter
closes that — paraphrase defeats every scanner. So the boundary is an
information-flow property, enforced by provisioning:

**A gated repo may only be pushed by runs that never contained
person-data.**

Every run is either **tainted** — it carries interaction content: channel
sessions, relay tasks, continuations with channel context, anything with
memory recall — or **clean**: trigger-fired, admin-filed, self-filed, or
ad-hoc, with a prompt and no participants.

| Run | Gated-repo clones | Memory recall |
|---|---|---|
| tainted | read-only, push refused | yes |
| clean | writable, push granted | none |

The model never chooses its privilege; the run's provenance does. The
enforcement point is the ref write — the push refuses a tainted run —
backed by the read-only mount. Reading is fine — world→person is the safe
direction. (The worker's [store](store.md) is deliberately outside this
system: it mounts read-write in every run, and the discretion about what
lands there is the worker's own.)

**The bridge.** A conversation that surfaces something worth durable
research files a task (`file_task`) instead of writing the repo. The filed
task is worker-only by construction, so it runs clean with writable
clones. That makes the task prompt the entire residual leak surface — one
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

- One policy set (`[knowledge]` limits, clean-room rule) applies to every
  gated repo a worker touches — there is no per-connection policy yet.
- Gated connections must point at a **bare** repository; config refuses a
  checkout (advancing a checked-out branch would desync its worktree).
- No owner command for redacting content that made it into `main`'s
  history — that's git surgery by hand today.
- No automatic remote mirror of gated repos — they live where their path
  says; the per-worker knowledge repo sits in the data root, which is your
  backup set.
