// The box runner: one pi session in a locked-down container.
//
// What the box gets: the repo read-only (pi + this code, at the pinned
// versions on disk — it cannot edit its own rules), a writable workspace,
// session dir and throwaway HOME, and proxy env pointing at the gateway as
// its only way out. What it never gets: secrets (.env is shadowed), an
// internet route, or a life beyond the ceiling timeout.
//
// The one accepted exposure: a throwaway copy of the model auth rides in
// .pihome so pi can call its provider. Moves behind the gateway in a later
// increment (docs/roster-handoff.md §9, increment 3).

import { spawn, execFileSync } from "node:child_process";
import { createWriteStream, existsSync, mkdirSync, readFileSync, realpathSync, writeFileSync } from "node:fs";
import { homedir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { ensureLockdown, GATEWAY_PORT, LOCKDOWN_NETWORK } from "./lockdown.ts";

// The Rust gateway owns the CA (at ~/.roster/ca) and creates it on startup.
// The box only mounts the public cert; it never sees the key.
const HOST_CA_CERT = join(homedir(), ".roster", "ca", "ca.crt");

// Where the CA's public cert is mounted inside the box (read-only) and the
// env vars that make every client trust it — so the gateway can terminate
// TLS and see full requests. The CA private key never enters the box.
const BOX_CA_PATH = "/opt/roster/ca.crt";

export interface BoxResult {
  runId: string;
  runDir: string;
  endedBy: "exit" | "ceiling";
  exitCode: number | null;
}

const repoRoot = join(dirname(fileURLToPath(import.meta.url)), "..");

/** pi's real JS entrypoint (the npm .bin shim is a shell script; read the
 * package's bin field instead so the box can invoke it with plain node). */
function resolvePiEntry(): string {
  const pkgDir = realpathSync(join(repoRoot, "node_modules/@earendil-works/pi-coding-agent"));
  const pkg = JSON.parse(readFileSync(join(pkgDir, "package.json"), "utf8")) as {
    bin: string | Record<string, string>;
  };
  return join(pkgDir, typeof pkg.bin === "string" ? pkg.bin : pkg.bin.pi);
}

const SENTINEL = "roster-sentinel-no-real-credential-in-box";
const FAR_FUTURE_MS = Date.now() + 100 * 365 * 24 * 3600 * 1000;

/** A structurally-valid but useless JWT, so pi can decode it (it reads the
 * account id and expiry out of the token) and send it — without it being a
 * real credential. Far-future `exp` so pi never tries its own refresh. The
 * gateway replaces the whole thing in transit anyway. */
function sentinelJwt(): string {
  const nowSec = Math.floor(Date.now() / 1000);
  const b64 = (o: unknown) => Buffer.from(JSON.stringify(o)).toString("base64url");
  const header = { alg: "none", typ: "JWT" };
  const payload = {
    iat: nowSec,
    exp: nowSec + 100 * 365 * 24 * 3600,
    "https://api.openai.com/auth": { chatgpt_account_id: "roster-sentinel-account" },
  };
  return `${b64(header)}.${b64(payload)}.${Buffer.from("roster-sentinel-signature").toString("base64url")}`;
}

/** Replace the secret fields of a real auth entry with sentinels. pi will
 * form a request from these; the gateway swaps the sentinel for the real
 * token in transit (src/gateway.ts). */
function sentinelize(entry: Record<string, unknown>): Record<string, unknown> {
  const out = { ...entry };
  // A JWT access token is decoded by pi (it extracts the account id + expiry),
  // so a JWT-shaped one needs a well-formed fake; others just need a string.
  if (typeof out.access === "string") out.access = out.access.split(".").length === 3 ? sentinelJwt() : SENTINEL;
  if (typeof out.refresh === "string") out.refresh = SENTINEL;
  if (typeof out.accountId === "string") out.accountId = "roster-sentinel-account";
  if (typeof out.expires === "number") out.expires = FAR_FUTURE_MS;
  return out;
}

/** Throwaway per-run HOME. The box gets a SENTINEL auth (real shape, secrets
 * nulled) — never a real credential. The gateway holds the real key and
 * injects it in transit. See docs/injection-spec.md. */
function preparePihome(pihome: string): { hasAuthFile: boolean } {
  const agentDir = join(pihome, "agent");
  mkdirSync(agentDir, { recursive: true });
  const authSrc = join(homedir(), ".pi/agent/auth.json");
  const hasAuthFile = existsSync(authSrc);
  if (hasAuthFile) {
    const real = JSON.parse(readFileSync(authSrc, "utf8")) as Record<string, Record<string, unknown>>;
    const sentinel = Object.fromEntries(Object.entries(real).map(([prov, e]) => [prov, sentinelize(e)]));
    writeFileSync(join(agentDir, "auth.json"), JSON.stringify(sentinel, null, 2) + "\n");
  }
  // Settings are rebuilt, not copied: only the model selection carries over.
  // Anything else from the host (e.g. a "packages" list pi would npm-install
  // at boot — denied by the gateway, and not the box's to decide) stays out.
  const settingsSrc = join(homedir(), ".pi/agent/settings.json");
  const host: Record<string, unknown> = existsSync(settingsSrc)
    ? (JSON.parse(readFileSync(settingsSrc, "utf8")) as Record<string, unknown>)
    : {};
  const settings = Object.fromEntries(
    ["defaultProvider", "defaultModel", "defaultThinkingLevel"]
      .filter((k) => host[k] !== undefined)
      .map((k) => [k, host[k]]),
  );
  writeFileSync(join(agentDir, "settings.json"), JSON.stringify(settings, null, 2) + "\n");
  return { hasAuthFile };
}

export async function runBox(prompt: string, ceilingMinutes: number = 30): Promise<BoxResult> {
  await ensureLockdown(); // throws — never proceeds with open egress
  if (!existsSync(HOST_CA_CERT)) {
    throw new Error(`the gateway CA is not present at ${HOST_CA_CERT} — start the gateway first (it creates the CA)`);
  }
  const caCert = HOST_CA_CERT; // the box trusts it, never sees the key

  const runId = new Date().toISOString().slice(0, 19).replace(/[T:]/g, "-");
  const runDir = join(repoRoot, "runs", runId);
  const workspace = join(runDir, "workspace");
  const sessionDir = join(runDir, "session");
  const pihome = join(runDir, ".pihome");
  for (const d of [workspace, sessionDir]) mkdirSync(d, { recursive: true });

  const { hasAuthFile } = preparePihome(pihome);
  if (!hasAuthFile && !process.env.ANTHROPIC_API_KEY) {
    throw new Error("no model credentials: neither ~/.pi/agent/auth.json nor ANTHROPIC_API_KEY exists");
  }

  const proxyUrl = `http://host.docker.internal:${GATEWAY_PORT}`;
  const containerName = `roster-box-${runId}`;
  const envFile = join(repoRoot, ".env");
  const uid = process.getuid?.() ?? 1000;
  const gid = process.getgid?.() ?? 1000;

  const args = [
    "run", "--rm", "--name", containerName,
    "--add-host=host.docker.internal:host-gateway",
    "--network", LOCKDOWN_NETWORK,
    "-u", `${uid}:${gid}`,
    "-v", `${repoRoot}:${repoRoot}:ro`,
    ...(existsSync(envFile) ? ["-v", `/dev/null:${envFile}:ro`] : []),
    "-v", `${workspace}:${workspace}`,
    "-v", `${sessionDir}:${sessionDir}`,
    "-v", `${pihome}:${pihome}`,
    // the CA public cert (read-only) so the box trusts the gateway's minted certs
    "-v", `${caCert}:${BOX_CA_PATH}:ro`,
    "-e", `HOME=${pihome}`,
    "-e", `PI_CODING_AGENT_DIR=${join(pihome, "agent")}`,
    // every egress client honors these; the gateway is the only door
    ...["HTTP_PROXY", "HTTPS_PROXY", "http_proxy", "https_proxy"].flatMap((k) => ["-e", `${k}=${proxyUrl}`]),
    "-e", "NODE_USE_ENV_PROXY=1",
    "-e", "NO_PROXY=",
    // trust our CA so terminated TLS verifies (node fetch, curl, python requests)
    "-e", `NODE_EXTRA_CA_CERTS=${BOX_CA_PATH}`,
    "-e", `CURL_CA_BUNDLE=${BOX_CA_PATH}`,
    "-e", `REQUESTS_CA_BUNDLE=${BOX_CA_PATH}`,
    ...(!hasAuthFile ? ["-e", `ANTHROPIC_API_KEY=${process.env.ANTHROPIC_API_KEY}`] : []),
    "-w", workspace,
    "roster-box",
    "node", resolvePiEntry(),
    "--mode", "json", "--no-extensions",
    "--session-dir", sessionDir,
    prompt,
  ];

  const child = spawn("docker", args, { stdio: ["ignore", "pipe", "inherit"] });
  child.stdout.pipe(createWriteStream(join(runDir, "stdout.jsonl")));

  let endedBy: BoxResult["endedBy"] = "exit";
  const kill = (reason: BoxResult["endedBy"]) => {
    endedBy = reason;
    try {
      execFileSync("docker", ["kill", containerName], { stdio: "ignore", timeout: 15_000 });
    } catch {
      // already gone — fine, --rm cleans up
    }
  };
  const ceiling = setTimeout(() => kill("ceiling"), ceilingMinutes * 60_000);
  const onSignal = () => kill("exit");
  process.on("SIGINT", onSignal);
  process.on("SIGTERM", onSignal);

  const exitCode = await new Promise<number | null>((resolve) => child.on("close", resolve));
  clearTimeout(ceiling);
  process.off("SIGINT", onSignal);
  process.off("SIGTERM", onSignal);

  return { runId, runDir, endedBy, exitCode };
}
