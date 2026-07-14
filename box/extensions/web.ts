/**
 * impyard web tools — governed `web_search` + `fetch_pages` for the box.
 *
 * Two tools over one pipeline (fetch a URL → extract its readable text as markdown):
 *
 *   web_search(searches[])   Keyless DuckDuckGo HTML search. Turns each query into
 *                            a results page and hands back the ranked links + snippets,
 *                            so the MODEL curates which sources are worth opening
 *                            (no SEO junk auto-pulled into context).
 *   fetch_pages(urls[])      Read the chosen pages as clean markdown.
 *
 * Why this exists instead of an off-the-shelf package: every byte of HTTP here goes
 * through Node's built-in `fetch` (undici), so it inherits the box's governed-egress
 * plumbing for free —
 *   • NODE_USE_ENV_PROXY + HTTPS_PROXY route every request through the Impyard gateway,
 *     where it is identity-attributed, judged (default-deny), and metered;
 *   • NODE_EXTRA_CA_CERTS makes it trust the gateway's MITM certificate.
 * No API keys, no native code: extraction is pure-JS linkedom + Defuddle. (A native
 * TLS-impersonating client would neither trust our CA nor route through the proxy.)
 */

import { parseHTML } from "linkedom";
import { Defuddle } from "defuddle/node";

// DuckDuckGo's no-JavaScript HTML endpoint. `{query}` is percent-encoded per search.
const SEARCH_URL_TEMPLATE = "https://html.duckduckgo.com/html/?q={query}";

// Cap on extracted text handed back per query/page, so one huge page can't blow out
// the model's context. A DDG results page measures ~8k chars through this pipeline.
const MAX_CHARS = 10_000;

const FETCH_TIMEOUT_MS = 12_000;

// How many fetches run at once. DDG is touchy about bursts, so search stays serial;
// reading already-chosen pages can go a few at a time.
const SEARCH_CONCURRENCY = 1;
const FETCH_CONCURRENCY = 3;

// undici's default User-Agent is "node", which many sites (and DDG) reject. Present a
// plain browser UA. This is just a header — not TLS-fingerprint impersonation, which our
// re-originating gateway would strip anyway.
const BROWSER_HEADERS: Record<string, string> = {
  "User-Agent":
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 " +
    "(KHTML, like Gecko) Chrome/140.0.0.0 Safari/537.36",
  Accept: "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
  "Accept-Language": "en-US,en;q=0.9",
};

// ── fetch + extract ──────────────────────────────────────────────────────────

/** The outcome of fetching one URL: readable text, or the reason it failed. */
type PageResult =
  | { ok: true; requestedUrl: string; finalUrl: string; title: string; text: string }
  | { ok: false; requestedUrl: string; error: string };

// Defuddle unconditionally console.warns when a page's own metadata holds a
// malformed URL (e.g. an og:url with two comma-joined links) — noise we can't
// act on and which pollutes the box log. Silence warn/error just for the
// extraction call. Depth-counted so concurrent fetches restore console correctly.
let quietDepth = 0;
let realWarn: typeof console.warn;
let realError: typeof console.error;
async function quiet<T>(fn: () => Promise<T>): Promise<T> {
  if (quietDepth++ === 0) {
    realWarn = console.warn;
    realError = console.error;
    console.warn = () => {};
    console.error = () => {};
  }
  try {
    return await fn();
  } finally {
    if (--quietDepth === 0) {
      console.warn = realWarn;
      console.error = realError;
    }
  }
}

