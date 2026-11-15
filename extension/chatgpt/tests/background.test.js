import "fake-indexeddb/auto";
import * as db from "../store.js";
import test from "node:test";
import assert from "node:assert/strict";
const pause = () => new Promise(resolve => setTimeout(resolve, 2));
async function until(check) {
  for (let i = 0; i < 500; i++) { if (await check()) return; await pause(); }
  assert.fail("Timed out waiting for extension state");
}

test("automatic archive lifecycle, recovery, pause, history and destination isolation", async () => {
  const data = { port: 18242, autoSync: false };
  const alarms = new Map();
  let messageListener;
  let alarmListener;
  let details = 0;
  let writes = 0;
  let failIngest = false;
  let ingestStatus = 200;
  let updated = 100;
  let storageId = "local";
  let accountId = "user-1";
  let acknowledgement = "created";
  let currentNode = "node";
  let lastPayload;
  let hold;
  let ids = ["chat-1"];
  globalThis.chrome = {
    storage: { local: {
      async get(keys) {
        if (keys === null) keys = Object.keys(data);
        if (typeof keys === "string") keys = [keys];
        return Object.fromEntries(keys.filter(k => k in data).map(k => [k, structuredClone(data[k])]));
      },
      async set(values) { Object.assign(data, structuredClone(values)); },
      async remove(keys) { for (const key of Array.isArray(keys) ? keys : [keys]) delete data[key]; },
    } },
    alarms: { async clear() {}, async create(name, options) { alarms.set(name, options); }, onAlarm: { addListener(fn) { alarmListener = fn; } } },
    runtime: { id: "test", getURL: path => `chrome-extension://test/${path}`, onMessage: { addListener(fn) { messageListener = fn; } } },
  };
  globalThis.fetch = async (url, options = {}) => {
    const reply = value => ({ ok: true, status: 200, json: async () => value });
    if (url.endsWith("/health")) return reply({ app: "buddy", schema_version: 1, storage_id: storageId, storage_path: "/archive/chatgpt" });
    if (url.endsWith("/api/auth/session")) return reply({ accessToken: "provider-secret", user: { id: accountId } });
    if (url.includes("/conversations?")) return reply({ total: ids.length, items: ids.map(id => ({ id, title: `Title ${id}`, update_time: updated })) });
    if (url.includes("/conversation/")) {
      details++;
      const id = url.split("/").pop();
      return reply({ conversation_id: id, title: `Title ${id}`, mapping: {}, current_node: currentNode, update_time: updated });
    }
    if (url.includes("/v1/archives?")) return reply({ total: 1, items: [{ id: "chat-1" }] });
    if (url.endsWith("/v1/archives/retry")) return reply({ status: "queued" });
    if (url.endsWith("/v1/chatgpt")) {
      assert.equal(options.headers.Authorization, `Bearer ${"b".repeat(64)}`);
      const payload = JSON.parse(options.body);
      lastPayload = payload;
      assert.equal(payload.account.length, 64);
      assert.equal(options.body.includes("provider-secret"), false);
      if (hold) await hold;
      if (failIngest) throw new TypeError("Failed to fetch");
      if (ingestStatus !== 200) return { ok: false, status: ingestStatus };
      writes++;
      return reply({ status: acknowledgement });
    }
    throw new Error(`Unexpected URL: ${url}`);
  };
  await import("../background.js?first");
  await until(() => alarms.has("sync"));
  assert.equal(alarms.get("sync").periodInMinutes, 15);
  assert.equal(data.autoSync, undefined);
  async function message(value, ok = true) {
    const response = await new Promise(resolve => messageListener(value, { id: "test", url: "chrome-extension://test/popup.html" }, resolve));
    assert.equal(response.ok, ok, response.error);
    return response;
  }
  async function send(value) {
    const before = JSON.stringify(data.status);
    await message(value);
    await until(() => JSON.stringify(data.status) !== before || data.status?.running);
    await pause();
    await until(() => data.status && !data.status.running);
  }
  await send({ type: "settings", port: 18242, token: "b".repeat(64) });
  assert.equal(writes, 1, "pairing starts the first automatic run");
  assert.equal(details, 1);
  assert.equal((await db.get("meta", "activeProgress")), undefined);
  assert.equal(data.status.created, 1);
  assert.equal(data.status.state, "completed");
  assert.ok(data.status.nextSync > Date.now());
  assert.equal(data.connection.storage_path, "/archive/chatgpt");
  assert.equal((await db.page("runs")).total, 1);
  const first = { results: (await db.page("results", { taskId: data.status.id })).items };
  assert.equal(first.results[0].title, "Title chat-1");
  assert.equal(first.results[0].outcome, "created");
  assert.equal(JSON.stringify(first).includes("provider-secret"), false);
  await send({ type: "sync" });
  assert.equal(writes, 1);
  assert.equal(data.status.checkpointSkipped, 1);
  await send({ type: "sync", currentId: "chat-1" });
  assert.equal(writes, 2);
  assert.equal(details, 2);
  updated = 200;
  failIngest = true;
  await send({ type: "sync" });
  assert.match(data.status.error, /Buddy upload.*MiB.*Failed to fetch/);
  const failedKey = (await db.get("meta", "activeProgress"));
  assert.equal((await db.get("pending", failedKey)).conversation.update_time, 200);
  const failedId = data.status.id;
  assert.ok(data.status.retryAt > Date.now());
  failIngest = false;
  data.status.retryAt = 0;
  await import("../background.js?restarted");
  await until(() => data.status.state === "completed");
  assert.equal(data.status.id, failedId, "recovery keeps the same task");
  assert.equal((await db.get("meta", "activeProgress")), undefined);
  assert.equal(data.status.error, null);
  assert.equal((await db.page("results", { taskId: failedId })).total, 1);
  assert.equal(writes, 3, "the durable pending item is delivered once during recovery");
  await send({ type: "sync", full: true });
  assert.equal(data.status.saved, 1);
  ingestStatus = 413;
  await send({ type: "sync", full: true });
  assert.match(data.status.error, /exceeds.*size limit/);
  assert.ok((await db.get("pending", await db.get("meta", "activeProgress"))));
  ingestStatus = 200;
  await send({ type: "sync" });
  assert.equal(data.status.error, null);
  assert.equal((await db.get("meta", "activeProgress")), undefined);
  const beforeSwitch = details;
  storageId = "shared";
  await send({ type: "sync" });
  assert.equal(details, beforeSwitch + 1, "new destinations cannot reuse old checkpoints");
  await send({ type: "sync" });
  assert.equal(details, beforeSwitch + 1);
  acknowledgement = "stale";
  await send({ type: "sync", full: true });
  assert.equal(data.status.saved, 0);
  assert.equal(data.status.stale, 1);
  const checkpointKey = data.status.checkpointScope;
  const beforeRecheck = details;
  currentNode = "new-message-with-same-update-time";
  await db.commit([db.put("checkpoints", [checkpointKey, "chat-1"], { revision: updated, checkedAt: Date.now() - 24 * 3600000 })]);
  acknowledgement = "updated";
  await send({ type: "sync" });
  assert.equal(details, beforeRecheck + 1);
  assert.equal(lastPayload.conversation.current_node, currentNode);
  assert.equal(data.status.updated, 1);
  await db.commit([db.put("checkpoints", [checkpointKey, "chat-1"], updated)]);
  await send({ type: "sync" });
  assert.equal(details, beforeRecheck + 2, "legacy numeric checkpoints are revalidated");

  // Pause while an upload is in flight: acknowledge it, but do not start the next item.
  ids = ["chat-1", "chat-2"];
  let release;
  hold = new Promise(resolve => { release = resolve; });
  await message({ type: "sync", full: true });
  await until(() => data.status.stage === "Saving conversation");
  await message({ type: "sync", currentId: "chat-1" }, false);
  await message({ type: "discard" }, false);
  await message({ type: "pause" });
  release(); hold = null;
  await until(() => !data.status.running);
  assert.equal(data.status.state, "paused");
  assert.equal(data.status.checked, 1);
  const pausedId = data.status.id;
  const beforePause = writes;
  await alarmListener({ name: "sync" });
  await alarmListener({ name: "resume" });
  assert.equal(writes, beforePause);
  assert.equal(data.paused, true);
  await send({ type: "sync", currentId: "chat-1" });
  assert.equal(data.paused, true, "a manual single save does not unpause scheduled work");
  assert.ok((await db.get("meta", "activeProgress")), "the paused history task is preserved");
  await import("../background.js?paused-with-progress");
  await pause();
  assert.equal(data.paused, true);
  await send({ type: "resume" });
  assert.equal(data.status.id, pausedId);
  assert.equal(data.status.checked, 2, "counts survive a pause and a separate single save");
  assert.equal(data.status.updated, 2);
  assert.equal((await db.get("meta", "activeProgress")), undefined);

  // Simulate a transaction failure after receiver acknowledgement: neither cursor nor counts may advance.
  const originalPut = IDBObjectStore.prototype.put;
  let abortOnce = true;
  IDBObjectStore.prototype.put = function(value, key) {
    if (this.name === "results" && value.outcome === "updated" && abortOnce) {
      abortOnce = false;
      throw new Error("simulated acknowledgement transaction failure");
    }
    return originalPut.call(this, value, key);
  };
  try { await send({ type: "sync", full: true }); }
  finally { IDBObjectStore.prototype.put = originalPut; }
  assert.match(data.status.error, /simulated acknowledgement transaction failure/);
  const interrupted = await db.get("progress", await db.get("meta", "activeProgress"));
  assert.equal(interrupted.index, 0);
  assert.equal(interrupted.run.checked, 0);
  assert.ok(await db.get("pending", interrupted.key));
  await send({ type: "sync" });
  assert.equal(data.status.checked, 2);
  assert.equal(data.status.updated, 2);
  assert.equal(data.status.failed, 0);

  // An unfinished task cannot silently deliver its queued body to a new destination/account.
  failIngest = true;
  await send({ type: "sync", full: true });
  const taskId = data.status.id;
  failIngest = false;
  storageId = "third";
  const beforeMismatch = writes;
  await send({ type: "sync" });
  assert.match(data.status.error, /destination changed/);
  assert.equal(writes, beforeMismatch);
  storageId = "shared";
  accountId = "other-user";
  await send({ type: "sync" });
  assert.match(data.status.error, /another ChatGPT account/);
  assert.equal(data.status.id, taskId);
  assert.equal(writes, beforeMismatch);
  await message({ type: "pause" });
  await send({ type: "discard" });
  assert.equal((await db.get("meta", "activeProgress")), undefined);
  assert.equal((await db.get("runs", taskId)).state, "discarded");
  assert.equal(data.paused, true);
  const response = await message({ type: "archives", q: "Title" });
  assert.equal(response.data.total, 1);
  await message({ type: "retryMarkdown", account: "a".repeat(64), id: "chat-1" });

  // Browser restart preserves an explicit pause; migration enables old opt-out installations once.
  await import("../background.js?paused-restart");
  await pause();
  assert.equal(data.paused, true);
  assert.equal(writes, beforeMismatch);
  data.migrationVersion = undefined;
  data.autoSync = false;
  data.status.nextSync = Date.now() + 60000;
  await import("../background.js?legacy-upgrade");
  await until(() => data.migrationVersion === 1);
  assert.equal(data.paused, false);
  assert.equal(data.autoSync, undefined);
  assert.equal(data.migrationNotice, true);
  await message({ type: "health" });
  // Scheduled work runs without any opt-in; completed task details expire after 90 days.
  const expired = { id: "expired", endedAt: Date.now() - 91 * 86400000 };
  await db.commit([db.put("runs", "expired", { ...expired, startedAt: 1 })]);
  data.status.nextSync = Date.now() - 1;
  const beforeScheduled = writes;
  await alarmListener({ name: "sync" });
  assert.ok(writes > beforeScheduled);
  assert.equal(data.status.trigger, "scheduled");
  assert.equal(await db.get("runs", "expired"), undefined);

  // Upgrade an old durable queue without losing its pagination or account/destination scope.
  const legacyKey = `progress:${lastPayload.account}:${storageId}`;
  data[legacyKey] = { offset: 0, index: 1, items: ids.map(id => ({ id })), total: 2, full: true, currentId: null };
  data.activeProgress = legacyKey;
  data.storageVersion = undefined;
  await db.commit([db.del("meta", "migrated")]);
  data.pending = { ...lastPayload, conversation: { ...lastPayload.conversation, conversation_id: "chat-2" } };
  const beforeLegacyDetails = details;
  data.status.retryAt = 0;
  await import("../background.js?legacy-queue");
  await message({ type: "health" });
  await until(async () => (await db.get("meta", "activeProgress")) === undefined);
  assert.equal(data.pending, undefined);
  assert.equal(data[legacyKey], undefined);
  assert.equal(details, beforeLegacyDetails + 1, "old pagination resumes at its saved index");
  assert.equal(data.status.state, "completed");

  acknowledgement = "unexpected";
  await send({ type: "sync", full: true });
  assert.match(data.status.error, /recognized persistence result/);
  assert.equal(data.status.saved, 0, "unrecognized acknowledgements never count as saved");
  assert.equal(data.status.failed, 1);
  await message({ type: "pause" });
  await send({ type: "discard" });

});
