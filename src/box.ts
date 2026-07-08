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
import { copyFileSync, createWriteStream, existsSync, mkdirSync, readFileSync, realpathSync, writeFileSync } from "node:fs";
import { homedir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { ensureLockdown, GATEWAY_PORT, LOCKDOWN_NETWORK } from "./lockdown.ts";

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

/** Throwaway per-run HOME holding a copy of the model auth — pi can refresh
 * tokens in its copy without ever seeing host session state. */
function preparePihome(pihome: string): { hasAuthFile: boolean } {
  const agentDir = join(pihome, "agent");
  mkdirSync(agentDir, { recursive: true });
  const authSrc = join(homedir(), ".pi/agent/auth.json");
  const hasAuthFile = existsSync(authSrc);
  if (hasAuthFile) copyFileSync(authSrc, join(agentDir, "auth.json"));
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
    "-e", `HOME=${pihome}`,
    "-e", `PI_CODING_AGENT_DIR=${join(pihome, "agent")}`,
    // every egress client honors these; the gateway is the only door
    ...["HTTP_PROXY", "HTTPS_PROXY", "http_proxy", "https_proxy"].flatMap((k) => ["-e", `${k}=${proxyUrl}`]),
    "-e", "NODE_USE_ENV_PROXY=1",
    "-e", "NO_PROXY=",
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
