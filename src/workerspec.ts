// WorkerSpec (orchestration side): `create` scaffolds a worker spec, `deploy`
// compiles org.toml + workers/<name>/worker.toml into the runtime config the
// gateway reads. Owner authors TOML; deploy validates and provisions. Rules and
// limits are tagged with their scope ("org" or "org/<name>") so the gateway
// applies them by the same ancestor logic the ledger uses. See docs/budget-spec.md.

import { existsSync, mkdirSync, readdirSync, readFileSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { parse as parseToml } from "smol-toml";

const repoRoot = join(dirname(fileURLToPath(import.meta.url)), "..");

type Dict = Record<string, unknown>;

/** Scaffold workers/<name>/worker.toml from a minimal template. */
export function create(name: string): string {
  if (!/^[a-z0-9][a-z0-9-]*$/.test(name)) {
    throw new Error(`worker name must be lowercase letters/numbers/hyphens: "${name}"`);
  }
  const dir = join(repoRoot, "workers", name);
  const path = join(dir, "worker.toml");
  if (existsSync(path)) throw new Error(`worker "${name}" already exists at ${path}`);
  mkdirSync(dir, { recursive: true });
  writeFileSync(
    path,
    `# Worker spec — OWNER-ONLY. Overlays org.toml at scope "org/${name}".
name = "${name}"

[engine]
image = "roster-box"
ceiling_minutes = 30

# This worker's own caps (scope "org/${name}"). Grants inherited from org.toml.
[[budget.limit]]
currency = "model_calls"
window = "day"
max = 5000
`,
  );
  return path;
}

/** Compile org.toml + all worker specs into runs/compiled/{policy,budget}.json. */
export function deploy(): { workers: string[]; rules: number; limits: number } {
  const org = readToml(join(repoRoot, "org.toml"));

  const rules: Dict[] = [];
  const limits: Dict[] = [];

  // Org-scope grants + limits.
  for (const g of asArray(org.grant)) rules.push({ ...(g as Dict), scope: "org" });
  const orgBudget = (org.budget as Dict | undefined) ?? {};
  for (const l of asArray(orgBudget.limit)) limits.push({ ...(l as Dict), scope: "org" });

  // Per-worker overlays.
  const workersDir = join(repoRoot, "workers");
  const workers: string[] = [];
  if (existsSync(workersDir)) {
    for (const name of readdirSync(workersDir).sort()) {
      const path = join(workersDir, name, "worker.toml");
      if (!existsSync(path)) continue;
      const w = readToml(path);
      if (w.name !== name) throw new Error(`${path}: name "${String(w.name)}" != folder "${name}"`);
      const scope = `org/${name}`;
      workers.push(name);
      for (const g of asArray(w.grant)) rules.push({ ...(g as Dict), scope });
      const wBudget = (w.budget as Dict | undefined) ?? {};
      for (const l of asArray(wBudget.limit)) limits.push({ ...(l as Dict), scope });
    }
  }

  validate(rules, limits);

  const policy = { rules };
  const budget = {
    scope: "org",
    currencies: (orgBudget.currencies as unknown) ?? [],
    vars: (orgBudget.vars as unknown) ?? {},
    meters: asArray(orgBudget.meter),
    limits,
  };

  const outDir = join(repoRoot, "runs", "compiled");
  mkdirSync(outDir, { recursive: true });
  writeFileSync(join(outDir, "policy.json"), JSON.stringify(policy, null, 2) + "\n");
  writeFileSync(join(outDir, "budget.json"), JSON.stringify(budget, null, 2) + "\n");

  return { workers, rules: rules.length, limits: limits.length };
}

function readToml(path: string): Dict {
  if (!existsSync(path)) return {};
  return parseToml(readFileSync(path, "utf8")) as Dict;
}

function asArray(v: unknown): unknown[] {
  return Array.isArray(v) ? v : v === undefined ? [] : [v];
}

function validate(rules: Dict[], limits: Dict[]): void {
  for (const r of rules) {
    if (typeof r.name !== "string" || typeof r.verdict !== "string") {
      throw new Error(`grant needs a name and verdict: ${JSON.stringify(r)}`);
    }
  }
  for (const l of limits) {
    if (typeof l.currency !== "string" || typeof l.window !== "string" || typeof l.max !== "number") {
      throw new Error(`limit needs currency, window, and numeric max: ${JSON.stringify(l)}`);
    }
  }
}
