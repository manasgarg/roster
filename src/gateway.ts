// The inspecting gateway: the box's only door, now with a brain.
//
// It terminates TLS (presenting per-host certs minted by our CA), so it sees
// the full request — method, path, headers, body, and any MCP tool call
// inside. It asks the judge (policies/gateway.json, reloaded per request so
// owner edits are live), records the decision with a permanent id, and only
// on `allow` re-originates the request to the real host. Default-deny; a
// broken policy denies everything. The `tunnel` verdict is the escape hatch
// for cert-pinning clients — decided at CONNECT, raw-piped, host-only.
//
// Zero npm deps. See docs/judge-spec.md.

import { createServer as httpServer, request as httpRequest, type IncomingMessage, type ServerResponse } from "node:http";
import { createServer as httpsServer, request as httpsRequest } from "node:https";
import { connect as netConnect, type Socket } from "node:net";
import { appendFileSync, mkdirSync, readFileSync } from "node:fs";
import { randomUUID } from "node:crypto";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { GATEWAY_PORT } from "./lockdown.ts";
import { certPemForHost, contextForHost, ensureCA, leafKeyPem } from "./ca.ts";
import { judge } from "./judge.ts";
import { getFreshCredential, type Credential } from "./vault.ts";
import type { GovernedRequest, Policy } from "./schema.ts";

const repoRoot = join(dirname(fileURLToPath(import.meta.url)), "..");
const POLICY_PATH = join(repoRoot, "policies", "gateway.json");
const DECISIONS_PATH = join(repoRoot, "runs", "decisions.jsonl");
const MAX_BODY = 50 * 1024 * 1024;
const SENSITIVE = new Set(["authorization", "cookie", "set-cookie", "x-api-key", "proxy-authorization"]);

/** Read the policy fresh each decision so owner edits are live. Fail closed:
 * an unparseable policy denies everything (empty rule list). */
function loadPolicy(): Policy {
  try {
    const p = JSON.parse(readFileSync(POLICY_PATH, "utf8")) as Policy;
    if (!Array.isArray(p.rules)) throw new Error("policy has no rules array");
    return p;
  } catch (err) {
    console.error(`gateway: policy unreadable, denying all — ${err instanceof Error ? err.message : err}`);
    return { rules: [] };
  }
}

function redact(headers: Record<string, string>): Record<string, string> {
  const out: Record<string, string> = {};
  for (const [k, v] of Object.entries(headers)) out[k] = SENSITIVE.has(k) ? "<redacted>" : v;
  return out;
}

function normalizeHeaders(raw: IncomingMessage["headers"]): Record<string, string> {
  const out: Record<string, string> = {};
  for (const [k, v] of Object.entries(raw)) out[k.toLowerCase()] = Array.isArray(v) ? v.join(", ") : (v ?? "");
  return out;
}

/** Lift MCP's own terms from a JSON-RPC body, if that's what this is. */
function liftMcp(headers: Record<string, string>, body: Buffer): GovernedRequest["mcp"] {
  if (body.length === 0 || !(headers["content-type"] ?? "").includes("json")) return null;
  try {
    const parsed = JSON.parse(body.toString("utf8")) as unknown;
    const msg = (Array.isArray(parsed) ? parsed[0] : parsed) as { jsonrpc?: string; method?: unknown; params?: { name?: unknown } };
    if (typeof msg?.method !== "string") return null;
    if (msg.jsonrpc !== "2.0" && !msg.method.includes("/")) return null; // JSON-RPC or an MCP-shaped method
    const mcp: NonNullable<GovernedRequest["mcp"]> = { method: msg.method };
    if (msg.method === "tools/call" && typeof msg.params?.name === "string") mcp.tool = msg.params.name;
    return mcp;
  } catch {
    return null;
  }
}

