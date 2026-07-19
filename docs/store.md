# The store

Every worker has one durable directory: `data/workers/<name>/store/`,
bind-mounted read-write at `$HOME/store` in **every** run — chat session,
queued task, CLI run alike. It is the worker's own space: no reserved
layout, no filename rules, no review gate. Notes, records, working files,
project directories, whole git repositories — the worker organizes it the
way it wants to find things again, and the briefing tells it so.

The store is an ordinary [connection](connections.md) under the hood — an
auto-provisioned rw host-dir grant — which is why it appears in
`roster connection ls` alongside everything else.

## Interaction memory lives here

`roster migrate` seeds `store/memory/` with a copy of the worker's
`memory.jsonl`. From there the memory practice belongs to the worker: the
briefing teaches it to consult its memory when someone rings familiar,
record what deserves keeping, and carry person-facts with discretion —
what someone says in a private conversation isn't material for another
room. The store mounts read-write in tainted and clean runs alike; unlike
[gated repos](repos.md), cross-channel discretion here is the worker's
conduct, not a storage boundary.

## The host's side of the bargain

The host treats the store as **inert bytes** — a standing rule, not a
habit: rsync it, list it, back it up; never run git in it, never parse
it, never execute from it. A box-written `.git/config` or hook is an
execution vector, and this rule is the whole defense.

**Snapshots.** After every run that changed the store (and on a daily
sweep), the host takes an rsync snapshot into
`data/workers/<name>/store-snapshots/`, hardlinked against the previous
one so N snapshots cost roughly one full copy plus deltas. The last 14
are kept (`[storage] store.snapshots` in org.toml; `0` disables). The
snapshot pass holds the store lock, so a git repo inside is never
captured mid-write, and it records an itemized change list in the run
dir (`store-changes.txt`) — "what did run X change?" stays answerable.

Snapshots protect against **bad runs, not disk loss** — they live on the
same disk, inside the data root. Disk loss remains what your backup of
`data/` is for.

**Restore** is a real command, and it is always undoable — the current
state is snapshotted before it is overwritten:

```bash
roster worker restore yuko --list          # available snapshots
roster worker restore yuko                 # newest
roster worker restore yuko --from 20260719-104432.512
```

## Concurrency between instances

Several runs of one worker can hold the store read-write at once.
Coordination is `flock(2)` on files under `store/.locks/` — bind mounts
share the inode, so a lock taken in one box excludes every other box and
the host, and the kernel releases it if the holder dies. The box ships
`roster-lock`:

```bash
roster-lock notes -- python3 update_notes.py
```

runs the command while holding the named lock; the host's snapshot pass
takes the whole-store lock (`store`) for the duration of its copy. For
git repositories in the store no lock is needed if they are kept **bare**
and cloned into `workspace/` to work — concurrent pushes to a bare repo
are atomic ref transactions, and the loser rebases and retries. (Two runs
sharing one checkout is how repos corrupt; the briefing says so.)

The locks are advisory — a run that skips them can tear a shared file.
That is an accepted failure mode: contention is rare, and the recovery is
a restore, not data loss.

## Other granted directories

The store is just the built-in instance of a **host-dir connection**; the
operator can grant more ([connections.md](connections.md)). One
difference matters: roster snapshots only the store. Granting `rw` on any
other directory warns at config load — no gate, no snapshots; a bad run's
writes there are unrecoverable by roster.
