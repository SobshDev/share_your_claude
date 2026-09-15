// Real opencodex adapter, isolated mock-backed router. Never opens a real Claude connection.
import assert from "node:assert/strict";
import { pathToFileURL } from "node:url";
const source = process.env.OPENCODEX_SOURCE!;
const { createAnthropicAdapter } = await import(
  pathToFileURL(`${source}/src/adapters/anthropic.ts`).href
);
const { createTranslatorBudget } = await import(
  pathToFileURL(`${source}/src/lib/translator-budget.ts`).href
);
const baseUrl = process.env.SMOKE_ROUTER_URL!;
const apiKey = process.env.SMOKE_ROUTER_KEY!;
const adapter = createAnthropicAdapter({
  adapter: "anthropic",
  baseUrl,
  authMode: "key",
  apiKey,
});

const catalog = await fetch(`${baseUrl}/v1/models`, {
  headers: { "x-api-key": apiKey },
});
assert.equal(catalog.status, 200);
const models = await catalog.json();
assert.equal(models.data.length, 1);
assert.equal(models.data[0].id, "claude-sonnet-4-6");

for (const stream of [false, true]) {
  const parsed = {
    modelId: models.data[0].id,
    stream,
    options: { maxOutputTokens: 128 },
    context: {
      systemPrompt: ["Return one tool call."],
      messages: [{ role: "user", content: "hello", timestamp: Date.now() }],
      tools: [
        {
          name: "custom_search",
          description: "A smoke-test tool",
          parameters: { type: "object", properties: {} },
        },
      ],
    },
  };
  const request = await adapter.buildRequest(parsed);
  const response = await fetch(request.url, {
    method: request.method,
    headers: request.headers,
    body: request.body,
  });
  assert.equal(response.status, 200);
  const budget = createTranslatorBudget();
  const events = [];
  try {
    if (stream)
      for await (const event of adapter.parseStream(response, budget))
        events.push(event);
    else events.push(...(await adapter.parseResponse(response, budget)));
    assert(!events.some((event) => event.type === "error"));
    const done = events.find((event) => event.type === "done");
    assert(done);
    assert.equal(done.usage.inputTokens, 60);
    assert.equal(done.usage.outputTokens, 7);
    assert(events.some((event) => event.name === "custom_search"));
  } finally {
    budget.dispose();
  }
}
console.log(
  "opencodex model discovery, JSON, SSE, tool names, and usage: passed",
);
