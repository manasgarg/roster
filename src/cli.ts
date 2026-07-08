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
}

const verb = process.argv[2];

if (verb === undefined || verb === "help" || verb === "--help") {
  printHelp();
} else if (verb in VERBS) {
  console.error(`roster: "${verb}" is not implemented yet`);
  process.exit(1);
} else {
  console.error(`roster: unknown verb "${verb}" (try: node src/cli.ts help)`);
  process.exit(1);
}
