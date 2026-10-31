import test from "node:test";
import assert from "node:assert/strict";
import { request, revision } from "../api.js";

test("provider requests carry browser credentials and classify login errors", async () => {
  let call;
  const result = await request("https://chatgpt.com/test", "secret", async (url, options) => {
    call = { url, options };
    return { ok: true, status: 200, json: async () => ({ value: 1 }) };
  });
  assert.deepEqual(result, { value: 1 });
  assert.equal(call.options.credentials, "include");
  assert.equal(call.options.headers.Authorization, "Bearer secret");
  for (const status of [401, 403]) {
    await assert.rejects(request("unused", null, async () => ({ status })), /Sign in to ChatGPT/);
  }
  await assert.rejects(request("unused", null, async () => ({ status: 404, ok: false })), /HTTP 404/);
});

test("rate limiting backs off and retries", async () => {
  const original = globalThis.setTimeout;
  let calls = 0;
  let delay;
  globalThis.setTimeout = (callback, milliseconds) => { delay = milliseconds; callback(); };
  try {
    const body = await request("unused", null, async () => ++calls === 1
      ? { status: 429, headers: new Headers({ "retry-after": "2" }) }
      : { status: 200, ok: true, json: async () => ({ ok: true }) });
    assert.deepEqual(body, { ok: true });
    assert.equal(calls, 2);
    assert.equal(delay, 2000);
  } finally { globalThis.setTimeout = original; }
});

test("list revisions normalize seconds and ISO dates", () => {
  assert.equal(revision(1700000000), 1700000000);
  assert.equal(revision("2026-01-01T00:00:00Z"), 1767225600);
  assert.equal(revision(undefined), 0);
});
