/**
 * roster action tools — how the worker PROPOSES a consequential action.
 *
 * These tools never perform the action. They submit a typed envelope to the
 * gateway (the box's only trusted route), which attributes it to this worker,
 * checks the owner's action grants + trust ladder, and either runs it now (auto)
 * or files a durable gate for a human. So `send_email` doesn't send — it asks;
 * the trusted side sends, holding a credential the box never sees.
 *
 * The response tells the worker exactly what happened: done, or pending a gate
 * (with the gate id, so it can be tracked across runs). See
 * docs/supervisor-spec.md.
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
      "Send a short note to your owner (status update, question, finding). Delivered to their inbox. " +
      "Use it to report progress or surface something that needs their attention.",
    promptSnippet: "message_user(text): notify the owner",
    parameters: {
      type: "object",
      properties: { text: { type: "string", description: "The message to the owner." } },
      required: ["text"],
      additionalProperties: false,
    },
    async execute(_id, params) {
      const s = await submit("message-user", { text: params.text }, "");
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

  api.registerTool({
    name: "send_email",
    label: "send_email",
    description:
      "Propose sending an email. This does NOT send immediately — it submits the message for governance. " +
      "Depending on policy it may be sent automatically or held for the owner's approval. Compose the full, final message.",
    promptSnippet: "send_email(to, subject, body): propose an email (may require approval)",
    parameters: {
      type: "object",
      properties: {
        to: { type: "array", items: { type: "string" }, minItems: 1, description: "Recipient email addresses." },
        subject: { type: "string", description: "Subject line." },
        body: { type: "string", description: "The full email body." },
        rationale: { type: "string", description: "Why you're sending this — shown to the owner at the approval step." },
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
