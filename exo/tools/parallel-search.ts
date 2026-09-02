import { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { StreamableHTTPClientTransport } from "@modelcontextprotocol/sdk/client/streamableHttp.js";
import { CallToolResultSchema } from "@modelcontextprotocol/sdk/types.js";
import { z } from "zod";

import type { WebSearchResult } from "./web-tools";

const ENDPOINT = "https://search.parallel.ai/mcp";
const MAX_RESPONSE_BYTES = 1_000_000;
const MAX_SNIPPET_CHARS = 2_500;

const searchPayload = z.object({
  results: z.array(
    z.object({
      url: z.url(),
      title: z.string().nullish(),
      excerpts: z.array(z.string()),
    }),
  ),
  warnings: z.array(z.string()).nullish(),
});

/** Anonymous, explicitly selected search. The SDK owns MCP framing and sessions. */
export async function searchParallel(
  query: string,
  count: number,
  conversationId: string,
): Promise<{
  results: WebSearchResult[];
  warnings: string[];
  truncated: boolean;
}> {
  // Reject rather than silently accept the service's query truncation.
  if (query.length > 200) {
    throw new Error("Parallel search queries must be at most 200 characters");
  }
  let deadline = AbortSignal.timeout(12_000);
  const client = new Client({ name: "exo-parallel-search", version: "1.0.0" });
  const transport = new StreamableHTTPClientTransport(new URL(ENDPOINT), {
    // No auth provider, API key, fallback, or automatic stream reconnection.
    reconnectionOptions: {
      maxRetries: 0,
      initialReconnectionDelay: 1_000,
      maxReconnectionDelay: 1_000,
      reconnectionDelayGrowFactor: 1,
    },
    fetch: async (url, init) => {
      const response = await fetch(url, {
        ...init,
        redirect: "error",
        signal: AbortSignal.any(
          init?.signal ? [deadline, init.signal] : [deadline],
        ),
      });
      if (response.body === null) return response;
      let bytes = 0;
      // Bound JSON and SSE before the SDK buffers/parses the response.
      const body = response.body.pipeThrough(
        new TransformStream<Uint8Array, Uint8Array>({
          transform(chunk, controller) {
            bytes += chunk.byteLength;
            if (bytes > MAX_RESPONSE_BYTES) {
              throw new Error("Parallel search response exceeded 1 MB");
            }
            controller.enqueue(chunk);
          },
        }),
      );
      return new Response(body, {
        status: response.status,
        statusText: response.statusText,
        headers: response.headers,
      });
    },
  });

  try {
    await client.connect(transport, { signal: deadline });
    let cursor: string | undefined;
    do {
      const page = await client.listTools({ cursor }, { signal: deadline });
      if (page.tools.some(({ name }) => name === "web_search")) break;
      cursor = page.nextCursor;
      if (cursor === undefined) {
        throw new Error("Parallel MCP did not advertise web_search");
      }
    } while (cursor !== undefined);

    const result = CallToolResultSchema.parse(
      await client.callTool(
        {
          name: "web_search",
          arguments: {
            objective: query,
            search_queries: [query],
            session_id: conversationId,
          },
        },
        undefined,
        { signal: deadline },
      ),
    );
    if (result.isError) {
      const detail = result.content
        .filter((block) => block.type === "text")
        .map((block) => block.text)
        .join("\n")
        .slice(0, 2_000);
      throw new Error(`Parallel search failed: ${detail}`);
    }
    // Choose one representation, never duplicate structured and text content.
    const payload = searchPayload.parse(
      result.structuredContent ??
        JSON.parse(
          result.content
            .filter((block) => block.type === "text")
            .map((block) => block.text)
            .join("\n"),
        ),
    );
    let truncated = false;
    const results = payload.results.slice(0, count).map((item) => {
      const snippet = item.excerpts.join("\n");
      if (snippet.length > MAX_SNIPPET_CHARS) truncated = true;
      return {
        title: item.title ?? "",
        url: item.url,
        snippet: snippet.slice(0, MAX_SNIPPET_CHARS),
      };
    });
    return { results, warnings: payload.warnings ?? [], truncated };
  } finally {
    // A canceled search must not prevent cleanup or let cleanup hide its result.
    deadline = AbortSignal.timeout(1_000);
    if (transport.sessionId !== undefined) {
      await transport.terminateSession().catch(() => {});
    }
    await client.close().catch(() => {});
  }
}
