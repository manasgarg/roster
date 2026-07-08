// Roster CLI entry point.
//
// Lifecycle verbs use infrastructure vocabulary (a worker is a deployed
// configuration, not an employee). create + deploy are implemented; the rest
// land in later increments.

const VERBS: Record<string, string> = {
  create: "scaffold a new worker spec (workers/<name>/worker.toml)",
  deploy: "validate specs and compile the runtime config the gateway reads",
  suspend: "pause a worker",
  resume: "unpause a worker",
  archive: "decommission a worker (journals stay immutable forever)",
};

const IMPLEMENTED = new Set(["create", "deploy"]);

function printHelp(): void {
  console.log("roster — digital workers with owned governance");
  console.log("\nusage: node src/cli.ts <verb>\n\nlifecycle verbs:");
  for (const [verb, summary] of Object.entries(VERBS)) {
    const tag = IMPLEMENTED.has(verb) ? "" : "  (not implemented yet)";
    console.log(`  ${verb.padEnd(8)} ${summary}${tag}`);
  }
  console.log("\ndev verbs:");
  console.log('  box         run one pi session in the locked-down container:');
  console.log('              node src/cli.ts box [--worker <name>] [--ceiling <minutes>] "<prompt>"');
  console.log("              (--worker sets the budget subject org/<name>, default adhoc;");
  console.log("               needs the gateway running: cd gateway && ROSTER_ROOT=.. cargo run)");
  console.log("  connect     create a credential in the vault via the provider's login flow:");
  console.log("              node src/cli.ts connect <provider>   (e.g. openai-codex, anthropic)");
  console.log("  vault-sync  shortcut: import already-logged-in pi credentials into the vault");
}

const verb = process.argv[2];

if (verb === undefined || verb === "help" || verb === "--help") {
  printHelp();
} else if (verb === "create") {
  const name = process.argv[3];
  if (!name) {
    console.error("roster: create needs a worker name: node src/cli.ts create <name>");
    process.exit(1);
  }
  const { create } = await import("./workerspec.ts");
  try {
    const path = create(name);
    console.log(`created ${path}`);
    console.log("edit it, then run: node src/cli.ts deploy");
  } catch (err) {
    console.error(`roster: ${err instanceof Error ? err.message : String(err)}`);
    process.exit(1);
  }
} else if (verb === "deploy") {
  const { deploy } = await import("./workerspec.ts");
  try {
    const r = deploy();
    console.log(`deployed: ${r.workers.length} worker(s) [${r.workers.join(", ")}], ${r.rules} rule(s), ${r.limits} limit(s)`);
    console.log("compiled → runs/compiled/{policy,budget}.json (the gateway reads these)");
  } catch (err) {
    console.error(`roster: ${err instanceof Error ? err.message : String(err)}`);
    process.exit(1);
  }
} else if (verb === "box") {
  const rest = process.argv.slice(3);
  let ceiling = 30;
  const ci = rest.indexOf("--ceiling");
  if (ci !== -1) {
    ceiling = Number(rest[ci + 1]);
    rest.splice(ci, 2);
    if (!Number.isFinite(ceiling) || ceiling <= 0) {
      console.error("roster: --ceiling wants a positive number of minutes");
      process.exit(1);
    }
  }
  let worker = "adhoc";
  const wi = rest.indexOf("--worker");
  if (wi !== -1) {
    worker = String(rest[wi + 1] ?? "").trim();
    rest.splice(wi, 2);
    if (worker === "") {
      console.error("roster: --worker wants a name");
      process.exit(1);
    }
  }
  const prompt = rest.join(" ").trim();
  if (prompt === "") {
    console.error('roster: box needs a prompt: node src/cli.ts box "<prompt>"');
    process.exit(1);
  }
  const { runBox } = await import("./box.ts");
  try {
    const result = await runBox(prompt, ceiling, worker);
    console.log(`box ${result.runId} ended by ${result.endedBy} (exit code ${result.exitCode})`);
    console.log(`outputs: ${result.runDir}`);
    process.exit(result.endedBy === "ceiling" ? 2 : (result.exitCode ?? 1));
  } catch (err) {
    console.error(`roster: ${err instanceof Error ? err.message : String(err)}`);
    process.exit(1);
  }
} else if (verb === "connect") {
  const name = process.argv[3];
  if (!name) {
    console.error("roster: connect needs a provider: node src/cli.ts connect <provider>");
    process.exit(1);
  }
  const { connect } = await import("./connect.ts");
  try {
    await connect(name);
  } catch (err) {
    console.error(`roster: ${err instanceof Error ? err.message : String(err)}`);
    process.exit(1);
  }
} else if (verb === "vault-sync") {
  const { syncFromPiAuth } = await import("./vault.ts");
  const names = syncFromPiAuth();
  console.log(`vault synced from pi auth: ${names.join(", ")}`);
  console.log("(stored at ~/.roster/vault — off the box mount; the gateway injects these in transit)");
} else if (verb in VERBS) {
  console.error(`roster: "${verb}" is not implemented yet`);
  process.exit(1);
} else {
  console.error(`roster: unknown verb "${verb}" (try: node src/cli.ts help)`);
  process.exit(1);
}
