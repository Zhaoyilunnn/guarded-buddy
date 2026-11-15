export const $ = id => document.getElementById(id);
export const date = value => value ? new Date(value).toLocaleString() : "Never";
export const labels = { created: "New", updated: "Updated", unchanged: "Identical content", stale: "Older revision ignored", checkpointSkipped: "Recently verified", failed: "Failed", completed: "Completed", paused: "Paused", running: "Running", interrupted: "Waiting to resume", discarded: "Discarded", ready: "Ready", pending: "Generating" };
export async function send(message) {
  const response = await chrome.runtime.sendMessage(message);
  if (!response?.ok) throw new Error(response?.error || "Extension unavailable");
  return response.data;
}
export function element(tag, text, className) {
  const node = document.createElement(tag);
  if (text != null) node.textContent = text;
  if (className) node.className = className;
  return node;
}
export function link(text, url) {
  const node = element("a", text);
  node.href = url;
  node.target = "_blank";
  node.rel = "noopener noreferrer";
  return node;
}
export function bind(id, action) {
  $(id).addEventListener("click", async () => {
    $("feedback").textContent = "";
    try { await action(); } catch (error) { $("feedback").textContent = error.message; }
  });
}
export function counts(run = {}) {
  return `${run.created || 0} new · ${run.updated || 0} updated · ${(run.unchanged || 0) + (run.stale || 0) + (run.checkpointSkipped || 0)} unchanged${run.failed ? ` · ${run.failed} failed` : ""}`;
}
