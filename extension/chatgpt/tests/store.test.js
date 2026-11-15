import "fake-indexeddb/auto";
import test from "node:test";
import assert from "node:assert/strict";
import * as db from "../store.js";

test("queue acknowledgement is atomic, task results are paged, and migration is restartable", async () => {
  const legacyRun = { id: "legacy-task", startedAt: 1, checked: 1, results: [{ id: "one", at: 1, outcome: "created" }] };
  const local = { paused: true, token: "synthetic", status: { id: "legacy-task" },
    activeProgress: "progress:legacy", "progress:legacy": { key: "progress:legacy", index: 1, run: legacyRun,
      pending: { account: "a".repeat(64), conversation: { conversation_id: "two", text: "large queued payload" } } },
    runs: [legacyRun], "run:legacy-task": legacyRun,
    "checkpoints:scope": { one: { revision: 1, checkedAt: 2 } } };
  let failCleanup = true;
  globalThis.chrome = { storage: { local: {
    async get(keys) { return structuredClone(keys === null ? local : Object.fromEntries((Array.isArray(keys) ? keys : [keys]).filter(k => k in local).map(k => [k, local[k]]))); },
    async set(values) { Object.assign(local, values); },
    async remove(keys) { if (failCleanup) { failCleanup = false; throw new Error("simulated worker termination"); } for (const key of keys) delete local[key]; },
  } } };
  await assert.rejects(db.migrate(), /simulated worker termination/);
  assert.ok(local["progress:legacy"], "legacy copy survives failed cleanup");
  assert.ok(await db.get("pending", "progress:legacy"));
  assert.equal((await db.get("progress", "progress:legacy")).pending, undefined);
  assert.equal((await db.get("progress", "progress:legacy")).run.results, undefined);
  await db.commit([db.put("results", ["legacy-task", "one"], { id: "one", taskId: "legacy-task", at: 1, outcome: "updated" })]);
  await db.migrate();
  assert.equal(local.activeProgress, undefined);
  assert.equal(local.paused, true);
  assert.equal(local.token, "synthetic");
  assert.equal((await db.page("results", { taskId: "legacy-task" })).items[0].outcome, "updated", "a resumed migration must not overwrite imported records");

  const commands = [db.put("progress", "progress:legacy", { index: 2 }),
    db.put("checkpoints", ["scope", "two"], { revision: 2 }),
    db.put("results", ["legacy-task", "two"], { taskId: "legacy-task", id: "two", at: 2 }),
    db.del("pending", "progress:legacy")];
  await assert.rejects(db.commit([...commands, db.put("meta", undefined, "invalid key")]));
  assert.ok(await db.get("pending", "progress:legacy"), "an aborted acknowledgement retains the payload");
  assert.equal((await db.get("progress", "progress:legacy")).index, 1);
  assert.equal(await db.get("checkpoints", ["scope", "two"]), undefined);
  await db.commit(commands);
  assert.equal(await db.get("pending", "progress:legacy"), undefined);
  assert.equal((await db.get("progress", "progress:legacy")).index, 2);
  assert.equal((await db.get("checkpoints", ["scope", "two"])).revision, 2);

  await db.commit(Array.from({ length: 70 }, (_, i) => db.put("results", ["paged", String(i)], { taskId: "paged", id: String(i), at: i })));
  const first = await db.page("results", { taskId: "paged", limit: 25 });
  const second = await db.page("results", { taskId: "paged", offset: 25, limit: 25 });
  assert.equal(first.total, 70);
  assert.equal(first.items.length, 25);
  assert.equal(second.items.length, 25);
  assert.equal(first.items.some(a => second.items.some(b => a.id === b.id)), false);
  await db.commit([db.put("runs", "paged", { id: "paged", startedAt: 1, endedAt: 2 })]);
  await db.prune(3);
  assert.equal((await db.page("results", { taskId: "paged" })).total, 0);
  assert.ok(await db.get("runs", "legacy-task"), "unfinished tasks are retained");
});