function record(req: GovernedRequest, verdict: string, rule: string | null, injected?: string[]): void {
  const decision = { decision_id: randomUUID(), ts: new Date().toISOString(), verdict, rule, request: { ...req, headers: redact(req.headers) }, ...(injected ? { injected } : {}) };
  appendFileSync(DECISIONS_PATH, JSON.stringify(decision) + "\n");
  console.log(`${verdict} ${req.method} ${req.host}${req.path} ${rule ?? "(no rule)"}${injected ? ` +inject:${injected.join(",")}` : ""}${req.mcp ? ` mcp:${req.mcp.method}${req.mcp.tool ? `/${req.mcp.tool}` : ""}` : ""}`);
}

/** Turn a vault credential into the auth headers to inject. OAuth today. */
function renderInjection(cred: Credential): Record<string, string> {
  const h: Record<string, string> = {};
  if (cred.type === "oauth" && cred.access) {
    h["authorization"] = `Bearer ${cred.access}`;
    if (cred.accountId) h["chatgpt-account-id"] = cred.accountId;
  }
  return h;
}

function collectBody(req: IncomingMessage, done: (body: Buffer, tooBig: boolean) => void): void {
  const chunks: Buffer[] = [];
  let size = 0;
  let aborted = false;
  req.on("data", (c: Buffer) => {
    size += c.length;
    if (size > MAX_BODY) {
      aborted = true;
      req.destroy();
      done(Buffer.concat(chunks), true);
      return;
    }
    chunks.push(c);
  });
  req.on("end", () => !aborted && done(Buffer.concat(chunks), false));
  req.on("error", () => !aborted && done(Buffer.concat(chunks), false));
}

/** Terminated request → judge → forward-on-allow. Used for both the
 * TLS-terminated (https) path and absolute-form http proxy requests. */
function handleDecrypted(protocol: "http" | "https", req: IncomingMessage, res: ServerResponse): void {
  let host: string, port: number, path: string, query: string;
  if (protocol === "https") {
    host = (req.headers.host ?? "").split(":")[0];
    port = 443;
    const u = new URL(req.url ?? "/", `https://${host}`);
    path = u.pathname;
    query = u.search.replace(/^\?/, "");
  } else {
    const u = new URL(req.url ?? "");
    host = u.hostname;
    port = Number(u.port || 80);
    path = u.pathname;
    query = u.search.replace(/^\?/, "");
  }
  const headers = normalizeHeaders(req.headers);

  collectBody(req, async (body, tooBig) => {
    const gr: GovernedRequest = {
      worker: null, protocol, method: req.method ?? "GET", host, port, path, query, headers,
      bodySize: body.length, mcp: liftMcp(headers, body),
    };
    if (tooBig) {
      record(gr, "deny", null);
      res.writeHead(413, { "content-type": "application/json" });
      res.end(JSON.stringify({ error: "payload too large" }));
      return;
    }
    const policy = loadPolicy();
    const { verdict, rule } = judge(gr, policy);

    // Injection: if the deciding rule injects a credential, render it now so
    // we can fail closed (deny) when the vault lacks it — never forward the
    // box's sentinel to the real host.
    let inject: Record<string, string> | null = null;
    let injectedNames: string[] | undefined;
    if (verdict === "allow" && rule) {
      const ruleObj = policy.rules.find((r) => r.name === rule);
      if (ruleObj?.inject) {
        const credName = ruleObj.inject.credential;
        let cred: Credential | null;
        try {
          // Refreshes the OAuth token first if it has expired; throws if the
          // refresh fails — in which case we deny rather than inject a stale key.
          cred = await getFreshCredential(credName);
        } catch (err) {
          record(gr, "deny", rule);
          res.writeHead(403, { "content-type": "application/json" });
          res.end(JSON.stringify({ error: `credential "${credName}" refresh failed`, detail: String(err instanceof Error ? err.message : err), rule }));
          return;
        }
        if (cred === null) {
          record(gr, "deny", rule);
          res.writeHead(403, { "content-type": "application/json" });
          res.end(JSON.stringify({ error: `credential "${credName}" not in vault`, rule }));
          return;
        }
        inject = renderInjection(cred);
        injectedNames = Object.keys(inject);
      }
    }

    record(gr, verdict, rule, injectedNames);
    if (verdict !== "allow") {
      res.writeHead(403, { "content-type": "application/json" });
      res.end(JSON.stringify({ error: `denied by gateway (${verdict})`, rule }));
      return;
    }
    forward(protocol, host, port, req.method ?? "GET", path + (query ? `?${query}` : ""), headers, body, res, inject);
  });
}

