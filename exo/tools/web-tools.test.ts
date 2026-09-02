import { randomUUID } from "node:crypto";
import { createServer } from "node:http";

import { HarnessToolRegistry, type TurnContext } from "@exo/harness";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { resolveExoProfile } from "../profiles";

import {
  decodeEntities,
  extractArticleMarkdown,
  extractReadableText,
  isPrivateIp,
  normalizeDuckDuckGoUrl,
  parseDuckDuckGoHtml,
} from "./web-tools";

describe("isPrivateIp", () => {
  it("blocks private and special IPv4 ranges", () => {
    for (const ip of [
      "127.0.0.1",
      "10.1.2.3",
      "172.16.0.1",
      "172.31.255.255",
      "192.168.1.1",
      "169.254.169.254",
      "100.64.0.1",
      "0.0.0.0",
      "198.18.0.1",
      "224.0.0.1",
      "255.255.255.255",
    ]) {
      expect(isPrivateIp(ip), ip).toBe(true);
    }
  });

  it("allows public IPv4 addresses", () => {
    for (const ip of ["8.8.8.8", "1.1.1.1", "93.184.216.34", "172.32.0.1"]) {
      expect(isPrivateIp(ip), ip).toBe(false);
    }
  });

  it("blocks private and special IPv6 ranges", () => {
    for (const ip of [
      "::1",
      "::",
      "fc00::1",
      "fd12:3456::1",
      "fe80::1",
      "ff02::1",
      "::ffff:10.0.0.1",
      "::ffff:127.0.0.1",
    ]) {
      expect(isPrivateIp(ip), ip).toBe(true);
    }
  });

  it("allows public IPv6 addresses", () => {
    for (const ip of ["2606:4700::1111", "2001:4860:4860::8888"]) {
      expect(isPrivateIp(ip), ip).toBe(false);
    }
  });

  it("fails closed on non-IP input", () => {
    expect(isPrivateIp("not-an-ip")).toBe(true);
  });
});

describe("normalizeDuckDuckGoUrl", () => {
  it("decodes the uddg redirect parameter", () => {
    const href =
      "//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fpage&rut=abc";
    expect(normalizeDuckDuckGoUrl(href)).toBe("https://example.com/page");
  });

  it("passes through direct http urls", () => {
    expect(normalizeDuckDuckGoUrl("https://example.com/x")).toBe(
      "https://example.com/x",
    );
  });

  it("drops internal duckduckgo links and empty hrefs", () => {
    expect(normalizeDuckDuckGoUrl("https://duckduckgo.com/y.js?ad=1")).toBe(
      null,
    );
    expect(normalizeDuckDuckGoUrl("")).toBe(null);
  });
});

describe("parseDuckDuckGoHtml", () => {
  const fixture = `
    <div class="result results_links results_links_deep web-result">
      <a rel="nofollow" class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fone&rut=1">First <b>Result</b></a>
      <a class="result__snippet" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fone">Snippet one &amp; more</a>
    </div>
    <div class="result">
      <a rel="nofollow" class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.org%2Ftwo&rut=2">Second Result</a>
      <a class="result__snippet" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.org%2Ftwo">Snippet two</a>
    </div>
  `;

  it("extracts paired titles, urls, and snippets", () => {
    const results = parseDuckDuckGoHtml(fixture, 10);
    expect(results).toEqual([
      {
        title: "First Result",
        url: "https://example.com/one",
        snippet: "Snippet one & more",
      },
      {
        title: "Second Result",
        url: "https://example.org/two",
        snippet: "Snippet two",
      },
    ]);
  });

  it("respects the result limit", () => {
    expect(parseDuckDuckGoHtml(fixture, 1)).toHaveLength(1);
  });

  it("returns empty for unrecognized markup", () => {
    expect(parseDuckDuckGoHtml("<html><body>captcha</body></html>", 5)).toEqual(
      [],
    );
  });

  it("does not skew pairing when a result has no snippet", () => {
    const html = `
      <a class="result__a" href="https://example.com/no-snippet">No Snippet</a>
      <a class="result__a" href="https://example.org/with">With Snippet</a>
      <a class="result__snippet" href="https://example.org/with">Only snippet</a>
    `;
    expect(parseDuckDuckGoHtml(html, 10)).toEqual([
      {
        title: "No Snippet",
        url: "https://example.com/no-snippet",
        snippet: "",
      },
      {
        title: "With Snippet",
        url: "https://example.org/with",
        snippet: "Only snippet",
      },
    ]);
  });
});

