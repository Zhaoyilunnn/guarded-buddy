import { $, date, send, bind, element, link, labels } from "./ui.js";
let state = {};
let showSettings = false;
let recentLoading = false;
function render() {
  const { status: task = {}, connection = {}, token } = state;
  $("setup").hidden = Boolean(token) && !showSettings;
  $("dashboard").hidden = !token;
  $("headline").textContent = task.stopping ? "Stopping after the current operation…"
    : task.running ? "Archiving conversations" : task.unfinished ? "Unfinished sync"
    : connection.ok === false ? "Receiver needs attention" : "Manual sync";
  $("syncHint").textContent = task.running ? "This task continues when you close the popup."
    : task.unfinished ? "Click Continue sync to retry or resume. Nothing runs automatically."
    : "Choose Save current conversation or Sync now to start.";
  $("taskTitle").textContent = task.running ? task.stage : labels[task.state] || "Waiting for first sync";
  $("taskDetail").textContent = task.id ? `${task.checked || 0}${Number.isFinite(task.total) ? ` / ${task.total}` : ""} checked · ${task.detailsRead || 0} details read${task.failed ? ` · ${task.failed} failed` : ""} · ${task.endedAt ? `Finished ${date(task.endedAt)}` : `Started ${date(task.startedAt)}`}` : "Connect to begin archiving.";
  $("progress").hidden = !task.running;
  if (Number.isFinite(task.total) && task.total > 0) {
    $("progress").max = task.total;
    $("progress").value = Math.min(task.checked || 0, task.total);
  } else $("progress").removeAttribute("value");
  $("currentTitle").textContent = task.running ? task.currentTitle || "" : "";
  $("newCount").textContent = task.created || 0;
  $("updatedCount").textContent = task.updated || 0;
  $("unchangedCount").textContent = (task.unchanged || 0) + (task.stale || 0) + (task.diskSkipped || 0);
  $("taskError").textContent = task.error || "";
  $("activity").textContent = task.running ? `Last progress: ${date(task.changedAt)}`
    : task.lastSync ? `Last successful sync: ${date(task.lastSync)}` : "No successful sync recorded yet.";
  $("sync").disabled = Boolean(task.running);
  $("sync").textContent = task.running ? "Syncing…" : task.unfinished ? "Continue sync" : "Sync now";
  $("current").disabled = Boolean(task.running || task.unfinished);
  $("full").disabled = Boolean(task.running || task.unfinished);
  $("discard").disabled = Boolean(task.running || !task.unfinished);
  $("stop").hidden = !task.running;
  $("stop").disabled = Boolean(task.stopping);
  $("receiverLabel").textContent = `Local receiver · ${connection.ok ? "Last check passed" : "Not connected"}`;
  $("connectionStatus").textContent = `127.0.0.1:${state.port || 18242} · Checked ${date(connection.checkedAt)}${connection.error ? ` · ${connection.error}` : ""}`;
  $("destinationPreview").textContent = connection.storage_path ? `Saving to ${connection.storage_path}` : "";
  $("storagePath").textContent = connection.storage_path || "Connect to see the actual storage directory.";
  $("cloudNote").textContent = connection.shared_directory ? "Configured archive directory. Your cloud client may replicate these files." : "Local storage. Any external cloud replication is managed by your cloud client.";
}
async function recent() {
  if (recentLoading || !state.token) return;
  recentLoading = true;
  try {
    const data = await send({ type: "archives", limit: 4 });
    $("recent").replaceChildren();
    for (const row of data.items) {
      const node = element("div", null, "recent-item");
      const text = element("div");
      text.append(link(row.title, row.url), element("small", date(row.saved_at)));
      node.append(text, element("span", `Markdown: ${labels[row.markdown_status] || row.markdown_status}`, "badge"));
      $("recent").append(node);
    }
    if (!data.items.length) $("recent").append(element("p", "No archived conversations yet.", "muted"));
    $("archiveError").textContent = data.warnings.length ? "Some archive files could not be read. View all for details." : "";
  } catch (error) { $("archiveError").textContent = `Archive list unavailable: ${error.message}`; }
  finally { recentLoading = false; }
}
async function refresh() {
  state = await chrome.storage.local.get(["port", "token", "status", "connection"]);
  render();
}
await refresh();
$("port").value = state.port || 18242;
$("token").value = state.token || "";
bind("settingsToggle", () => { showSettings = !showSettings; render(); });
bind("connect", async () => {
  await send({ type: "settings", port: Number($("port").value), token: $("token").value.trim() });
  showSettings = false;
  await refresh();
  await recent();
});
bind("sync", () => send({ type: state.status?.unfinished ? "continue" : "sync" }));
bind("full", () => send({ type: "sync", full: true }));
bind("current", async () => {
  const [tab] = await chrome.tabs.query({ active: true, currentWindow: true });
  const url = new URL(tab?.url || "https://invalid.local");
  const match = url.hostname === "chatgpt.com" && url.pathname.match(/^\/c\/([a-zA-Z0-9_-]+)(?:\/|$)/);
  if (!match) throw new Error("Open a saved ChatGPT conversation first.");
  await send({ type: "sync", currentId: match[1] });
});
bind("stop", () => send({ type: "stop" }));
bind("history", () => chrome.tabs.create({ url: chrome.runtime.getURL("history.html") }));
bind("check", async () => { await send({ type: "health" }); await recent(); });
bind("more", () => { $("moreActions").hidden = !$("moreActions").hidden; });
bind("discard", () => send({ type: "discard" }));
chrome.storage.onChanged.addListener(async (changes, area) => {
  if (area !== "local") return;
  await refresh();
  if (changes.status && !changes.status.newValue?.running) void recent();
});
if (state.token) send({ type: "health" }).then(recent).catch(error => { $("archiveError").textContent = error.message; });
setInterval(() => { render(); void recent(); }, 15000);