function forward(
  protocol: "http" | "https", host: string, port: number, method: string, target: string,
  headers: Record<string, string>, body: Buffer, res: ServerResponse, inject: Record<string, string> | null,
): void {
  const outHeaders = { ...headers };
  delete outHeaders["proxy-connection"];
  delete outHeaders["content-length"]; // set from the buffered body
  delete outHeaders["transfer-encoding"];
  if (inject) Object.assign(outHeaders, inject); // swap the sentinel for the real credential
  const requester = protocol === "https" ? httpsRequest : httpRequest;
  const upstream = requester({ host, port, method, path: target, headers: outHeaders, servername: host }, (up) => {
    res.writeHead(up.statusCode ?? 502, up.headers);
    up.pipe(res);
  });
  upstream.on("error", (err) => {
    if (!res.headersSent) res.writeHead(502, { "content-type": "application/json" });
    res.end(JSON.stringify({ error: "upstream error", detail: String(err.message) }));
  });
  upstream.end(body);
}

// --- server wiring ---
ensureCA();
mkdirSync(dirname(DECISIONS_PATH), { recursive: true });

// Internal TLS terminator: not bound to a port; CONNECT hands sockets to it.
const tls = httpsServer(
  {
    key: leafKeyPem(),
    cert: certPemForHost("localhost"),
    SNICallback: (name, cb) => {
      try {
        cb(null, contextForHost(name));
      } catch (err) {
        cb(err as Error);
      }
    },
  },
  (req, res) => handleDecrypted("https", req, res),
);
tls.on("clientError", () => {});

const proxy = httpServer((req, res) => {
  if (req.url === "/healthz") {
    res.writeHead(200, { "content-type": "application/json" });
    res.end(JSON.stringify({ ok: true }));
    return;
  }
  if (req.url?.startsWith("http://")) {
    handleDecrypted("http", req, res);
    return;
  }
  res.writeHead(400, { "content-type": "application/json" });
  res.end(JSON.stringify({ error: "not a proxy request" }));
});

// CONNECT: `tunnel` verdict raw-pipes (host-only); everything else is
// terminated and fully judged as a decrypted request above.
proxy.on("connect", (req: IncomingMessage, clientSocket: Socket, head: Buffer) => {
  const [host = "", portStr] = (req.url ?? "").split(":");
  const port = Number(portStr ?? "443");
  const pre: GovernedRequest = {
    worker: null, protocol: "https", method: "CONNECT", host, port, path: "", query: "", headers: {}, bodySize: 0, mcp: null,
  };
  const { verdict, rule } = judge(pre, loadPolicy());
  clientSocket.on("error", () => {});
  if (verdict === "tunnel") {
    record(pre, "tunnel", rule);
    const upstream = netConnect(port, host, () => {
      clientSocket.write("HTTP/1.1 200 Connection Established\r\n\r\n");
      if (head.length > 0) upstream.write(head);
      upstream.pipe(clientSocket);
      clientSocket.pipe(upstream);
    });
    upstream.on("error", () => clientSocket.destroy());
    return;
  }
  // Terminate: hand the socket to the TLS server; it will parse the request
  // and handleDecrypted judges it with full visibility.
  clientSocket.write("HTTP/1.1 200 Connection Established\r\n\r\n");
  if (head.length > 0) clientSocket.unshift(head);
  tls.emit("connection", clientSocket);
});

proxy.listen(GATEWAY_PORT, "0.0.0.0", () => {
  console.log(`gateway listening on 0.0.0.0:${GATEWAY_PORT} — terminating TLS, judging against ${POLICY_PATH}`);
});

for (const sig of ["SIGINT", "SIGTERM"] as const) {
  process.on(sig, () => {
    proxy.close();
    process.exit(0);
  });
}
