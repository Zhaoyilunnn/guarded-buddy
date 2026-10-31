const $ = id => document.getElementById(id);
const config = await chrome.storage.local.get(["port", "token", "autoSync"]);
$("port").value = config.port || 18242;
$("token").value = config.token || "";
$("autoSync").checked = config.autoSync || false;

async function send(message) {
  const result = await chrome.runtime.sendMessage(message);
  if (!result?.ok) throw new Error(result?.error || "Extension unavailable");
}
function action(id, callback) {
  $(id).addEventListener("click", () => callback().catch(error => { $("status").textContent = error.message; }));
}
action("connect", async () => {
  await send({ type: "settings", port: Number($("port").value), token: $("token").value.trim(), autoSync: $("autoSync").checked });
  $("status").textContent = "Connected to buddy.";
});
action("sync", () => send({ type: "sync" }));
action("full", () => send({ type: "sync", full: true }));
action("current", async () => {
  const [tab] = await chrome.tabs.query({ active: true, currentWindow: true });
  const url = new URL(tab.url);
  const match = url.hostname === "chatgpt.com" && url.pathname.match(/\/c\/([a-zA-Z0-9_-]+)(?:\/|$)/);
  if (!match) throw new Error("Open a saved ChatGPT conversation first.");
  await send({ type: "sync", currentId: match[1] });
});
function render(status = {}) {
  $("status").textContent = [
    status.running ? "Syncing…" : "Idle",
    `Saved: ${status.saved || 0} · Unchanged: ${status.skipped || 0}`,
    status.lastSync ? `Last completed: ${new Date(status.lastSync).toLocaleString()}` : "",
    status.error || "",
  ].filter(Boolean).join("\n");
}
render((await chrome.storage.local.get("status")).status);
chrome.storage.onChanged.addListener((changes, area) => { if (area === "local" && changes.status) render(changes.status.newValue); });
