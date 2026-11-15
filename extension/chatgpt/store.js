// Durable queue and task data are separate records; UI state never carries payloads.
let database;
const tables = ["meta", "progress", "pending", "checkpoints", "runs", "results"];
export function openStore() {
  database ||= new Promise((resolve, reject) => {
    const request = indexedDB.open("buddy-chatgpt", 1);
    request.onupgradeneeded = () => {
      for (const name of tables) {
        const store = request.result.createObjectStore(name);
        if (name === "runs") { store.createIndex("startedAt", "startedAt"); store.createIndex("endedAt", "endedAt"); }
        if (name === "results") store.createIndex("taskAt", ["taskId", "at", "id"]);
      }
    };
    request.onsuccess = () => { request.result.onversionchange = () => { request.result.close(); database = null; }; resolve(request.result); };
    request.onerror = () => { database = null; reject(request.error); };
    request.onblocked = () => { database = null; reject(new Error("Close other Buddy extension pages to upgrade its database.")); };
  });
  return database;
}
export async function get(table, key) {
  const db = await openStore();
  return new Promise((resolve, reject) => {
    const request = db.transaction(table).objectStore(table).get(key);
    request.onsuccess = () => resolve(request.result);
    request.onerror = () => reject(request.error);
  });
}
export async function commit(commands) {
  if (!commands.length) return;
  const db = await openStore();
  return new Promise((resolve, reject) => {
    const transaction = db.transaction([...new Set(commands.map(c => c.table))], "readwrite");
    transaction.oncomplete = resolve;
    transaction.onerror = () => reject(transaction.error);
    transaction.onabort = () => reject(transaction.error || new Error("Archive transaction aborted"));
    try {
      for (const command of commands) {
        const store = transaction.objectStore(command.table);
        if (command.delete) store.delete(command.key);
        else store.put(command.value, command.key);
      }
    } catch (error) { transaction.abort(); reject(error); }
  });
}
export const put = (table, key, value) => ({ table, key, value });
export const del = (table, key) => ({ table, key, delete: true });
export async function page(table, { offset = 0, limit = 25, taskId } = {}) {
  const db = await openStore();
  return new Promise((resolve, reject) => {
    const store = db.transaction(table).objectStore(table);
    const index = store.index(table === "runs" ? "startedAt" : "taskAt");
    const range = table === "results" ? IDBKeyRange.bound([taskId, 0, ""], [taskId, Number.MAX_SAFE_INTEGER, "\uffff"]) : null;
    const count = index.count(range);
    const cursor = index.openCursor(range, "prev");
    const items = [];
    let skipped = false;
    cursor.onerror = () => reject(cursor.error);
    cursor.onsuccess = () => {
      const current = cursor.result;
      if (!current || items.length >= limit) { resolve({ items, total: count.result }); return; }
      if (!skipped && offset > 0) { skipped = true; current.advance(offset); return; }
      items.push(current.value);
      current.continue();
    };
  });
}
export async function prune(before) {
  const db = await openStore();
  return new Promise((resolve, reject) => {
    const tx = db.transaction(["runs", "results"], "readwrite");
    tx.oncomplete = resolve;
    tx.onerror = () => reject(tx.error);
    const cursor = tx.objectStore("runs").index("endedAt").openCursor(IDBKeyRange.upperBound(before, true));
    cursor.onsuccess = () => {
      const current = cursor.result;
      if (!current) return;
      if (current.value.endedAt && current.value.endedAt < before) {
        const id = current.value.id;
        current.delete();
        const results = tx.objectStore("results").index("taskAt").openCursor(IDBKeyRange.bound([id, 0, ""], [id, Number.MAX_SAFE_INTEGER, "\uffff"]));
        results.onsuccess = () => { if (results.result) { results.result.delete(); results.result.continue(); } };
      }
      current.continue();
    };
  });
}
export async function migrate() {
  if ((await chrome.storage.local.get("storageVersion")).storageVersion === 2) return;
  const local = await chrome.storage.local.get(null);
  const oldKeys = Object.keys(local).filter(k => /^(progress:|checkpoints:|run:)/.test(k) || ["runs", "pending", "activeProgress", "currentProgress"].includes(k));
  if (!await get("meta", "migrated")) {
    const commands = [];
    const addRun = run => {
      const { results = [], ...summary } = run;
      commands.push(put("runs", run.id, summary));
      for (const result of results) commands.push(put("results", [run.id, result.id], { ...result, taskId: run.id }));
    };
    for (const key of oldKeys) {
      const value = local[key];
      if (key.startsWith("run:")) addRun(value);
      else if (key === "runs") {
        for (const summary of value) if (!local[`run:${summary.id}`]) addRun(summary);
      } else if (key.startsWith("progress:")) {
        const { pending, run, ...progress } = value;
        const summary = run && { ...run };
        if (summary) { delete summary.results; addRun(run); }
        commands.push(put("progress", key, { ...progress, ...(summary ? { run: summary } : {}) }));
        if (pending) commands.push(put("pending", key, pending));
      } else if (key.startsWith("checkpoints:")) {
        for (const [id, checkpoint] of Object.entries(value)) commands.push(put("checkpoints", [key, id], checkpoint));
      } else if (["activeProgress", "currentProgress"].includes(key)) commands.push(put("meta", key, value));
      else if (key === "pending") commands.push(put("pending", "legacy", value));
    }
    commands.push(put("meta", "migrated", true));
    await commit(commands);
  }
  // Remove legacy data only after the entire import transaction commits.
  await chrome.storage.local.remove(oldKeys);
  await chrome.storage.local.set({ storageVersion: 2 });
}
