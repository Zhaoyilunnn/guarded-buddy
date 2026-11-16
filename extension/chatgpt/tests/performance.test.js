import "fake-indexeddb/auto";
import test from "node:test";
import assert from "node:assert/strict";
import * as db from "../store.js";

test("snapshots are queued once, stage writes remain small, and unchanged details use bounded verification", async () => {
  const local = { port: 18242, token: "b".repeat(64), status: {} };
  const ids = Array.from({ length: 8 }, (_, i) => `sample-${i}`);
  let listener;
  let details = 0;
  let pendingWrites = 0;
  let bytes = 0;
  let metadataMax = 0;
  const nativePut = IDBObjectStore.prototype.put;
  IDBObjectStore.prototype.put = function(value, key) {
    const size = Buffer.byteLength(JSON.stringify(value));
    bytes += size;
    if (this.name === "pending") pendingWrites++;
    else { metadataMax = Math.max(metadataMax, size); assert.equal(value?.pending, undefined); assert.equal(value?.run?.results, undefined); }
    return nativePut.call(this, value, key);
  };
  const nativeNow = Date.now;
  let clock = nativeNow();
  Date.now = () => clock;
  globalThis.chrome = { storage: { local: {
    async get(keys) { return structuredClone(keys === null ? local : Object.fromEntries((Array.isArray(keys) ? keys : [keys]).filter(k => k in local).map(k => [k, local[k]]))); },
    async set(values) { const size = Buffer.byteLength(JSON.stringify(values)); bytes += size; metadataMax = Math.max(metadataMax, size); assert.ok(size < 65536); Object.assign(local, structuredClone(values)); },
    async remove(keys) { for (const k of Array.isArray(keys) ? keys : [keys]) delete local[k]; },
  } }, runtime: { id: "bench", getURL: p => `chrome-extension://bench/${p}`, onMessage: { addListener(fn) { listener = fn; } } } };
  const reply = value => ({ ok: true, status: 200, json: async () => value });
  globalThis.fetch = async url => {
    if (url.endsWith("/health")) return reply({ app: "buddy", schema_version: 1, storage_id: "bench" });
    if (url.endsWith("/api/auth/session")) return reply({ accessToken: "synthetic", user: { id: "synthetic" } });
    if (url.includes("/conversations?")) return reply({ total: ids.length, items: ids.map(id => ({ id, update_time: 100 })) });
    if (url.includes("/conversation/")) { details++; return reply({ conversation_id: url.split("/").pop(), title: "Synthetic", current_node: "n", mapping: {}, update_time: 100, metadata: "x".repeat(512 * 1024) }); }
    if (url.endsWith("/v1/chatgpt")) return reply({ status: "skipped", save_ms: 0.1 });
    throw new Error("Unexpected endpoint");
  };
  try {
    await import("../background.js?performance");
    async function run(full = false) {
      const previous = local.status.id;
      const response = await new Promise(resolve => listener({ type: "sync", full }, { id: "bench", url: "chrome-extension://bench/popup.html" }, resolve));
      assert.equal(response.ok, true, response.error);
      for (let i = 0; i < 2000; i++) {
        await new Promise(resolve => setTimeout(resolve, 2));
        if (local.status.id !== previous && local.status.state === "completed" && !local.status.running) return;
        if (local.status.error) assert.fail(local.status.error);
      }
      assert.fail("Sync did not finish");
    }
    await run(true);
    await run(true);
    assert.equal(details, 16);
    assert.equal(pendingWrites, 16, "each delivered snapshot is written exactly once");
    assert.ok(metadataMax < 65536, "stage/result writes cannot grow to snapshot size");
    assert.ok(bytes / 1048576 < 9, "two full checks should stay close to their 8 MiB payload total");
    console.log(`Two full checks, 8 × 512 KiB: ${(bytes / 1048576).toFixed(2)} MiB writes; ${pendingWrites} queue writes; max metadata ${(metadataMax / 1024).toFixed(1)} KiB`);
    const firstRun = (await db.page("runs")).items[0];
    const record = (await db.page("results", { taskId: firstRun.id })).items[0];
    assert.ok(Number.isFinite(record.timings.providerMs));
    assert.ok(Number.isFinite(record.timings.queueMs));
    assert.ok(Number.isFinite(record.timings.receiverMs));
    assert.ok(Number.isFinite(record.timings.commitMs));
    clock += 15 * 60000;
    await run();
    assert.equal(details, 16, "a later manual check skips unchanged details");
    clock += 24 * 3600000;
    await run();
    assert.equal(details, 21, "old unchanged details have a five-item verification budget");
    clock += 24 * 3600000;
    await run();
    assert.equal(details, 24, "verification rotates to the remaining items");
    const dbInstance = await db.openStore();
    const pending = await new Promise(resolve => { const request = dbInstance.transaction("pending").objectStore("pending").count(); request.onsuccess = () => resolve(request.result); });
    assert.equal(pending, 0, "confirmed snapshots are removed");
  } finally { Date.now = nativeNow; IDBObjectStore.prototype.put = nativePut; }
});
