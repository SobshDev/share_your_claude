// Real opencodex adapter, isolated mock-backed router. Never opens a real Claude connection.
//
// Do not run this file directly. The ignored Rust test `opencodex_adapter_smoke` starts a
// mock-backed router, then runs `bun scripts/opencodex-smoke.ts` with SMOKE_ROUTER_URL and
// SMOKE_ROUTER_KEY set:
//
//   OPENCODEX_SOURCE=/absolute/path/to/@bitkyc08/opencodex \
//   cargo test --locked opencodex_adapter_smoke -- --ignored
//
// Requires Bun (CI uses the version in .github/workflows/opencodex-smoke.yml). Tested against
// the opencodex version pinned as OPENCODEX_VERSION in that workflow. The script imports
// opencodex internals (src/adapters/anthropic.ts, src/lib/translator-budget.ts), so a new
// opencodex release can move them; bump the pinned version deliberately.
import assert from "node:assert/strict";
import { pathToFileURL } from "node:url";

function requireEnv(name: string, hint: string): string {
  const value = process.env[name]?.trim();
  if (!value) {
    console.error(`opencodex smoke: ${name} is not set. ${hint}`);
    process.exit(2);
  }
  return value;
}

const source = requireEnv(
  "OPENCODEX_SOURCE",
  "Point it at an unpacked @bitkyc08/opencodex package that contains src/.",
);
const baseUrl = requireEnv(
  "SMOKE_ROUTER_URL",
  "Run this through `cargo test opencodex_adapter_smoke -- --ignored`.",
);
const apiKey = requireEnv(
  "SMOKE_ROUTER_KEY",
  "Run this through `cargo test opencodex_adapter_smoke -- --ignored`.",
);

// These values mirror the mock upstream in the Rust test harness (src/tests): the MODEL
// constant, and mock_message usage of 10 input + 20 cache read + 30 cache creation tokens
// with 7 output tokens. Update both sides together.
const MOCK_MODEL = "claude-sonnet-4-6";
const MOCK_INPUT_TOKENS = 10 + 20 + 30;
const MOCK_OUTPUT_TOKENS = 7;

const { createAnthropicAdapter } = await import(
  pathToFileURL(`${source}/src/adapters/anthropic.ts`).href
);
const { createTranslatorBudget } = await import(
  pathToFileURL(`${source}/src/lib/translator-budget.ts`).href
);
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
assert.equal(models.data[0].id, MOCK_MODEL);

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
    assert.equal(done.usage.inputTokens, MOCK_INPUT_TOKENS);
    assert.equal(done.usage.outputTokens, MOCK_OUTPUT_TOKENS);
    assert(events.some((event) => event.name === "custom_search"));
  } finally {
    budget.dispose();
  }
}
console.log(
  "opencodex model discovery, JSON, SSE, tool names, and usage: passed",
);
