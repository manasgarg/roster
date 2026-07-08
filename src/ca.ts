// The host-minted CA that lets the gateway terminate TLS and see inside
// requests. The CA private key lives at ~/.roster/ca/ — OUTSIDE the repo,
// so it is never under the box's read-only mount; only ca.crt is exposed to
// the box (as a trust anchor). Per-host leaf certs are minted on demand and
// cached. openssl does the crypto; we hold no key material in JS.
// See docs/judge-spec.md.

import { execFileSync } from "node:child_process";
import { existsSync, mkdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";
import { createSecureContext, type SecureContext } from "node:tls";

const CA_DIR = join(homedir(), ".roster", "ca");
const CA_KEY = join(CA_DIR, "ca.key");
const CA_CRT = join(CA_DIR, "ca.crt");
const LEAF_KEY = join(CA_DIR, "leaf.key");
const HOSTS_DIR = join(CA_DIR, "hosts");

function openssl(args: string[]): void {
  execFileSync("openssl", args, { stdio: "pipe" });
}

/** Create the CA (key + cert) and shared leaf key if absent. Idempotent.
 * Returns the path to the public CA cert (the only part the box may see). */
export function ensureCA(): string {
  mkdirSync(HOSTS_DIR, { recursive: true });
  if (!existsSync(CA_KEY) || !existsSync(CA_CRT)) {
    openssl(["genrsa", "-out", CA_KEY, "2048"]);
    openssl(["req", "-x509", "-new", "-nodes", "-key", CA_KEY, "-sha256", "-days", "3650",
      "-subj", "/CN=Roster Box CA", "-out", CA_CRT]);
  }
  if (!existsSync(LEAF_KEY)) openssl(["genrsa", "-out", LEAF_KEY, "2048"]);
  return CA_CRT;
}

export function caCertPath(): string {
  return CA_CRT;
}

/** Sign a leaf cert for `host` (SAN=host) with the shared leaf key. */
function mintLeaf(host: string, crtOut: string): void {
  const csr = join(HOSTS_DIR, `${host}.csr`);
  const ext = join(HOSTS_DIR, `${host}.ext`);
  writeFileSync(
    ext,
    `subjectAltName=DNS:${host}\nbasicConstraints=CA:FALSE\nkeyUsage=digitalSignature,keyEncipherment\nextendedKeyUsage=serverAuth\n`,
  );
  openssl(["req", "-new", "-key", LEAF_KEY, "-subj", `/CN=${host}`, "-out", csr]);
  openssl(["x509", "-req", "-in", csr, "-CA", CA_CRT, "-CAkey", CA_KEY, "-CAcreateserial",
    "-sha256", "-days", "825", "-extfile", ext, "-out", crtOut]);
  rmSync(csr);
}

/** PEM chain (leaf + CA) the gateway presents for `host`. */
export function certPemForHost(host: string): string {
  ensureCA();
  const crt = join(HOSTS_DIR, `${host}.crt`);
  if (!existsSync(crt)) mintLeaf(host, crt);
  return readFileSync(crt, "utf8") + readFileSync(CA_CRT, "utf8");
}

export function leafKeyPem(): string {
  ensureCA();
  return readFileSync(LEAF_KEY, "utf8");
}

const contextCache = new Map<string, SecureContext>();

/** Cached TLS SecureContext for `host`, for the gateway's SNICallback. */
export function contextForHost(host: string): SecureContext {
  let ctx = contextCache.get(host);
  if (ctx) return ctx;
  ctx = createSecureContext({ key: leafKeyPem(), cert: certPemForHost(host) });
  contextCache.set(host, ctx);
  return ctx;
}
