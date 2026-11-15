import * as db from "./store.js";
import { login, listPage, conversation, revision } from "./api.js";

const interval = 15 * 60000;
const retention = 90 * 86400000;
let running = false;
let stopRequested = false;
const defaults = { port: 18242, token: "", paused: false };
async function settings() {
  return { ...defaults, ...await chrome.storage.local.get(Object.keys(defaults)) };
}

async function receiver(config, path, payload) {
  if (!config.token) throw new Error("Paste the token from buddy web pair first.");
  const body = payload ? JSON.stringify(payload) : undefined;
  const size = body ? `${(new TextEncoder().encode(body).byteLength / 1048576).toFixed(1)} MiB` : null;
  let response;
  try {
    response = await fetch(`http://127.0.0.1:${config.port}${path}`, {
      method: payload ? "POST" : "GET",
      headers: { Authorization: `Bearer ${config.token}`, ...(payload ? { "Content-Type": "application/json" } : {}) },
      ...(payload ? { body } : {}),
      signal: AbortSignal.timeout(15000),
    });
  } catch (error) {
    throw new Error(`Buddy ${payload ? `upload (${size})` : "health check"} failed: ${error.message || String(error)}. Check buddy web serve${payload ? " and its web.max_body_mib setting" : ""}.`);
  }
  if (response.status === 413) {
    throw new Error(`Buddy upload (${size}) exceeds the receiver size limit. Increase [web] max_body_mib in ~/.config/buddy/config.toml or use buddy web serve --max-body-mib, then restart the receiver and retry.`);
  }
  if (response.status === 404 && path.startsWith("/v1/archives")) {
    throw new Error("Archive browsing requires the updated Buddy receiver. Reinstall Buddy and restart buddy web serve.");
  }
  if (!response.ok) throw new Error(`Buddy receiver HTTP ${response.status}. Check the token and buddy web serve.`);
  const result = await response.json();
  if (path === "/health" && (result.app !== "buddy" || result.schema_version !== 1)) {
    throw new Error("This port does not host a compatible buddy receiver.");
  }
  return result;
}

async function status(patch) {
  const { status = {} } = await chrome.storage.local.get("status");
  await chrome.storage.local.set({ status: { ...status, ...patch, changedAt: Date.now() } });
}

async function health(config) {
  try {
    const info = await receiver(config, "/health");
    await chrome.storage.local.set({ connection: { ...info, port: config.port, checkedAt: Date.now(), ok: true } });
    return info;
  } catch (error) {
    await chrome.storage.local.set({ connection: { port: config.port, checkedAt: Date.now(), ok: false, error: error.message } });
    throw error;
  }
}

async function publish(progress, extra = {}, commands = []) {
  await db.commit([db.put("progress", progress.key, progress), db.put("runs", progress.run.id, progress.run), ...commands]);
  await status({ ...progress.run, saved: progress.run.created + progress.run.updated,
    skipped: progress.run.unchanged + progress.run.stale + progress.run.checkpointSkipped, ...extra });
}

async function phase(progress, stage, title = null) {
  progress.run.stage = stage;
  progress.run.currentTitle = title;
  progress.run.state = "running";
  progress.run.error = null;
  await publish(progress, { running: true, error: null });
}

async function result(progress, item, outcome, error = null, timings = {}) {
  const previous = await db.get("results", [progress.run.id, item.id]);
  progress.run.failed = Math.max(0, (progress.run.failed || 0) - (previous?.outcome === "failed" ? 1 : 0) + (outcome === "failed" ? 1 : 0));
  return db.put("results", [progress.run.id, item.id], {
    taskId: progress.run.id, id: item.id, title: item.title || item.id, outcome, error, at: Date.now(), timings,
  });
}

