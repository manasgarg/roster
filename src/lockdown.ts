// Egress lockdown: the network half of the box's cage.
//
// A Docker bridge with IP masquerade (NAT) disabled. Containers on it can
// reach the host (L2, not NAT) — that's how the box reaches the gateway —
// but packets to the internet leave with a private source address replies
// can't route back to, so outbound connections never complete.
//
// Fail closed (NanoClaw's pattern): if the network can't be ensured or the
// gateway isn't answering, throw. Never spawn a box with open egress.

import { execFileSync } from "node:child_process";

export const LOCKDOWN_NETWORK = "roster-locked";
export const GATEWAY_PORT = 7300;

export class LockdownError extends Error {
  constructor(reason: string) {
    super(`refusing to start the box with open egress: ${reason}`);
    this.name = "LockdownError";
  }
}

function dockerOk(args: string[]): boolean {
  try {
    execFileSync("docker", args, { stdio: "ignore", timeout: 15_000 });
    return true;
  } catch {
    return false;
  }
}

/** Ensure the NAT-disabled bridge exists and the gateway answers. Throws
 * LockdownError otherwise — the caller must not fall back to open egress. */
export async function ensureLockdown(): Promise<void> {
  if (
    !dockerOk(["network", "inspect", LOCKDOWN_NETWORK]) &&
    !dockerOk(["network", "create", "-o", "com.docker.network.bridge.enable_ip_masquerade=false", LOCKDOWN_NETWORK])
  ) {
    throw new LockdownError(`the "${LOCKDOWN_NETWORK}" docker network could not be created`);
  }
  try {
    const res = await fetch(`http://127.0.0.1:${GATEWAY_PORT}/healthz`, { signal: AbortSignal.timeout(2000) });
    if (!res.ok) throw new Error(`healthz answered ${res.status}`);
  } catch {
    throw new LockdownError(
      `the gateway is not answering on :${GATEWAY_PORT} — start it with: node src/gateway.ts`,
    );
  }
}