describe("extractReadableText", () => {
  it("extracts title, headings, links, and body text", () => {
    const html = `
      <html>
        <head><title>Page &amp; Title</title><style>body { color: red; }</style></head>
        <body>
          <script>var tracked = true;</script>
          <nav><a href="https://example.com/nav">Nav link</a></nav>
          <h1>Main Heading</h1>
          <p>Hello <b>world</b>, see <a href="https://example.com/doc">the docs</a>.</p>
          <ul><li>Alpha</li><li>Beta</li></ul>
        </body>
      </html>
    `;
    const { title, text } = extractReadableText(html);
    expect(title).toBe("Page & Title");
    expect(text).toContain("# Main Heading");
    expect(text).toContain("Hello world");
    expect(text).toContain("[the docs](https://example.com/doc)");
    expect(text).toContain("- Alpha");
    expect(text).not.toContain("tracked");
    expect(text).not.toContain("color: red");
    expect(text).not.toContain("Nav link");
  });
});

describe("extractArticleMarkdown", () => {
  const articleHtml = `
    <html>
      <head><title>Promises Explained | Example Blog</title></head>
      <body>
        <nav><a href="/">Home</a> <a href="/about">About</a></nav>
        <aside>Subscribe to our newsletter! Ads ads ads.</aside>
        <article>
          <h1>Promises Explained</h1>
          <p>A Promise represents the eventual completion or failure of an
          asynchronous operation and its resulting value. Unlike callbacks,
          promises can be chained, which makes asynchronous code far more
          readable and maintainable in complex applications.</p>
          <p>Promises have three states: pending, fulfilled, and rejected.
          Once a promise settles it stays settled, which makes promises a
          reliable primitive for coordinating work across large codebases.
          See <a href="/docs/promises">the docs</a> for details.</p>
          <ul><li>pending</li><li>fulfilled</li><li>rejected</li></ul>
        </article>
        <footer>© 2026 Example Corp</footer>
      </body>
    </html>
  `;

  it("extracts main content as markdown and drops boilerplate", () => {
    const result = extractArticleMarkdown(
      articleHtml,
      "https://example.com/articles/promises",
    );
    expect(result).not.toBeNull();
    expect(result?.title).toContain("Promises Explained");
    expect(result?.text).toContain("eventual completion or failure");
    expect(result?.text).toMatch(/-\s+pending/);
    expect(result?.text).not.toContain("newsletter");
    expect(result?.text).not.toContain("Ads ads ads");
  });

  it("resolves relative links against the page url", () => {
    const result = extractArticleMarkdown(
      articleHtml,
      "https://example.com/articles/promises",
    );
    expect(result?.text).toContain(
      "[the docs](https://example.com/docs/promises)",
    );
  });

  it("returns null when there is no content to extract", () => {
    expect(extractArticleMarkdown("", "https://example.com/")).toBe(null);
    expect(
      extractArticleMarkdown(
        "<html><body><script>x()</script></body></html>",
        "https://example.com/",
      ),
    ).toBe(null);
  });
});

describe("decodeEntities", () => {
  it("decodes named and numeric entities", () => {
    expect(decodeEntities("a &amp; b &lt;c&gt; &#39;d&#39; &#x41;")).toBe(
      "a & b <c> 'd' A",
    );
  });

  it("decodes common typographic named entities", () => {
    expect(
      decodeEntities("June 30 &middot; a&mdash;b &rsquo;x&rsquo; &hellip;"),
    ).toBe("June 30 · a—b ’x’ …");
  });

  it("leaves unknown entities and invalid code points intact", () => {
    expect(decodeEntities("&bogus; &#x110000; &#0;")).toBe(
      "&bogus; &#x110000; &#0;",
    );
  });
});