async function sync({ currentId = null, full = false, trigger = "manual", expectedAccount = null } = {}) {
  if (running) return { busy: true };
  running = true;
  stopRequested = false;
  let progress;
  let pointer;
  try {
    const config = await settings();
    if (config.paused && !currentId) return { paused: true };
    const pointers = { activeProgress: await db.get("meta", "activeProgress"), currentProgress: await db.get("meta", "currentProgress") };
    pointer = currentId ? "currentProgress" : (pointers.currentProgress ? "currentProgress" : "activeProgress");
    const key = pointers[pointer];
    if (key) progress = await db.get("progress", key);
    if (progress && !progress.run) {
      // Preserve old pagination; counters and task history begin with this upgrade.
      progress = { ...progress, legacy: true };
    }
    if (progress?.run && currentId && progress.currentId !== currentId) {
      throw new Error("Finish the pending single-conversation save before saving another conversation.");
    }
    if (!progress?.run) {
      const id = crypto.randomUUID();
      const legacyScope = progress?.legacy && key?.match(/^progress:([a-f0-9]{64}):(.+)$/);
      progress = { ...progress, key: `progress:${id}`, offset: progress?.offset || 0,
        index: progress?.index || 0, items: progress?.items || null, full: progress?.full || full,
        currentId: progress?.currentId || currentId, expectedAccount,
        run: { id, trigger, scope: (progress?.currentId || currentId) ? "conversation" : (progress?.full || full) ? "full" : "incremental",
          startedAt: Date.now(), endedAt: null, total: progress?.total ?? null, error: null, currentTitle: null, state: "running", stage: "Connecting to Buddy",
          checked: progress?.legacy ? (progress.offset || 0) + (progress.index || 0) : 0,
          previouslyChecked: progress?.legacy ? (progress.offset || 0) + (progress.index || 0) : 0,
          account: legacyScope ? legacyScope[1] : null, storageId: legacyScope ? legacyScope[2] : null,
          created: 0, updated: 0, unchanged: 0, stale: 0, checkpointSkipped: 0, failed: 0, detailsRead: 0, verificationUsed: 0 } };
      await publish(progress, { running: true }, [db.put("meta", pointer, progress.key)]);
      if (key && key !== progress.key) await db.commit([db.del("progress", key)]);
    }
    if (progress.expectedAccount && expectedAccount && progress.expectedAccount !== expectedAccount) {
      throw new Error("The pending task belongs to another account. Finish or discard it first.");
    }
    await phase(progress, "Connecting to Buddy");
    const info = await health(config);
    const storageId = info.storage_id || "legacy";
    if (progress.run.storageId && progress.run.storageId !== storageId) {
      throw new Error("The destination changed during an unfinished task. Restore the previous destination or discard this task before syncing to the new one.");
    }
    progress.run.storageId = storageId;
    progress.run.port = config.port;
    progress.run.storagePath = info.storage_path;
    await phase(progress, "Checking ChatGPT login");
    const auth = await login(wait => phase(progress, "Waiting for ChatGPT login retry", `${Math.ceil(wait / 1000)} seconds`));
    if ((progress.run.account && progress.run.account !== auth.account)
        || (progress.expectedAccount && progress.expectedAccount !== auth.account)
        || (expectedAccount && expectedAccount !== auth.account)) {
      throw new Error("This task belongs to another ChatGPT account. Sign in to that account before continuing.");
    }
    progress.run.account = auth.account;
    const digest = await crypto.subtle.digest("SHA-256", new TextEncoder().encode(config.token));
    const receiverId = [...new Uint8Array(digest)].map(b => b.toString(16).padStart(2, "0")).join("");
    const checkpointKey = `checkpoints:${auth.account}:${config.port}:${receiverId}:${storageId}`;
    progress.run.checkpointScope = checkpointKey;
    const verificationKey = `verification:${checkpointKey}`;
    progress.run.verificationStart ??= await db.get("meta", verificationKey) || 0;
    const legacyPending = await db.get("pending", "legacy");
    if (legacyPending && !stopRequested) {
      if (legacyPending.account !== auth.account) throw new Error("A pending snapshot belongs to another account. Sign in to that account to finish syncing.");
      await receiver(config, "/v1/chatgpt", legacyPending);
      await db.commit([db.del("pending", "legacy")]);
    }
    async function stopped() {
      if (!stopRequested) return false;
      progress.run.state = "paused";
      progress.run.stage = "Paused";
      await publish(progress, { running: false });
      return true;
    }
    if (await stopped()) return;
    while (true) {
      if (!progress.items) {
        await phase(progress, "Getting conversation list");
        if (progress.currentId) { progress.items = [{ id: progress.currentId }]; progress.run.total = 1; }
        else {
          const page = await listPage(auth.token, progress.offset, wait => phase(progress, "Waiting for ChatGPT list retry", `${Math.ceil(wait / 1000)} seconds`));
          progress.items = page.items;
          progress.run.total = page.total;
          if (Number.isFinite(page.total) && progress.run.verificationStart >= page.total) progress.run.verificationStart = 0;
        }
        await publish(progress, { running: true });
      }
      for (; progress.index < progress.items.length;) {
        if (await stopped()) return;
        const item = progress.items[progress.index];
        progress.run.currentId = item.id;
        await phase(progress, "Checking conversation", item.title || item.id);
        const updated = revision(item.update_time);
        const checkpoint = await db.get("checkpoints", [checkpointKey, item.id]);
        const age = Date.now() - checkpoint?.checkedAt;
        let outcome;
        let body;
        let checkpointCommand;
        const timings = {};
        let pending = await db.get("pending", progress.key);
        const verify = !pending && !progress.full && !progress.currentId && checkpoint?.revision === updated
          && progress.offset + progress.index >= progress.run.verificationStart && age >= 24 * 3600000 && (progress.run.verificationUsed || 0) < 5;
        if (!pending && !progress.full && !progress.currentId && updated > 0
            && checkpoint?.revision === updated && age >= 0 && !verify) {
          outcome = "checkpointSkipped";
        } else {
          if (!pending) {
            await phase(progress, "Reading conversation", item.title || item.id);
            const readStart = performance.now();
            body = await conversation(auth.token, item.id, wait => phase(progress, "Waiting for ChatGPT retry", `${Math.ceil(wait / 1000)} seconds`));
            timings.providerMs = performance.now() - readStart;
            progress.run.detailsRead = (progress.run.detailsRead || 0) + 1;
            if (verify) progress.run.verificationUsed = (progress.run.verificationUsed || 0) + 1;
            progress.pendingVerificationPosition = verify ? progress.offset + progress.index : null;
            pending = { schema_version: 1, account: auth.account,
              captured_at: new Date().toISOString(), conversation: body };
            const queueStart = performance.now();
            await db.commit([db.put("pending", progress.key, pending), db.put("progress", progress.key, progress)]);
            timings.queueMs = performance.now() - queueStart;
          }
          if (pending.account !== auth.account || pending.conversation.conversation_id !== item.id) throw new Error("Pending snapshot identity does not match the current task item.");
          body = pending.conversation;
          if (await stopped()) return;
          await phase(progress, "Saving conversation", body.title || item.title || item.id);
          const uploadStart = performance.now();
          const acknowledgement = await receiver(config, "/v1/chatgpt", pending);
          timings.receiverMs = performance.now() - uploadStart;
          timings.jsonSaveMs = acknowledgement.save_ms;
          if (!["created", "updated", "skipped", "stale"].includes(acknowledgement.status)) {
            throw new Error("Buddy did not confirm a recognized persistence result. Upgrade the receiver and retry.");
          }
          outcome = acknowledgement.status === "skipped" ? "unchanged" : acknowledgement.status;
          checkpointCommand = db.put("checkpoints", [checkpointKey, item.id], { revision: revision(body.update_time), checkedAt: Date.now() });
        }
        progress.run[outcome]++;
        progress.run.checked++;
        const resultCommand = await result(progress, { ...item, title: body?.title || item.title }, outcome, null, timings);
        const verificationCommand = progress.pendingVerificationPosition != null
          ? db.put("meta", verificationKey, progress.pendingVerificationPosition + 1) : null;
        progress.pendingVerificationPosition = null;
        progress.index++;
        // Persist the acknowledgement and cursor together before doing more work.
        const commitStart = performance.now();
        await publish(progress, { running: true }, [resultCommand, db.del("pending", progress.key), ...(checkpointCommand ? [checkpointCommand] : []), ...(verificationCommand ? [verificationCommand] : [])]);
        timings.commitMs = performance.now() - commitStart;
        await db.commit([resultCommand]);
        await status({ commitMs: timings.commitMs });
      }
      if (await stopped()) return;
      if (progress.currentId || !progress.items.length ||
          (progress.run.total != null && progress.offset + progress.items.length >= progress.run.total)) break;
      progress = { ...progress, offset: progress.offset + progress.items.length, index: 0, items: null };
      await publish(progress, { running: true });
    }
    progress.run.state = "completed";
    progress.run.stage = "Sync complete";
    progress.run.endedAt = Date.now();
    progress.run.currentTitle = null;
    await db.commit([db.put("runs", progress.run.id, progress.run), db.del("progress", progress.key), db.del("meta", pointer),
      ...(!progress.full && !progress.currentId && (progress.run.verificationUsed || 0) < 5 ? [db.put("meta", verificationKey, 0)] : [])]);
    await db.prune(Date.now() - retention);
    await status({ ...progress.run, running: false, saved: progress.run.created + progress.run.updated,
      skipped: progress.run.unchanged + progress.run.stale + progress.run.checkpointSkipped,
      lastSync: new Date().toISOString(), retryAt: 0, nextSync: Date.now() + interval, error: null });

  } catch (error) {
    const message = error.message || String(error);
    if (progress?.run) {
      // A failed acknowledgement transaction must not publish an advanced in-memory cursor.
      progress = await db.get("progress", progress.key) || progress;
      progress.run.state = "failed";
      progress.run.error = message;
      const item = progress.items?.[progress.index];
      const failedResult = item ? await result(progress, { ...item, title: progress.run.currentTitle || item.title }, "failed", message) : null;
      await publish(progress, { running: false, error: message, retryAt: Date.now() + interval }, failedResult ? [failedResult] : []);
    } else await status({ running: false, error: message, retryAt: Date.now() + interval });
  } finally { running = false; }
}

