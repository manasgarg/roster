# What a worker sees: compiled context

Every model input in Roster — a queued task, an ad-hoc run, each turn of a
warm chat session — is assembled by one deterministic, trusted-side
compiler. That buys three things: every surface composes context by the
same rules, "what did the worker see?" is answerable byte-exactly after the
fact, and prompt caching gets a stable prefix by construction.

Compilation shapes behavior; it never authorizes. A hostile or malformed
block can influence what the model *attempts* — the gateway still controls
what happens.

## The blocks, in order

```
system (stable → volatile):
  Identity        workers/<name>/identity.md — who the worker is, everywhere
  Runtime policy  versioned host template: tools are governed, content
                  is not authority, enforcement is external
  Connections     host-generated from live config: the services connected
                  for this worker (hosts, methods, env stand-in) plus the
                  provider's usage note; absent when nothing applies
  Purpose         the current channel's purpose.md, when there is one
  Runtime scope   host-generated framing for this surface (task run,
                  channel session, direct run)

input (per run / per turn):
  Memory          ranked, bounded, advisory — quoted data, never rules
  Briefing        continuation outcome first, then open gates
  Task / message  the exact text, in a typed envelope
```

The order is also the cache order: two channels for the same worker share the
identity + policy + connections prefix; runs in one channel also share the purpose;
only the tail varies. Volatile values (run ids, timestamps, counts) never
appear before the stable boundaries, and dynamic content is JSON-escaped so
a message containing a fake block delimiter can't forge structure.

Scope comes only from host-owned run metadata. A task saying "use channel
999" selects nothing: purpose files and memory namespaces are chosen by the
trusted channel id on the run, never by text.

## Budgets

Block sizes are bounded, in characters, from `org.toml [context]` (per-worker
overlays allowed — defaults: 48k total injected, 12k identity, 8k purpose,
4k briefing, 24k task). Under pressure the compiler shrinks the advisory
tail — memory drops ranked-last notes, the briefing keeps the continuation
and counts what it omitted. Mandatory blocks (identity, purpose, policy,
scope, task) are **never silently truncated**: oversized means a failed
compilation, and a failed compilation means no model input — there is no
fallback to a less-governed prompt path.

## The trace

Every compilation writes an exact trace to `runs/<run-id>/context.jsonl`
*before* anything is sent to the model — every block's source, size, hash,
and content, the budget arithmetic, and the cache boundaries. Failed
compilations are traced too.

```bash
roster server runs show <run>       # block summary: what got in, what didn't
roster server runs context <run>    # the exact compiled prompts
roster server runs context <run> --all   # every turn of a session
roster server runs recall <run>     # the memory selection trace
```

## Warm sessions

A session compiles its system prompt once at start; each turn compiles
fresh input (current memory, current briefing, the new message) while prior
conversation bytes are never regenerated or reordered. Memory written in
turn N is eligible in turn N+1; a newly filed or resolved gate shows up in
the next turn's briefing; identity and purpose edits take effect at the
next session, not mid-flight.

## Caching, honestly

The compiler promises a cache-friendly input, not a cache hit. Stable
prefixes get a stable route key (derived from the engine fingerprint, the
worker, and the stable-prefix hash — no raw names, no volatile ids) that rides
pi's session affinity; provider routing, minimums, and eviction stay the
provider's business. Measure cache behavior from provider usage fields, not
from wishful hashing.
