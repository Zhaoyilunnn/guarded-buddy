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
