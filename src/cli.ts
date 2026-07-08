// Roster CLI entry point.
//
// The lifecycle verbs come from the design (docs/roster-handoff.md, §3.11).
// Each verb gets implemented in its own increment; until then the CLI only
// knows what it will become.

const VERBS: Record<string, string> = {
  hire: "scaffold a new worker from a template",
  deploy: "validate a worker spec and provision its surfaces",
  steer: "send a running worker a steering message",
  suspend: "pause a worker",
  audit: "inspect a worker's journals and decision records",
  retire: "archive a worker (journals stay immutable forever)",
};

function printHelp(): void {
  console.log("roster — digital workers with owned governance");
  console.log("\nusage: node src/cli.ts <verb>\n\nverbs (none implemented yet):");
  for (const [verb, summary] of Object.entries(VERBS)) {
    console.log(`  ${verb.padEnd(8)} ${summary}`);
  }
  console.log("\ndev verbs:");
  console.log('  box      run one pi session in the locked-down container:');
  console.log('           node src/cli.ts box [--ceiling <minutes>] "<prompt>"');
  console.log("           (needs the gateway running: node src/gateway.ts)");
}

const verb = process.argv[2];

if (verb === undefined || verb === "help" || verb === "--help") {
  printHelp();
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
  const prompt = rest.join(" ").trim();
  if (prompt === "") {
    console.error('roster: box needs a prompt: node src/cli.ts box "<prompt>"');
    process.exit(1);
  }
  const { runBox } = await import("./box.ts");
  try {
    const result = await runBox(prompt, ceiling);
    console.log(`box ${result.runId} ended by ${result.endedBy} (exit code ${result.exitCode})`);
    console.log(`outputs: ${result.runDir}`);
    process.exit(result.endedBy === "ceiling" ? 2 : (result.exitCode ?? 1));
  } catch (err) {
    console.error(`roster: ${err instanceof Error ? err.message : String(err)}`);
    process.exit(1);
  }
} else if (verb in VERBS) {
  console.error(`roster: "${verb}" is not implemented yet`);
  process.exit(1);
} else {
  console.error(`roster: unknown verb "${verb}" (try: node src/cli.ts help)`);
  process.exit(1);
}