/** Fetch one URL and extract its readable text. Never throws — failures come back as `{ ok: false }`. */
async function fetchReadablePage(url: string, signal?: AbortSignal): Promise<PageResult> {
  // Bound every request by our own timeout, honoring an outer cancel (the tool's signal) too.
  const timeout = AbortSignal.timeout(FETCH_TIMEOUT_MS);
  const deadline = signal ? AbortSignal.any([signal, timeout]) : timeout;
  try {
    // Reject a malformed URL here (synchronously, so this try/catch handles it).
    // Otherwise it reaches Defuddle, whose internal new URL() can reject outside
    // this scope and crash the whole tool call.
    new URL(url);
    const response = await fetch(url, {
      headers: BROWSER_HEADERS,
      redirect: "follow",
      signal: deadline,
    });
    if (!response.ok) {
      return { ok: false, requestedUrl: url, error: `HTTP ${response.status} ${response.statusText}` };
    }
    const finalUrl = response.url || url; // may differ after redirects
    const body = await response.text();

    // Non-HTML (JSON, plain text, etc.): return the body verbatim; extraction would only mangle it.
    if (!(response.headers.get("content-type") ?? "").includes("html")) {
      return { ok: true, requestedUrl: url, finalUrl, title: finalUrl, text: body.trim() };
    }

    const { document } = parseHTML(body);
    const extraction = await quiet(() => Defuddle(document, finalUrl, { markdown: true, removeImages: true }));
    return {
      ok: true,
      requestedUrl: url,
      finalUrl,
      title: extraction.title || finalUrl,
      text: (extraction.content || "").trim(),
    };
  } catch (caught) {
    return { ok: false, requestedUrl: url, error: caught instanceof Error ? caught.message : String(caught) };
  }
}

/** Run `task` over `items`, at most `limit` in flight at once, preserving order. */
async function mapLimit<In, Out>(items: In[], limit: number, task: (item: In) => Promise<Out>): Promise<Out[]> {
  const out = new Array<Out>(items.length);
  let next = 0;
  const imp = async () => {
    while (next < items.length) {
      const i = next++;
      out[i] = await task(items[i]!);
    }
  };
  await Promise.all(Array.from({ length: Math.min(limit, items.length) }, imp));
  return out;
}

/** Flatten model-supplied url args: split any that pack several URLs together
 *  (comma- or whitespace-joined — a common tool-call mistake) and keep only
 *  valid http(s) URLs, de-duplicated in order. */
function normalizeUrls(input: unknown): string[] {
  const raw = Array.isArray(input) ? input : [input];
  const seen = new Set<string>();
  for (const item of raw) {
    if (typeof item !== "string") continue;
    for (const piece of item.split(/[\s,]+/)) {
      const u = piece.trim();
      if (!u) continue;
      try {
        const parsed = new URL(u);
        if (parsed.protocol === "http:" || parsed.protocol === "https:") seen.add(u);
      } catch {
        // not a URL — drop it
      }
    }
  }
  return [...seen];
}

function cap(text: string): string {
  return text.length > MAX_CHARS ? text.slice(0, MAX_CHARS) + "\n…(truncated)" : text;
}

// ── DuckDuckGo specifics ───────────────────────────────────────────────────────

function buildSearchUrl(query: string): string {
  return SEARCH_URL_TEMPLATE.replace("{query}", encodeURIComponent(query));
}

/**
 * DDG wraps each result link in a redirect: `//duckduckgo.com/l/?uddg=<real-url>&rut=…`,
 * with the destination percent-encoded in `uddg`. Left alone, the model would hand these
 * opaque redirects to `fetch_pages`. Unwrap them back to the real URL wherever they appear.
 */
function unwrapDdgLinks(markdown: string): string {
  const redirect =
    /(?:https?:)?\/\/(?:[a-z0-9-]+\.)?duckduckgo\.com\/l\/\?[^)\s"'<>]*?\buddg=([^&)\s"'<>]+)[^)\s"'<>]*/gi;
  return markdown.replace(redirect, (whole, encoded) => {
    try {
      return decodeURIComponent(encoded);
    } catch {
      return whole; // malformed encoding → leave the link untouched
    }
  });
}

// ── result formatting ──────────────────────────────────────────────────────────

const SEARCH_FOLLOW_UP = [
  "# Next step: evaluate the results",
  "",
  "These are previews — brief, and sometimes stale. If they don't fully answer the question, read the full pages:",
  "1. Choose the most relevant URLs below.",
  "2. Call `fetch_pages` with those URLs.",
  "3. Answer from what you read.",
  "",
].join("\n");

