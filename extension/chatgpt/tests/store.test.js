import "fake-indexeddb/auto";
import test from "node:test";
import assert from "node:assert/strict";
import * as db from "../store.js";

test("queue acknowledgement is atomic and task results are paged and pruned", async () => {
  await db.commit([
    db.put("runs", "task", { id: "task", startedAt: 1 }),
    db.put("progress", "progress:task", { index: 1 }),
    db.put("pending", "progress:task", { account: "a".repeat(64), conversation: { conversation_id: "two" } }),
  ]);
  const commands = [db.put("progress", "progress:task", { index: 2 }),
    db.put("results", ["task", "two"], { taskId: "task", id: "two", at: 2 }),
    db.del("pending", "progress:task")];
  await assert.rejects(db.commit([...commands, db.put("meta", undefined, "invalid key")]));
  assert.ok(await db.get("pending", "progress:task"), "an aborted acknowledgement retains the payload");
  assert.equal((await db.get("progress", "progress:task")).index, 1);
  await db.commit(commands);
  assert.equal(await db.get("pending", "progress:task"), undefined);
  assert.equal((await db.get("progress", "progress:task")).index, 2);

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
  assert.ok(await db.get("runs", "task"), "unfinished tasks are retained");
});