describe("web search provider selection", () => {
  const nativeFetch = globalThis.fetch;
  const entries = [
    {
      url: "https://example.com/one",
      title: "One",
      excerpts: ["first", "second"],
    },
    { url: "https://example.com/two", title: null, excerpts: ["third"] },
  ];

  let now = Date.now();
  beforeEach(() => {
    // The existing Brave credential cache lasts one minute; isolate each case.
    vi.spyOn(Date, "now").mockImplementation(() => now);
    now += 61_000;
  });

  afterEach(() => {
    vi.restoreAllMocks();
    vi.unstubAllEnvs();
    vi.unstubAllGlobals();
    vi.useRealTimers();
  });

  function context(conversationId = randomUUID()) {
    return {
      agentConfig: { enableAgentToolCreation: false },
      streaming: false,
      exoharness: {
        listSecrets: async () => [],
        current: {
          conversation: { record: { id: conversationId } },
          turn: {
            writeArtifactText: async ({
              path,
              text,
            }: {
              path: string;
              text: string;
            }) => ({
              artifactId: "artifact",
              path,
              version: 1,
              sizeBytes: text.length,
            }),
          },
        },
      },
    } as unknown as TurnContext;
  }

  async function search(
    query: string = randomUUID(),
    count: number | null = null,
    ctx = context(),
  ) {
    const registry = new HarnessToolRegistry(ctx);
    resolveExoProfile("practical").registerTools(registry, ctx);
    const [event] = await registry.executePending([
      {
        toolCallId: "test-search",
        request: { functionName: "web_search", arguments: { query, count } },
      },
    ]);
    const result = event.result as { value: Record<string, unknown> };
    return result.value;
  }

  function mcp(
    result: unknown = { content: [], structuredContent: { results: entries } },
    options: {
      rpcError?: boolean;
      status?: number;
      sse?: boolean;
      missingTool?: boolean;
      cleanupFails?: boolean;
    } = {},
  ) {
    vi.stubEnv("EXO_WEB_SEARCH_PROVIDER", "parallel");
    const requests: { method: string; params?: Record<string, unknown> }[] = [];
    const mock = vi.fn(async (_url: unknown, init?: RequestInit) => {
      expect(String(_url)).toBe("https://search.parallel.ai/mcp");
      expect(init?.redirect).toBe("error");
      const headers = new Headers(init?.headers);
      expect(headers.has("authorization")).toBe(false);
      expect(headers.has("x-api-key")).toBe(false);
      if (init?.method === "GET") return new Response(null, { status: 405 });
      if (init?.method === "DELETE") {
        if (options.cleanupFails) throw new Error("cleanup failed");
        return new Response(null, { status: 200 });
      }
      const request = JSON.parse(String(init?.body));
      requests.push(request);
      if (request.method === "notifications/initialized")
        return new Response(null, { status: 202 });
      let payload: unknown;
      if (request.method === "initialize") {
        payload = {
          protocolVersion: request.params.protocolVersion,
          capabilities: { tools: {} },
          serverInfo: { name: "fixture", version: "1" },
        };
      } else if (request.method === "tools/list") {
        payload = options.missingTool
          ? { tools: [] }
          : {
              tools: [{ name: "web_search", inputSchema: { type: "object" } }],
            };
      } else {
        if (options.status)
          return new Response("service unavailable", {
            status: options.status,
          });
        payload = result;
      }
      const envelope =
        options.rpcError && request.method === "tools/call"
          ? {
              jsonrpc: "2.0",
              id: request.id,
              error: { code: -32000, message: "quota reached" },
            }
          : { jsonrpc: "2.0", id: request.id, result: payload };
      const isSse = options.sse && request.method === "tools/call";
      return new Response(
        isSse
          ? `event: message\ndata: ${JSON.stringify(envelope)}\n\n`
          : JSON.stringify(envelope),
        {
          headers: {
            "content-type": isSse ? "text/event-stream" : "application/json",
            "mcp-session-id": "fixture-session",
          },
        },
      );
    });
    vi.stubGlobal("fetch", mock);
    return { mock, requests };
  }

  it("maps search through the practical profile and real MCP SDK without credentials", async () => {
    vi.stubEnv("BRAVE_API_KEY", "unused-brave-key");
    vi.stubEnv("PARALLEL_API_KEY", "must-not-be-sent");
    const { requests } = mcp({
      content: [{ type: "text", text: "not the structured representation" }],
      structuredContent: { results: entries, warnings: ["service warning"] },
    });
    const ctx = context();
    const value = await search("public documentation", 1, ctx);
    expect(value).toMatchObject({
      ok: true,
      provider: "parallel",
      results: [
        { title: "One", url: entries[0].url, snippet: "first\nsecond" },
      ],
      warnings: ["service warning"],
      truncated: false,
    });
    expect(
      requests.find(({ method }) => method === "tools/call")?.params,
    ).toEqual({
      name: "web_search",
      arguments: {
        objective: "public documentation",
        search_queries: ["public documentation"],
        session_id: ctx.exoharness.current.conversation.record.id,
      },
    });
    expect(requests.map(({ method }) => method)).toContain("tools/list");
  });

  it("accepts text-only payloads and SDK-parsed SSE", async () => {
    mcp(
      {
        content: [{ type: "text", text: JSON.stringify({ results: entries }) }],
      },
      { sse: true },
    );
    expect(await search()).toMatchObject({
      ok: true,
      results: [{ snippet: "first\nsecond" }, { title: "" }],
    });
  });

  it("preserves valid empty results", async () => {
    mcp({ content: [], structuredContent: { results: [] } });
    expect(await search()).toMatchObject({ ok: true, results: [] });
  });

  it.each([
    [
      { content: [{ type: "text", text: "rate limited" }], isError: true },
      {},
      "rate limited",
    ],
    [
      {
        content: [],
        structuredContent: { results: [{ url: "bad", excerpts: [] }] },
      },
      {},
      "Invalid",
    ],
    [{ content: [] }, { rpcError: true }, "quota reached"],
    [{ content: [] }, { status: 503 }, "service unavailable"],
    [{ content: [] }, { missingTool: true }, "did not advertise"],
  ])(
    "returns errors without falling back: %j",
    async (result, options, error) => {
      mcp(result, options);
      expect(await search()).toMatchObject({
        ok: false,
        provider: "parallel",
        error: expect.stringContaining(error),
      });
    },
  );

  it("caps snippets and preserves warnings", async () => {
    mcp({
      content: [],
      structuredContent: {
        results: [{ ...entries[0], excerpts: ["x".repeat(3_000)] }],
        warnings: ["partial"],
      },
    });
    expect(await search()).toMatchObject({
      truncated: true,
      warnings: ["partial"],
      results: [{ snippet: "x".repeat(2_500) }],
    });
  });

  it("rejects overlong queries before making a request", async () => {
    const { mock } = mcp();
    expect(await search("x".repeat(201))).toMatchObject({
      ok: false,
      error: expect.stringContaining("200 characters"),
    });
    expect(mock).not.toHaveBeenCalled();
  });

  it("enforces the response byte limit before parsing", async () => {
    mcp({ content: [{ type: "text", text: "x".repeat(1_000_001) }] });
    expect(await search()).toMatchObject({
      ok: false,
      error: expect.stringContaining("exceeded 1 MB"),
    });
  });

  it("keeps conversation metadata stable across turns and distinct across conversations", async () => {
    const { requests } = mcp();
    const ctx = context();
    await search(randomUUID(), 1, ctx);
    await search(randomUUID(), 1, ctx);
    await search();
    const ids = requests
      .filter(({ method }) => method === "tools/call")
      .map(
        ({ params }) =>
          (params?.arguments as { session_id: string } | undefined)?.session_id,
      );
    expect(ids[0]).toBe(ids[1]);
    expect(ids[2]).not.toBe(ids[0]);
  });

  it("retains provider-scoped caching and does not repeat an MCP call on a cache hit", async () => {
    const { requests } = mcp();
    const query = randomUUID();
    await search(query);
    expect(await search(query)).toMatchObject({
      ok: true,
      cached: true,
      provider: "parallel",
    });
    expect(
      requests.filter(({ method }) => method === "tools/call"),
    ).toHaveLength(1);
  });

  it("does not let cleanup failure replace a successful result", async () => {
    mcp(undefined, { cleanupFails: true });
    expect(await search()).toMatchObject({ ok: true, provider: "parallel" });
  });

  it("aborts a stalled request at the host deadline", async () => {
    vi.useFakeTimers();
    vi.stubEnv("EXO_WEB_SEARCH_PROVIDER", "parallel");
    vi.stubGlobal(
      "fetch",
      vi.fn(
        (_url, init) =>
          new Promise((_resolve, reject) => {
            init.signal.addEventListener(
              "abort",
              () => reject(init.signal.reason),
              { once: true },
            );
          }),
      ),
    );
    // Node's AbortSignal.timeout uses native timers, so stub only its clock.
    vi.spyOn(AbortSignal, "timeout").mockImplementation((ms) => {
      const controller = new AbortController();
      setTimeout(() => controller.abort(new Error("request deadline")), ms);
      return controller.signal;
    });
    try {
      const pending = search();
      await vi.advanceTimersByTimeAsync(12_000);
      expect(await pending).toMatchObject({
        ok: false,
        error: expect.stringContaining("deadline"),
      });
    } finally {
      vi.restoreAllMocks();
    }
  });

  it("rejects redirects without contacting their destination", async () => {
    let destinationRequests = 0;
    const destination = createServer((_req, res) => {
      destinationRequests++;
      res.end("unexpected");
    });
    const redirect = createServer((_req, res) => {
      const address = destination.address();
      if (!address || typeof address === "string")
        throw new Error("missing address");
      res.writeHead(307, { Location: `http://127.0.0.1:${address.port}` });
      res.end();
    });
    await new Promise<void>((resolve) =>
      destination.listen(0, "127.0.0.1", resolve),
    );
    await new Promise<void>((resolve) =>
      redirect.listen(0, "127.0.0.1", resolve),
    );
    vi.stubEnv("EXO_WEB_SEARCH_PROVIDER", "parallel");
    vi.stubGlobal("fetch", (_url: unknown, init?: RequestInit) => {
      const address = redirect.address();
      if (!address || typeof address === "string")
        throw new Error("missing address");
      return nativeFetch(`http://127.0.0.1:${address.port}`, init);
    });
    try {
      expect(await search()).toMatchObject({ ok: false, provider: "parallel" });
      expect(destinationRequests).toBe(0);
    } finally {
      redirect.closeAllConnections();
      destination.closeAllConnections();
      await Promise.all(
        [redirect, destination].map(
          (server) =>
            new Promise<void>((resolve) => server.close(() => resolve())),
        ),
      );
    }
  });

  it.each([
    [undefined, undefined, "duckduckgo"],
    [undefined, "brave-key", "brave"],
    ["duckduckgo", "brave-key", "duckduckgo"],
    ["brave", "brave-key", "brave"],
  ])(
    "preserves existing provider routing: %s / %s",
    async (forced, key, provider) => {
      vi.stubEnv("EXO_WEB_SEARCH_PROVIDER", forced ?? "");
      vi.stubEnv("BRAVE_API_KEY", key ?? "");
      const mock = vi.fn(async (url: unknown) => {
        if (provider === "brave") {
          expect(String(url)).toContain("api.search.brave.com");
          return Response.json({
            web: {
              results: [
                { title: "Brave", url: entries[0].url, description: "snippet" },
              ],
            },
          });
        }
        expect(String(url)).toContain("html.duckduckgo.com");
        return new Response(
          `<a class="result__a" href="${entries[0].url}">DuckDuckGo</a>`,
        );
      });
      vi.stubGlobal("fetch", mock);
      expect(await search()).toMatchObject({ ok: true, provider });
      expect(mock).toHaveBeenCalledOnce();
    },
  );

  it("keeps explicitly selected Brave credential failure without a fallback", async () => {
    vi.stubEnv("EXO_WEB_SEARCH_PROVIDER", "brave");
    vi.stubEnv("BRAVE_API_KEY", "");
    const mock = vi.fn();
    vi.stubGlobal("fetch", mock);
    expect(await search()).toMatchObject({
      ok: false,
      provider: "brave",
      error: expect.stringContaining("no Brave key"),
    });
    expect(mock).not.toHaveBeenCalled();
  });
});
