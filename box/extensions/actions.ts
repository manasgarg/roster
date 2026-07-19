/**
 * roster action tools — how the worker PROPOSES a consequential action.
 *
 * These tools never perform the action. They submit a typed envelope to the
 * gateway (the box's only trusted route), which attributes it to this worker,
 * checks the admin's action grants + trust ladder, and either runs it now (auto)
 * or files a durable gate for a human. So `send_email` doesn't send — it asks;
 * the trusted side sends, holding a credential the box never sees.
 *
 * The response tells the worker exactly what happened: done, or pending a gate
 * (with the gate id, so it can be tracked across runs). See
 * docs/actions-and-trust.md.
 */

const ACTION_URL = "https://actions.roster.internal/submit";
const RUN_ID = process.env.ROSTER_RUN_ID ?? "";
const TASK_ID = process.env.ROSTER_TASK_ID ?? "";

type Submission = { status: string; result?: unknown; gate_id?: string; reason?: string; error?: string; message?: string };

async function submit(intent: string, payload: unknown, rationale: string): Promise<Submission> {
  try {
    const res = await fetch(ACTION_URL, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ intent, payload, rationale, run_id: RUN_ID, task_id: TASK_ID }),
      signal: AbortSignal.timeout(30_000),
    });
    return (await res.json()) as Submission;
  } catch (e) {
    return { status: "error", error: e instanceof Error ? e.message : String(e) };
  }
}

/** Turn the gateway's reply into a clear sentence for the model. */
function describe(s: Submission): string {
  switch (s.status) {
    case "done":
      return `Done. ${JSON.stringify(s.result ?? {})}`;
    case "pending":
      return `Submitted for human approval (gate ${s.gate_id}). It will run once approved — you can stop here; a later run will see the outcome.`;
    case "denied":
      return `Refused by governance: ${s.reason ?? "not permitted"}.`;
    default:
      return `Could not submit: ${s.error ?? "unknown error"}.`;
  }
}

interface PiToolApi {
  registerTool(definition: {
    name: string;
    label: string;
    description: string;
    promptSnippet?: string;
    parameters: unknown;
    execute: (id: string, params: Record<string, unknown>) => Promise<{ content: { type: "text"; text: string }[] }>;
  }): void;
}

