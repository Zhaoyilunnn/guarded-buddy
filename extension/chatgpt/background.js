import { login, listPage, conversation, revision } from "./api.js";

let running = false;
const defaults = { port: 18242, token: "", autoSync: false };

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
  if (!response.ok) throw new Error(`Buddy receiver HTTP ${response.status}. Check the token and buddy web serve.`);
  const result = await response.json();
  if (path === "/health" && (result.app !== "buddy" || result.schema_version !== 1)) {
    throw new Error("This port does not host a compatible buddy receiver.");
  }
  return result;
}

async function status(patch) {
  const { status = {} } = await chrome.storage.local.get("status");
  await chrome.storage.local.set({ status: { ...status, ...patch } });
}

async function sync({ currentId = null, full = false, resume = false } = {}) {
  if (running) return;
  running = true;
  let saved = 0;
  let skipped = 0;
  try {
    const config = await settings();
    await status({ running: true, error: null, saved, skipped });
    const health = await receiver(config, "/health");
    const storageId = health.storage_id || "legacy";
    const auth = await login();
    // A pending payload survives an offline receiver or worker suspension.
    const { pending } = await chrome.storage.local.get("pending");
    if (pending) {
      if (pending.account !== auth.account) throw new Error("A pending snapshot belongs to another account. Sign in to that account to finish syncing.");
      await receiver(config, "/v1/chatgpt", pending);
      await chrome.storage.local.remove("pending");
    }
    const progressKey = `progress:${auth.account}:${storageId}`;
    let { [progressKey]: progress } = await chrome.storage.local.get(progressKey);
    if (!resume || !progress) progress = { offset: 0, index: 0, items: null, full, currentId };
    full = progress.full;
    currentId = progress.currentId;
    await chrome.storage.local.set({ [progressKey]: progress, activeProgress: progressKey });
    const tokenDigest = await crypto.subtle.digest("SHA-256", new TextEncoder().encode(config.token));
    const receiverId = [...new Uint8Array(tokenDigest)].map(b => b.toString(16).padStart(2, "0")).join("");
    const checkpointKey = `checkpoints:${auth.account}:${config.port}:${receiverId}:${storageId}`;
    const stored = await chrome.storage.local.get(checkpointKey);
    const checkpoints = stored[checkpointKey] || {};
    while (true) {
      if (!progress.items) {
        if (currentId) progress.items = [{ id: currentId }];
        else {
          const page = await listPage(auth.token, progress.offset);
          progress.items = page.items;
          progress.total = page.total;
        }
        await chrome.storage.local.set({ [progressKey]: progress });
      }
      for (; progress.index < progress.items.length; progress.index++) {
        const item = progress.items[progress.index];
        const updated = revision(item.update_time);
        if (!full && !currentId && updated > 0 && checkpoints[item.id] === updated) skipped++;
        else {
          const body = await conversation(auth.token, item.id);
          const payload = { schema_version: 1, account: auth.account, captured_at: new Date().toISOString(), conversation: body };
          await chrome.storage.local.set({ pending: payload });
          const acknowledgement = await receiver(config, "/v1/chatgpt", payload);
          checkpoints[item.id] = revision(body.update_time);
          await chrome.storage.local.set({ [checkpointKey]: checkpoints });
          await chrome.storage.local.remove("pending");
          if (acknowledgement.status === "created" || acknowledgement.status === "updated") saved++;
          else skipped++;
        }
        await chrome.storage.local.set({ [progressKey]: { ...progress, index: progress.index + 1 } });
        await status({ saved, skipped });
      }
      if (currentId || progress.items.length === 0 || (progress.total != null && progress.offset + progress.items.length >= progress.total)) break;
      progress = { ...progress, offset: progress.offset + progress.items.length, index: 0, items: null };
      await chrome.storage.local.set({ [progressKey]: progress });
    }
    await chrome.storage.local.remove([progressKey, "activeProgress"]);
    await status({ running: false, saved, skipped, lastSync: new Date().toISOString(), error: null, retryAt: 0 });
  } catch (error) {
    await status({ running: false, saved, skipped, error: error.message || String(error), retryAt: Date.now() + 15 * 60000 });
  } finally { running = false; }
}

async function setupAlarm() {
  const config = await settings();
  await chrome.alarms.clear("sync");
  if (config.autoSync) await chrome.alarms.create("sync", { periodInMinutes: 15 });
  // A separate alarm resumes incomplete work after service-worker termination.
  await chrome.alarms.create("resume", { periodInMinutes: 1 });
}

chrome.alarms.onAlarm.addListener(async alarm => {
  if (alarm.name === "sync") await sync({ resume: true });
  if (alarm.name === "resume") {
    const { activeProgress, status = {} } = await chrome.storage.local.get(["activeProgress", "status"]);
    if (activeProgress && (!status.retryAt || status.retryAt <= Date.now())) await sync({ resume: true });
  }
});

chrome.runtime.onMessage.addListener((message, sender, respond) => {
  if (sender.id !== chrome.runtime.id || !sender.url?.startsWith(chrome.runtime.getURL(""))) return;
  (async () => {
    if (message.type === "settings") {
      const { port, token, autoSync } = message;
      if (!Number.isInteger(port) || port < 1 || port > 65535 || !/^[a-f0-9]{64}$/i.test(token) || typeof autoSync !== "boolean") throw new Error("Use a valid port and the 64-character buddy pairing token.");
      await chrome.storage.local.set({ port, token, autoSync });
      await setupAlarm();
      await receiver({ port, token }, "/health");
    } else if (message.type === "sync") {
      if (message.currentId && !/^[a-zA-Z0-9_-]{1,128}$/.test(message.currentId)) throw new Error("Invalid conversation ID.");
      void sync({ currentId: message.currentId || null, full: message.full === true });
    }
    respond({ ok: true });
  })().catch(error => respond({ ok: false, error: error.message }));
  return true;
});

// The persisted status may refer to a worker that was terminated.
chrome.storage.local.get("status").then(({ status = {} }) => chrome.storage.local.set({ status: { ...status, running: false } }));
setupAlarm();
