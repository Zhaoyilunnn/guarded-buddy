import "fake-indexeddb/auto";
import * as db from "../store.js";
import test from "node:test";
import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
const pause = () => new Promise(resolve => setTimeout(resolve, 2));
async function until(check) {
  for (let i = 0; i < 1000; i++) { if (await check()) return; await pause(); }
  assert.fail("Timed out waiting for extension state");
}

test("only manual actions start or continue sync; failures, stops, and restarts never restart it", async () => {
  const manifest = JSON.parse(await readFile(new URL("../manifest.json", import.meta.url), "utf8"));
  assert.equal(manifest.permissions.includes("alarms"), false);
  const data = { port: 18242 };
  let listener;
  const disk = new Map();
  let details = 0;
  let writes = 0;
  let providerCalls = 0;
  let failIngest = false;
  let storageId = "local";
  let accountId = "user-1";
  let updated = 100;
  let hold;
  const ids = ["chat-1", "chat-2"];
  globalThis.chrome = {
    storage: { local: {
      async get(keys) { return structuredClone(Object.fromEntries((Array.isArray(keys) ? keys : [keys]).filter(k => k in data).map(k => [k, data[k]]))); },
      async set(values) { Object.assign(data, structuredClone(values)); },
    } },
    // No alarms, startup hooks, or installation hooks are exposed by this harness.
    runtime: { id: "test", getURL: p => `chrome-extension://test/${p}`, onMessage: { addListener(fn) { listener = fn; } } },
  };
  globalThis.fetch = async (url, options = {}) => {
    const reply = value => ({ ok: true, status: 200, json: async () => value });
    if (url.endsWith("/v1/sync/prepare")) return reply({ ready: true });
    if (url.endsWith("/v1/sync/check")) {
      const query = JSON.parse(options.body);
      return reply({ items: query.items.map(item => ({ id: item.id,
        status: disk.get(`${storageId}:${query.account}:${item.id}`) === item.revision ? "unchanged" : "needed" })) });
    }
    if (url.endsWith("/health")) return reply({ app: "buddy", schema_version: 1, storage_id: storageId });
    if (url.includes("chatgpt.com")) providerCalls++;
    if (url.endsWith("/api/auth/session")) return reply({ accessToken: "provider-secret", user: { id: accountId } });
    if (url.includes("/conversations?")) return reply({ total: ids.length, items: ids.map(id => ({ id, title: `Title ${id}`, update_time: updated })) });
    if (url.includes("/conversation/")) {
      details++;
      const id = url.split("/").pop();
      return reply({ conversation_id: id, title: `Title ${id}`, mapping: {}, current_node: "node", update_time: updated });
    }
    if (url.includes("/v1/archives?")) return reply({ total: 0, items: [], warnings: [], accounts: [] });
    if (url.endsWith("/v1/chatgpt")) {
      assert.equal(options.headers.Authorization, `Bearer ${"b".repeat(64)}`);
      assert.equal(options.body.includes("provider-secret"), false);
      if (hold) await hold;
      if (failIngest) throw new TypeError("Failed to fetch");
      const snapshot = JSON.parse(options.body);
      disk.set(`${storageId}:${snapshot.account}:${snapshot.conversation.conversation_id}`, snapshot.conversation.update_time);
      writes++;
      return reply({ status: "updated" });
    }
    throw new Error(`Unexpected URL: ${url}`);
  };
  await import("../background.js?manual");
  async function message(value, ok = true) {
    const response = await new Promise(resolve => listener(value, { id: "test", url: "chrome-extension://test/popup.html" }, resolve));
    assert.equal(response.ok, ok, response.error);
    return response;
  }
  async function run(value) {
    const before = JSON.stringify(data.status);
    await message(value);
    await until(() => data.status?.running || JSON.stringify(data.status) !== before);
    await until(() => !data.status.running);
  }
  await message({ type: "settings", port: 18242, token: "b".repeat(64) });
  await message({ type: "health" });
  await message({ type: "archives" });
  await pause();
  assert.equal(providerCalls, 0, "startup, pairing and archive browsing do not contact ChatGPT");
  assert.equal((await db.page("runs")).total, 0);

  await run({ type: "sync", currentId: "chat-1" });
  assert.equal(writes, 1);
  assert.equal(data.status.state, "completed");
  assert.equal(data.status.unfinished, false);
  assert.equal(data.status.trigger, "manual");
  await run({ type: "sync" });
  assert.equal(writes, 2);
  assert.equal(details, 2);
  assert.equal(data.status.diskSkipped, 1);
  await run({ type: "sync" });
  assert.equal(details, 2, "disk versions skip all existing conversations");
  disk.clear();
  await run({ type: "sync" });
  assert.equal(details, 4, "deleted disk archives are fetched despite prior successful syncs");
  const beforeIdle = providerCalls;
  await import("../background.js?idle-restart");
  await message({ type: "health" });
  await pause();
  assert.equal(providerCalls, beforeIdle);

  updated = 200;
  failIngest = true;
  await run({ type: "sync", full: true });
  assert.match(data.status.error, /Buddy upload.*Failed to fetch/);
  assert.equal(data.status.unfinished, true);
  const failedId = data.status.id;
  const failedKey = await db.get("meta", "activeProgress");
  assert.equal((await db.get("pending", failedKey)).conversation.update_time, 200);
  const failedDetails = details;
  const beforeFailureRestart = providerCalls;
  failIngest = false;
  await import("../background.js?failed-restart");
  await message({ type: "tasks" });
  await pause();
  assert.equal(data.status.state, "failed");
  assert.equal(providerCalls, beforeFailureRestart, "failed uploads do not retry on worker restart");
  await message({ type: "sync" }, false);
  await message({ type: "pause" }, false);
  await message({ type: "resume" }, false);
  await run({ type: "continue" });
  assert.equal(data.status.id, failedId);
  assert.equal(data.status.checked, 2);
  assert.equal(data.status.failed, 0);
  assert.equal(details, failedDetails + 1, "manual continuation reuses the pending first snapshot");
  assert.equal(await db.get("pending", failedKey), undefined);

  // Stop after an in-flight upload; restarting the worker leaves the task stopped.
  let release;
  hold = new Promise(resolve => { release = resolve; });
  await message({ type: "sync", full: true });
  await until(() => data.status.stage === "Saving conversation");
  await message({ type: "sync", currentId: "chat-1" }, false);
  await message({ type: "discard" }, false);
  await message({ type: "stop" });
  release(); hold = null;
  await until(() => data.status.state === "stopped");
  assert.equal(data.status.checked, 1);
  const stoppedId = data.status.id;
  const stoppedWrites = writes;
  const stoppedCalls = providerCalls;
  await import("../background.js?stopped-restart");
  await message({ type: "health" });
  await pause();
  assert.equal(writes, stoppedWrites);
  assert.equal(providerCalls, stoppedCalls);
  await run({ type: "continue" });
  assert.equal(data.status.id, stoppedId);
  assert.equal(data.status.checked, 2);

  // A transaction failure after acknowledgement keeps both the queue and the old cursor.
  const originalPut = IDBObjectStore.prototype.put;
  let abortOnce = true;
  IDBObjectStore.prototype.put = function(value, key) {
    if (this.name === "results" && value.outcome === "updated" && abortOnce) {
      abortOnce = false;
      throw new Error("simulated acknowledgement transaction failure");
    }
    return originalPut.call(this, value, key);
  };
  try { await run({ type: "sync", full: true }); }
  finally { IDBObjectStore.prototype.put = originalPut; }
  assert.match(data.status.error, /simulated acknowledgement transaction failure/);
  const interrupted = await db.get("progress", await db.get("meta", "activeProgress"));
  assert.equal(interrupted.index, 0);
  assert.equal(interrupted.run.checked, 0);
  assert.ok(await db.get("pending", interrupted.key));
  // Reconstruct a worker interrupted during an operation, without allowing it to send anything.
  interrupted.run.state = "running";
  await db.commit([db.put("progress", interrupted.key, interrupted)]);
  const beforeInterruptedRestart = providerCalls;
  await import("../background.js?interrupted-restart");
  await message({ type: "tasks" });
  assert.equal(data.status.state, "interrupted");
  assert.equal(providerCalls, beforeInterruptedRestart);
  await run({ type: "continue" });
  assert.equal(data.status.updated, 2);
  assert.equal(data.status.failed, 0);

  failIngest = true;
  await run({ type: "sync", full: true });
  failIngest = false;
  const mismatchWrites = writes;
  storageId = "elsewhere";
  await run({ type: "continue" });
  assert.match(data.status.error, /destination changed/);
  assert.equal(writes, mismatchWrites);
  storageId = "local";
  accountId = "another-user";
  await run({ type: "continue" });
  assert.match(data.status.error, /another ChatGPT account/);
  assert.equal(writes, mismatchWrites);
  await message({ type: "discard" });
  assert.equal(data.status.state, "discarded");
  assert.equal(data.status.unfinished, false);
  assert.equal(await db.get("meta", "activeProgress"), undefined);
  await message({ type: "continue" }, false);
  assert.equal(data.status.nextSync, undefined);
  assert.equal(data.status.retryAt, undefined);
});