function formatSearch(results: { query: string; result: PageResult }[]): string {
  const sections = [SEARCH_FOLLOW_UP];
  for (const { query, result } of results) {
    sections.push(`## Query: "${query}"`);
    if (!result.ok) {
      sections.push(`_search failed: ${result.error}_\n`);
      continue;
    }
    sections.push(cap(unwrapDdgLinks(result.text) || "_no results extracted_") + "\n");
  }
  return sections.join("\n");
}

function formatPages(results: PageResult[]): string {
  const sections: string[] = [];
  for (const result of results) {
    if (!result.ok) {
      sections.push(`## ${result.requestedUrl}\n\n_fetch failed: ${result.error}_\n`);
      continue;
    }
    const header = result.finalUrl === result.requestedUrl ? result.finalUrl : `${result.requestedUrl} → ${result.finalUrl}`;
    sections.push(`## ${result.title}\n<${header}>\n\n${cap(result.text) || "_no content extracted_"}\n`);
  }
  return sections.join("\n");
}

// ── tool registration ──────────────────────────────────────────────────────────

// pi consumes `parameters` as a JSON Schema (typebox, which the reference uses, just emits
// one at runtime), so we write the schema directly and take on no schema dependency.
const searchParameters = {
  type: "object",
  properties: {
    searches: {
      type: "array",
      items: { type: "string" },
      minItems: 1,
      description:
        "One or more search queries to run together. Pass several at once to cover a topic from multiple angles in a single call.",
    },
  },
  required: ["searches"],
  additionalProperties: false,
};

const fetchParameters = {
  type: "object",
  properties: {
    urls: {
      type: "array",
      items: { type: "string" },
      minItems: 1,
      description: "One or more page URLs to read. Typically the best results from a prior web_search.",
    },
  },
  required: ["urls"],
  additionalProperties: false,
};

interface PiToolApi {
  registerTool(definition: {
    name: string;
    label: string;
    description: string;
    promptSnippet?: string;
    parameters: unknown;
    execute: (
      toolCallId: string,
      params: Record<string, unknown>,
      signal?: AbortSignal,
    ) => Promise<{ content: { type: "text"; text: string }[] }>;
  }): void;
}

export default function impyardWebTools(api: PiToolApi): void {
  api.registerTool({
    name: "web_search",
    label: "web_search",
    description:
      "Search the web. Call this whenever current or external information would change your answer — " +
      "latest versions, APIs, prices, dates, events, or anything you can't verify from memory. " +
      "Returns ranked result pages; follow up with fetch_pages to read the best ones.",
    promptSnippet: "web_search(searches: string[]): batch web search; returns ranked result pages",
    parameters: searchParameters,
    async execute(_id, params, signal) {
      const searches = (params.searches as string[]) ?? [];
      const results = await mapLimit(searches, SEARCH_CONCURRENCY, async (query) => ({
        query,
        result: await fetchReadablePage(buildSearchUrl(query), signal),
      }));
      return { content: [{ type: "text", text: formatSearch(results) }] };
    },
  });

  api.registerTool({
    name: "fetch_pages",
    label: "fetch_pages",
    description:
      "Fetch one or more web pages and return their readable content as markdown. " +
      "Use it to read the sources web_search surfaces, or any URL you already have.",
    promptSnippet: "fetch_pages(urls: string[]): read web pages as clean markdown",
    parameters: fetchParameters,
    async execute(_id, params, signal) {
      const urls = normalizeUrls(params.urls);
      if (urls.length === 0) {
        return { content: [{ type: "text", text: "No valid URLs to fetch. Pass each URL as a separate array element." }] };
      }
      const results = await mapLimit(urls, FETCH_CONCURRENCY, (url) => fetchReadablePage(url, signal));
      return { content: [{ type: "text", text: formatPages(results) }] };
    },
  });
}