export default function rosterActionTools(api: PiToolApi): void {
  api.registerTool({
    name: "message_user",
    label: "message_user",
    description:
      "Send a short note to your lead (status update, question, finding). Delivered to their inbox. " +
      "Use it to report progress or surface something that needs their attention.",
    promptSnippet: "message_user(text): notify your lead",
    parameters: {
      type: "object",
      properties: { text: { type: "string", description: "The message to your lead." } },
      required: ["text"],
      additionalProperties: false,
    },
    async execute(_id, params) {
      const s = await submit("message-user", { text: params.text }, "");
      return { content: [{ type: "text", text: describe(s) }] };
    },
  });

  api.registerTool({
    name: "discord_send",
    label: "discord_send",
    description:
      "Reply in a Discord channel. This does NOT send immediately — it submits the message for governance " +
      "(it may send automatically or wait for approval). Provide the channel id and the message text.",
    promptSnippet: "discord_send(channel_id, text): reply in a Discord channel (governed)",
    parameters: {
      type: "object",
      properties: {
        channel_id: { type: "string", description: "The Discord channel id to post in." },
        text: { type: "string", description: "The message to send." },
      },
      required: ["channel_id", "text"],
      additionalProperties: false,
    },
    async execute(_id, params) {
      const { channel_id, text } = params as { channel_id: string; text: string };
      const s = await submit("discord-send", { channel_id, text }, "");
      return { content: [{ type: "text", text: describe(s) }] };
    },
  });

  api.registerTool({
    name: "slack_send",
    label: "slack_send",
    description:
      "Reply in a Slack channel. This does NOT send immediately — it submits the message for governance " +
      "(it may send automatically or wait for approval). Write in Slack mrkdwn (*bold*, _italic_, <url|label>), " +
      "not Markdown. Provide the channel id and the message text; thread_ts replies inside a thread.",
    promptSnippet: "slack_send(channel_id, text[, thread_ts]): reply in a Slack channel (governed)",
    parameters: {
      type: "object",
      properties: {
        channel_id: { type: "string", description: "The Slack channel id to post in." },
        text: { type: "string", description: "The message to send, in Slack mrkdwn." },
        thread_ts: { type: "string", description: "Optional thread timestamp to reply inside a thread." },
      },
      required: ["channel_id", "text"],
      additionalProperties: false,
    },
    async execute(_id, params) {
      const { channel_id, text, thread_ts } = params as { channel_id: string; text: string; thread_ts?: string };
      const s = await submit("slack-send", { channel_id, text, thread_ts }, "");
      return { content: [{ type: "text", text: describe(s) }] };
    },
  });

  api.registerTool({
    name: "term_send",
    label: "term_send",
    description:
      "Deliver results to a terminal channel (channel ids starting with term-). The message is recorded on that " +
      "channel and shown to the operator the next time they open roster talk. Use this when your task briefing names " +
      "a terminal reply channel.",
    promptSnippet: "term_send(channel_id, text): deliver results to the operator's terminal channel",
    parameters: {
      type: "object",
      properties: {
        channel_id: { type: "string", description: "The terminal channel id from your briefing (term-…)." },
        text: { type: "string", description: "The message to deliver, plain text." },
      },
      required: ["channel_id", "text"],
      additionalProperties: false,
    },
    async execute(_id, params) {
      const { channel_id, text } = params as { channel_id: string; text: string };
      const s = await submit("term-send", { channel_id, text }, "");
      return { content: [{ type: "text", text: describe(s) }] };
    },
  });

  api.registerTool({
    name: "file_task",
    label: "file_task",
    description:
      "Add ONE task to your task partition — durable work for a later clean run (writable knowledge shelf, no conversation in the room). " +
      "Optionally schedule it with \"at\" (RFC3339 UTC). For reshaping the whole list (reorder, cancel, chain, recurring templates), read " +
      "$ROSTER_TASKS_FILE and use set_tasks instead. Describe the WORK, not the people — prompts naming conversation participants are refused.",
    promptSnippet: "file_task(prompt[, ceiling_min, at]): add one task; set_tasks reshapes the whole list",
    parameters: {
      type: "object",
      properties: {
        prompt: { type: "string", description: "What to research or do, self-contained — the future run sees only this text." },
        ceiling_min: { type: "number", description: "Optional wall-clock ceiling in minutes (default 30)." },
        at: { type: "string", description: "Optional earliest start, RFC3339 UTC (\"2026-07-18T09:00:00Z\"). Absent = eligible now." },
      },
      required: ["prompt"],
      additionalProperties: false,
    },
    async execute(_id, params) {
      const { prompt, ceiling_min, at } = params as { prompt: string; ceiling_min?: number; at?: string };
      const s = await submit("file-task", { prompt, ceiling_min, at }, "");
      return { content: [{ type: "text", text: describe(s) }] };
    },
  });

  api.registerTool({
    name: "set_tasks",
    label: "set_tasks",
    description:
      "Replace your task partition — pending tasks and recurring templates — with a reshaped version. Read the current document " +
      "from $ROSTER_TASKS_FILE ($HOME/self/schedule.json) first and send its \"version\" back as base_version. Rules the host enforces: " +
      "claimed/needs-review tasks must be echoed unchanged; system recurring entries (your heartbeat) are host-owned; task states are " +
      "host-attested (new entries are pending); dependencies must stay acyclic. On a version conflict the call fails with the current " +
      "version — re-read the file and retry.",
    promptSnippet: "set_tasks(base_version, tasks[, recurring]): reshape your task list (read $ROSTER_TASKS_FILE first)",
    parameters: {
      type: "object",
      properties: {
        base_version: { type: "number", description: "The \"version\" field from the tasks file you just read." },
        tasks: { type: "array", description: "The complete replacement task list. Echo claimed/needs-review entries unchanged; omit an entry to cancel it; leave \"id\" empty on new entries.", items: { type: "object" } },
        recurring: { type: "array", description: "The complete replacement recurring-template list (5-field cron schedules, host-local). Echo system entries unchanged.", items: { type: "object" } },
      },
      required: ["base_version", "tasks"],
      additionalProperties: false,
    },
    async execute(_id, params) {
      const { base_version, tasks, recurring } = params as { base_version: number; tasks: unknown[]; recurring?: unknown[] };
      const s = await submit("set-tasks", { base_version, tasks, recurring: recurring ?? [] }, "");
      return { content: [{ type: "text", text: describe(s) }] };
    },
  });

  api.registerTool({
    name: "file_update",
    label: "file_update",
    description:
      "Edit one of your own editable files (currently: config/worker.toml). Read the current file under $HOME/self/ first, " +
      "compute its sha256, and send it as base_hash with the FULL new content — a stale hash means someone changed the file " +
      "since you read it (re-read and retry). The host validates the whole config after the write and reverts an edit that " +
      "breaks it. identity.md goes through the identity action (gated); the schedule goes through set_tasks.",
    promptSnippet: "file_update(path, base_hash, content): check-and-set edit of an editable self/ file",
    parameters: {
      type: "object",
      properties: {
        path: { type: "string", description: "The file as listed under $HOME/self/, e.g. \"config/worker.toml\"." },
        base_hash: { type: "string", description: "sha256 (hex) of the exact bytes you read from $HOME/self/<path>." },
        content: { type: "string", description: "The complete replacement file content." },
      },
      required: ["path", "base_hash", "content"],
      additionalProperties: false,
    },
    async execute(_id, params) {
      const { path, base_hash, content } = params as { path: string; base_hash: string; content: string };
      const s = await submit("file-update", { path, base_hash, content }, "");
      return { content: [{ type: "text", text: describe(s) }] };
    },
  });

  api.registerTool({
    name: "check_gates",
    label: "check_gates",
    description:
      "List the actions you've proposed and their current state (pending approval, executed, denied). " +
      "Use it to avoid re-proposing something already awaiting approval, or to see how a past proposal resolved.",
    promptSnippet: "check_gates(): your proposed actions and their state",
    parameters: { type: "object", properties: {}, additionalProperties: false },
    async execute() {
      try {
        const res = await fetch("https://actions.roster.internal/gates", { signal: AbortSignal.timeout(15_000) });
        const { gates } = (await res.json()) as { gates: unknown[] };
        const text = !gates?.length ? "No proposed actions yet." : gates.map((g) => JSON.stringify(g)).join("\n");
        return { content: [{ type: "text", text }] };
      } catch (e) {
        return { content: [{ type: "text", text: `Could not read gate state: ${e instanceof Error ? e.message : String(e)}` }] };
      }
    },
  });

  // Outcome reporting — part of the task protocol, so only task runs get the
  // tools. The report is evidence for the host's attestation, not the
  // attestation itself: a crash or refused-and-silent run is failed no matter
  // what was claimed.
  if (TASK_ID) {
    const reportOutcome = async (status: "completed" | "failed", note?: string): Promise<string> => {
      try {
        const res = await fetch("https://actions.roster.internal/outcome", {
          method: "POST",
          headers: { "content-type": "application/json" },
          body: JSON.stringify({ status, note: note ?? "" }),
          signal: AbortSignal.timeout(15_000),
        });
        const s = (await res.json()) as Submission;
        return s.status === "done"
          ? `Outcome recorded: ${status}. You can wrap up and exit now.`
          : `Could not record the outcome: ${s.error ?? "unknown error"}.`;
      } catch (e) {
        return `Could not record the outcome: ${e instanceof Error ? e.message : String(e)}.`;
      }
    };

    api.registerTool({
      name: "task_complete",
      label: "task_complete",
      description:
        "Report that THIS task's work is done — call it once, just before you exit. The host attests the final state; " +
        "a run that ends without reporting, after refused calls, is recorded as failed.",
      promptSnippet: "task_complete([note]): report this task done, right before exiting",
      parameters: {
        type: "object",
        properties: { note: { type: "string", description: "One line on what was accomplished (optional)." } },
        additionalProperties: false,
      },
      async execute(_id, params) {
        return { content: [{ type: "text", text: await reportOutcome("completed", params.note as string | undefined) }] };
      },
    });

    api.registerTool({
      name: "task_fail",
      label: "task_fail",
      description:
        "Report that THIS task could not be done — call it once, just before you exit, with the reason. " +
        "Use it when you're blocked (refused calls, missing access, impossible ask) so the failure is visible with its cause.",
      promptSnippet: "task_fail(reason): report this task failed, right before exiting",
      parameters: {
        type: "object",
        properties: { reason: { type: "string", description: "Why the task could not be completed." } },
        required: ["reason"],
        additionalProperties: false,
      },
      async execute(_id, params) {
        return { content: [{ type: "text", text: await reportOutcome("failed", params.reason as string) }] };
      },
    });
  }

  // repo_push — land a committed branch on a gated repo's shared main.
  // Registered when this run holds at least one writable repo checkout
  // (ROSTER_REPOS_JSON lists every checkout: connection, dir, mode).
  type RepoEntry = { connection: string; dir: string; base: string; branch: string; mode: string };
  const repos: RepoEntry[] = (() => {
    try {
      return JSON.parse(process.env.ROSTER_REPOS_JSON ?? "[]") as RepoEntry[];
    } catch {
      return [];
    }
  })();
  const writableRepos = repos.filter((r) => r.mode === "write");
  if (writableRepos.length > 0) {
    const gitIn = async (dir: string, ...args: string[]): Promise<{ ok: boolean; out: string }> => {
      const { execFile } = await import("node:child_process");
      return new Promise((resolve) => {
        execFile("git", ["-C", dir, ...args], { timeout: 60_000 }, (error, stdout, stderr) => {
          resolve({ ok: !error, out: (error ? `${stdout}\n${stderr}` : stdout).trim() });
        });
      });
    };
    const names = writableRepos.map((r) => r.connection).join(", ");
    api.registerTool({
      name: "repo_push",
      label: "repo_push",
      description:
        `Land your committed changes in a gated repo (${names}) on its shared main branch. Commit your work in the ` +
        "repo's checkout under $HOME/mnt/ first (git add/commit); this bundles your branch and submits it — the host " +
        "validates the push and fast-forwards main. If it answers \"stale: main moved\", run " +
        "`git fetch origin && git rebase origin/main` in that checkout, resolve any conflicts, and call this again. " +
        "A push that deletes many files is refused with instructions to re-propose with " +
        "confirm_bulk_delete — that path waits for your lead's approval.",
      promptSnippet: `repo_push([connection, rationale]): land a committed branch on a gated repo's main (${names})`,
      parameters: {
        type: "object",
        properties: {
          connection: {
            type: "string",
            description: `Which gated repo to push (${names}). Optional when only one is writable.`,
          },
          rationale: { type: "string", description: "One line on what this push adds or changes." },
          confirm_bulk_delete: {
            type: "string",
            enum: ["yes"],
            description: "Pass \"yes\" ONLY when re-proposing a push the host refused for bulk deletion — it will be held for human approval.",
          },
        },
        additionalProperties: false,
      },
      async execute(_id, params) {
        const say = (text: string) => ({ content: [{ type: "text" as const, text }] });
        const chosen = (params.connection as string | undefined) ?? (writableRepos.length === 1 ? writableRepos[0].connection : undefined);
        if (!chosen) return say(`Several repos are writable — pass connection: one of ${names}.`);
        const repo = writableRepos.find((r) => r.connection === chosen);
        if (!repo) return say(`No writable repo "${chosen}" — writable here: ${names}.`);
        const git = (...args: string[]) => gitIn(repo.dir, ...args);
        const dirty = await git("status", "--porcelain");
        if (!dirty.ok) return say(`Could not inspect the ${chosen} checkout: ${dirty.out}`);
        if (dirty.out !== "") return say(`Uncommitted changes in the ${chosen} checkout — git add and commit them first, then push.`);
        const head = await git("rev-parse", "HEAD");
        if (!head.ok) return say(`Could not resolve HEAD: ${head.out}`);
        const range = await git("rev-list", "--count", "origin/main..HEAD");
        if (range.ok && range.out === "0") return say("Nothing to push — your branch has no commits beyond origin/main.");
        const bundle = await git("bundle", "create", ".git/roster-push.bundle", "origin/main..HEAD");
        if (!bundle.ok) return say(`Could not bundle the branch: ${bundle.out}`);
        const s = await submit(
          "repo-push",
          {
            connection: chosen,
            head: head.out,
            ...(params.confirm_bulk_delete === "yes" ? { confirm_bulk_delete: "yes" } : {}),
          },
          (params.rationale as string) ?? "",
        );
        return say(describe(s));
      },
    });
  }

  api.registerTool({
    name: "send_email",
    label: "send_email",
    description:
      "Propose sending an email. This does NOT send immediately — it submits the message for governance. " +
      "Depending on policy it may be sent automatically or held for your lead's approval. Compose the full, final message.",
    promptSnippet: "send_email(to, subject, body): propose an email (may require approval)",
    parameters: {
      type: "object",
      properties: {
        to: { type: "array", items: { type: "string" }, minItems: 1, description: "Recipient email addresses." },
        subject: { type: "string", description: "Subject line." },
        body: { type: "string", description: "The full email body." },
        rationale: { type: "string", description: "Why you're sending this — shown to your lead at the approval step." },
      },
      required: ["to", "subject", "body"],
      additionalProperties: false,
    },
    async execute(_id, params) {
      const { to, subject, body, rationale } = params as { to: string[]; subject: string; body: string; rationale?: string };
      const s = await submit("email-send", { to, subject, body }, rationale ?? "");
      return { content: [{ type: "text", text: describe(s) }] };
    },
  });
}