async function setupAlarm() {
  await chrome.alarms.create("sync", { periodInMinutes: 15 });
  await chrome.alarms.create("resume", { periodInMinutes: 1 });
}

async function tick() {
  const config = await settings();
  if (!config.token || config.paused || running) return;
  const activeProgress = await db.get("meta", "activeProgress");
  const currentProgress = await db.get("meta", "currentProgress");
  const { status = {} } = await chrome.storage.local.get("status");
  if (activeProgress || currentProgress) {
    if (!status.retryAt || status.retryAt <= Date.now()) await sync({ trigger: "recovery" });
  } else if (!status.nextSync || status.nextSync <= Date.now()) await sync({ trigger: "scheduled" });
}

chrome.alarms.onAlarm.addListener(async () => { await ready; await tick(); });
chrome.runtime.onStartup?.addListener(async () => { await ready; await tick(); });
chrome.runtime.onInstalled?.addListener(async () => { await ready; await tick(); });

chrome.runtime.onMessage.addListener((message, sender, respond) => {
  if (sender.id !== chrome.runtime.id || !sender.url?.startsWith(chrome.runtime.getURL(""))) return;
  (async () => {
    await ready;
    if (message.type === "settings") {
      const { port, token } = message;
      if (!Number.isInteger(port) || port < 1 || port > 65535 || !/^[a-f0-9]{64}$/i.test(token)) throw new Error("Use a valid port and the 64-character Buddy pairing token.");
      const config = await settings();
      const activeProgress = await db.get("meta", "activeProgress");
      const currentProgress = await db.get("meta", "currentProgress");
      if ((running || activeProgress || currentProgress) && (config.port !== port || config.token !== token)) {
        throw new Error("Finish or discard pending tasks before changing the connection.");
      }
      await health({ port, token });
      await chrome.storage.local.set({ port, token, paired: true });
      await setupAlarm();
      if (!config.paused) void sync({ trigger: "connection" });
    } else if (message.type === "health") {
      await health(await settings());
    } else if (message.type === "pause") {
      await chrome.storage.local.set({ paused: true });
      stopRequested = true;
      if (!running) await status({ running: false });
    } else if (message.type === "resume") {
      await chrome.storage.local.set({ paused: false });
      if (!running) void sync({ trigger: "recovery" });
    } else if (message.type === "discard") {
      if (running || !(await settings()).paused) throw new Error("Pause and wait for the current save to finish before discarding a task.");
      for (const pointer of ["activeProgress", "currentProgress"]) {
        const key = await db.get("meta", pointer);
        const progress = key && await db.get("progress", key);
        if (progress?.run) {
          progress.run.state = "discarded";
          progress.run.endedAt = Date.now();
          progress.run.stage = "Task discarded; saved files retained";
          await publish(progress, { running: false, retryAt: 0, error: null });
        }
        if (key) await db.commit([db.del("progress", key), db.del("meta", pointer), db.del("pending", key)]);
      }
      await db.commit([db.del("pending", "legacy")]);
    } else if (message.type === "sync") {
      if (running) throw new Error("A sync is already running. Check the current task for progress.");
      if (message.currentId && !/^[a-zA-Z0-9_-]{1,128}$/.test(message.currentId)) throw new Error("Invalid conversation ID.");
      if (message.full) {
        const activeProgress = await db.get("meta", "activeProgress");
        if (activeProgress) throw new Error("Finish or discard the pending history task before rechecking all history.");
      }
      const config = await settings();
      if (config.paused && !message.currentId) throw new Error("Resume automatic archiving before syncing history.");
      if (!running) void sync({ currentId: message.currentId || null, full: message.full === true,
        expectedAccount: message.account || null, trigger: "manual" });
    } else if (message.type === "tasks") {
      return { ok: true, data: await db.page("runs", { offset: Math.max(0, Number(message.offset) || 0), limit: 25 }) };
    } else if (message.type === "taskResults") {
      return { ok: true, data: await db.page("results", { taskId: String(message.taskId), offset: Math.max(0, Number(message.offset) || 0), limit: 25 }) };
    } else if (message.type === "archives") {
      const config = await settings();
      const query = new URLSearchParams({ q: String(message.q || ""), account: String(message.account || ""),
        offset: String(Math.max(0, Number(message.offset) || 0)), limit: String(Math.min(100, Number(message.limit) || 50)), refresh: String(message.refresh === true) });
      return { ok: true, data: await receiver(config, `/v1/archives?${query}`) };
    } else if (message.type === "retryMarkdown") {
      return { ok: true, data: await receiver(await settings(), "/v1/archives/retry", { account: message.account, id: message.id }) };
    } else throw new Error("Unknown extension action.");
    return { ok: true };
  })().then(respond).catch(error => respond({ ok: false, error: error.message }));
  return true;
});

const ready = (async () => {
  const { status: previous = {}, migrationVersion, token } = await chrome.storage.local.get(["status", "autoSync", "migrationVersion", "token"]);
  if (!migrationVersion) {
    await chrome.storage.local.set({ migrationVersion: 1, historyStartedAt: Date.now(),
      migrationNotice: Boolean(token), paused: false });
    await chrome.storage.local.remove("autoSync");
  }
  await status({ running: false, state: previous.running ? "interrupted" : previous.state });
  await db.migrate();
  await db.prune(Date.now() - retention);
  await setupAlarm();
})();
// Startup checks also cover an existing installation whose periodic alarm was absent.
ready.then(() => tick());
