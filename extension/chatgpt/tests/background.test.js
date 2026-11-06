import test from "node:test";
import assert from "node:assert/strict";

const pause = () => new Promise(resolve => setTimeout(resolve, 5));

test("sync checkpoints, current saves, offline queue and worker restart", async () => {
  const data = { port: 18242, token: "b".repeat(64), autoSync: false };
  let messageListener;
  let alarmListener;
  let details = 0;
  let writes = 0;
  let failIngest = false;
  let ingestStatus = 200;
  let updated = 100;
  let storageId = "local";
  let acknowledgement = "created";
  globalThis.chrome = {
    storage: { local: {
      async get(keys) {
        if (typeof keys === "string") keys = [keys];
        return Object.fromEntries(keys.filter(k => k in data).map(k => [k, structuredClone(data[k])]));
      },
      async set(values) { Object.assign(data, structuredClone(values)); },
      async remove(keys) { for (const key of Array.isArray(keys) ? keys : [keys]) delete data[key]; },
    } },
    alarms: { async clear() {}, async create() {}, onAlarm: { addListener(fn) { alarmListener = fn; } } },
    runtime: { id: "test", getURL: path => `chrome-extension://test/${path}`, onMessage: { addListener(fn) { messageListener = fn; } } },
  };
  globalThis.fetch = async (url, options = {}) => {
    const reply = value => ({ ok: true, status: 200, json: async () => value });
    if (url.endsWith("/health")) return reply({ app: "buddy", schema_version: 1, storage_id: storageId });
    if (url.endsWith("/api/auth/session")) return reply({ accessToken: "provider-secret", user: { id: "user-1" } });
    if (url.includes("/conversations?")) return reply({ total: 1, items: [{ id: "chat-1", update_time: updated }] });
    if (url.endsWith("/conversation/chat-1")) {
      details++;
      return reply({ conversation_id: "chat-1", mapping: {}, current_node: "node", update_time: updated });
    }
    if (url.endsWith("/v1/chatgpt")) {
      assert.equal(options.headers.Authorization, `Bearer ${"b".repeat(64)}`);
      const payload = JSON.parse(options.body);
      assert.equal(payload.account.length, 64);
      assert.equal(options.body.includes("provider-secret"), false);
      if (failIngest) throw new TypeError("Failed to fetch");
      if (ingestStatus !== 200) return { ok: false, status: ingestStatus };
      writes++;
      return reply({ status: acknowledgement });
    }
    throw new Error(`Unexpected URL: ${url}`);
  };
  await import("../background.js?first");
  await pause();
  async function send(message) {
    const response = await new Promise(resolve => messageListener(message, { id: "test", url: "chrome-extension://test/popup.html" }, resolve));
    assert.equal(response.ok, true, response.error);
    for (let i = 0; i < 200; i++) {
      await pause();
      if (data.status && !data.status.running) return;
    }
    assert.fail("sync did not finish");
  }
  await send({ type: "sync" });
  assert.equal(writes, 1);
  assert.equal(details, 1);
  assert.equal(data.activeProgress, undefined);
  await send({ type: "sync" });
  assert.equal(writes, 1);
  assert.equal(details, 1);
  assert.equal(data.status.skipped, 1);
  await send({ type: "sync", currentId: "chat-1" });
  assert.equal(writes, 2);
  assert.equal(details, 2);
  updated = 200;
  failIngest = true;
  await send({ type: "sync" });
  assert.match(data.status.error, /Buddy upload.*MiB.*Failed to fetch/);
  assert.equal(data.pending.conversation.update_time, 200);
  assert.ok(data.activeProgress);
  assert.ok(data.status.retryAt > Date.now());
  failIngest = false;
  data.status.retryAt = 0;
  await import("../background.js?restarted");
  await pause();
  await alarmListener({ name: "resume" });
  assert.equal(data.pending, undefined);
  assert.equal(data.activeProgress, undefined);
  assert.equal(data.status.error, null);
  assert.equal(writes, 4); // queued delivery and idempotent replay of the interrupted item
  await send({ type: "sync", full: true });
  assert.equal(writes, 5);
  assert.equal(data.status.saved, 1);
  ingestStatus = 413;
  await send({ type: "sync", full: true });
  assert.match(data.status.error, /exceeds.*size limit/);
  assert.match(data.status.error, /max_body_mib/);
  assert.ok(data.pending);
  ingestStatus = 200;
  await send({ type: "sync" });
  assert.equal(data.status.error, null);
  assert.equal(data.pending, undefined);
  const beforeSwitch = details;
  storageId = "shared";
  await send({ type: "sync" });
  assert.equal(details, beforeSwitch + 1, "a new destination must not reuse old checkpoints");
  await send({ type: "sync" });
  assert.equal(details, beforeSwitch + 1);
  acknowledgement = "stale";
  await send({ type: "sync", full: true });
  assert.equal(data.status.saved, 0);
  assert.equal(data.status.skipped, 1);
});
