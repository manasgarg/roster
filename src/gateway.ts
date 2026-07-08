// The seed gateway: the box's only door to the internet.
//
// A forward proxy on node:http/node:net, zero deps. CONNECT to an
// allowlisted model host on port 443 is tunneled; every other request —
// CONNECT to anything else, any plain-HTTP request — is refused with 403.
// Every decision is appended as one JSON line to runs/gateway.jsonl.
//
// This grows into the real gateway (judge consultation, key injection,
// /v1/* endpoints). What must not change as it grows: default-deny, and a
// written record for every answer.

import { createServer, type IncomingMessage, type ServerResponse } from "node:http";
import { connect as netConnect, type Socket } from "node:net";
import { appendFileSync, mkdirSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { GATEWAY_PORT } from "./lockdown.ts";

// The hosts pi's model providers call, for the credentials present on this
// machine (anthropic, openai-codex). Hardcoded until the judge exists.
const ALLOWED_HOSTS = new Set(["api.anthropic.com", "chatgpt.com"]);

const repoRoot = join(dirname(fileURLToPath(import.meta.url)), "..");
const logPath = join(repoRoot, "runs", "gateway.jsonl");

function record(method: string, host: string, verdict: "allow" | "deny"): void {
  const line = JSON.stringify({ ts: new Date().toISOString(), method, host, verdict });
  appendFileSync(logPath, line + "\n");
  console.log(line);
}

const server = createServer((req: IncomingMessage, res: ServerResponse) => {
  // Health check for the runner's fail-closed probe. Not proxied traffic:
  // proxy requests arrive with absolute-form URLs, this one is origin-form.
  if (req.url === "/healthz") {
    res.writeHead(200, { "content-type": "application/json" });
    res.end(JSON.stringify({ ok: true }));
    return;
  }
  // Plain HTTP through the proxy — always refused (the box has no business
  // speaking cleartext HTTP to anyone).
  const host = req.headers.host ?? new URL(req.url ?? "", "http://unknown").host;
  record(req.method ?? "?", host, "deny");
  res.writeHead(403, { "content-type": "application/json" });
  res.end(JSON.stringify({ error: "denied by gateway (default-deny)" }));
});

// CONNECT: the TLS tunnel path every HTTPS client uses via HTTP(S)_PROXY.
server.on("connect", (req: IncomingMessage, clientSocket: Socket, head: Buffer) => {
  const [host = "", portStr] = (req.url ?? "").split(":");
  const port = Number(portStr ?? "443");
  if (!ALLOWED_HOSTS.has(host) || port !== 443) {
    record("CONNECT", `${host}:${port}`, "deny");
    clientSocket.end("HTTP/1.1 403 Forbidden\r\n\r\n");
    return;
  }
  record("CONNECT", host, "allow");
  const upstream = netConnect(port, host, () => {
    clientSocket.write("HTTP/1.1 200 Connection Established\r\n\r\n");
    if (head.length > 0) upstream.write(head);
    upstream.pipe(clientSocket);
    clientSocket.pipe(upstream);
  });
  upstream.on("error", () => clientSocket.destroy());
  clientSocket.on("error", () => upstream.destroy());
});

mkdirSync(dirname(logPath), { recursive: true });
// 0.0.0.0, not 127.0.0.1: the box reaches us over the docker bridge.
server.listen(GATEWAY_PORT, "0.0.0.0", () => {
  console.log(`gateway listening on 0.0.0.0:${GATEWAY_PORT}, allowing ${[...ALLOWED_HOSTS].join(", ")}`);
});

for (const sig of ["SIGINT", "SIGTERM"] as const) {
  process.on(sig, () => {
    server.close();
    process.exit(0);
  });
}
