import { $, date, send, bind, element, link, counts, labels } from "./ui.js";
let offset = 0;
let total = 0;
let active = "archives";
let requestId = 0;
let runOffset = 0;
let catalogTimer;
function detail(list, name, value, className) {
  list.append(element("dt", name), element("dd", value || "—", className));
}
function action(text, callback) {
  const button = element("button", text);
  button.addEventListener("click", async () => {
    $("feedback").textContent = "";
    button.disabled = true;
    try { await callback(); } catch (error) { $("feedback").textContent = error.message; }
    finally { button.disabled = false; }
  });
  return button;
}
async function archives(force = false) {
  clearTimeout(catalogTimer);
  const id = ++requestId;
  $("archiveSummary").textContent = "Reading the receiver’s archive…";
  let data;
  try {
    data = await send({ type: "archives", q: $("search").value, account: $("account").value, offset, limit: 25, refresh: force });
  } catch (error) {
    if (id === requestId) $("archiveSummary").textContent = "Archive list unavailable. Any previous results below have not been refreshed.";
    throw error;
  }
  if (id !== requestId) return;
  if (data.refreshing && active === "archives") catalogTimer = setTimeout(() => archives().catch(error => { $("feedback").textContent = error.message; }), 1000);
  total = data.total;
  const selected = $("account").value;
  $("account").replaceChildren(new Option("All accounts", ""), ...data.accounts.map(a => new Option(a, a)));
  $("account").value = selected;
  $("archives").replaceChildren();
  $("archiveSummary").textContent = `${total} archived conversations · ${data.refreshing ? "Checking disk in background; showing last index" : "Sorted by most recent file save"}`;
  $("warnings").textContent = data.warnings.join("\n");
  for (const row of data.items) {
    const card = element("details", null, "card");
    const summary = element("summary");
    summary.append(element("span", row.title, "archive-title"));
    const meta = element("div", null, "archive-meta");
    meta.append(element("span", `Saved ${date(row.saved_at)}`), element("span", `Markdown: ${labels[row.markdown_status] || row.markdown_status}`));
    summary.append(meta);
    const list = element("dl", null, "detail-grid");
    detail(list, "Account", row.account, "path");
    detail(list, "Conversation updated", date(row.updated_at * 1000));
    detail(list, "Captured", date(row.captured_at));
    detail(list, "JSON", "Saved on disk");
    detail(list, "JSON path", row.path, "path");
    detail(list, "Markdown", `${labels[row.markdown_status] || row.markdown_status}${row.markdown_error ? `: ${row.markdown_error}` : ""}`);
    detail(list, "Markdown path", row.markdown_path, "path");
    const actions = element("div", null, "actions compact");
    actions.append(link("Open original conversation ↗", row.url),
      action("Copy file path", async () => { await navigator.clipboard.writeText(row.path); $("feedback").textContent = "File path copied."; }),
      action("Save again", async () => { await send({ type: "sync", currentId: row.id, account: row.account }); $("feedback").textContent = "Save requested. Check Sync tasks for progress; the matching ChatGPT account must be signed in."; }));
    if (row.markdown_status !== "ready") actions.append(action("Retry Markdown", async () => {
      await send({ type: "retryMarkdown", account: row.account, id: row.id });
      $("feedback").textContent = "Markdown conversion queued. Refresh to check the result.";
    }));
    card.append(summary, list, actions);
    $("archives").append(card);
  }
  if (!data.items.length) $("archives").append(element("p", "No matching archives.", "empty"));
  $("previous").disabled = offset === 0;
  $("next").disabled = offset + 25 >= total;
  $("page").textContent = total ? `${offset + 1}–${Math.min(offset + 25, total)} of ${total}` : "0 results";
}
async function runs() {
  const opened = new Set([...$("runs").querySelectorAll("details[open]")].map(node => node.dataset.taskId));
  const { historyStartedAt, status = {} } = await chrome.storage.local.get(["historyStartedAt", "status"]);
  const taskPage = await send({ type: "tasks", offset: runOffset });
  const runs = taskPage.items;
  $("retention").textContent = `Task details are kept for 90 days after completion. Recording started ${date(historyStartedAt)}; earlier tasks cannot be reconstructed.`;
  $("runs").replaceChildren();
  for (const summaryRun of runs) {
    const run = summaryRun.id === status.id ? { ...summaryRun, ...status } : summaryRun;
    const card = element("details", null, "card");
    card.dataset.taskId = run.id;
    const summary = element("summary");
    summary.append(element("span", `${labels[run.state] || run.state} · ${run.scope} · ${run.trigger}`, "archive-title"),
      element("p", `${date(run.startedAt)} · ${counts(run)}`, "muted"));
    const list = element("dl", null, "detail-grid");
    detail(list, "Task ID", run.id, "path");
    detail(list, "Account", run.account, "path");
    detail(list, "Destination", run.storagePath || run.storageId, "path");
    detail(list, "Started", date(run.startedAt));
    detail(list, "Finished", run.endedAt ? date(run.endedAt) : "Not finished");
    detail(list, "Checked", `${run.checked || 0}${run.total != null ? ` / ${run.total}` : ""}`);
    detail(list, "Details read", String(run.detailsRead || 0));
    detail(list, "Failed", String(run.failed || 0));
    detail(list, "Identical content", String(run.unchanged || 0));
    detail(list, "Already on disk", String(run.diskSkipped || 0));
    detail(list, "Older ignored", String(run.stale || 0));
    if (run.error) detail(list, "Error", run.error, "error");
    const rows = element("div", null, "task-results");
    let itemOffset = 0;
    async function loadResults() {
      const data = await send({ type: "taskResults", taskId: run.id, offset: itemOffset });
      rows.replaceChildren();
      for (const item of data.items) {
        const row = element("div", null, "task-result");
        const heading = element("div", null, "row");
        heading.append(link(item.title, `https://chatgpt.com/c/${encodeURIComponent(item.id)}`), element("span", labels[item.outcome] || item.outcome, "badge"));
        row.append(heading, element("small", date(item.at)));
        if (item.timings) row.append(element("p", Object.entries(item.timings).filter(([, value]) => Number.isFinite(value)).map(([name, value]) => `${({providerMs: "ChatGPT", queueMs: "Queue", receiverMs: "Receiver", jsonSaveMs: "JSON save", commitMs: "Local commit"})[name] || name}: ${(value / 1000).toFixed(2)}s`).join(" · "), "muted"));
        if (item.error) row.append(element("p", item.error, "error"));
        rows.append(row);
      }
      if (!data.items.length) rows.append(element("p", "No conversations processed yet.", "muted"));
      const controls = element("div", null, "row between");
      const previous = action("Previous results", async () => { itemOffset = Math.max(0, itemOffset - 25); await loadResults(); });
      const next = action("Next results", async () => { itemOffset += 25; await loadResults(); });
      previous.disabled = itemOffset === 0;
      next.disabled = itemOffset + 25 >= data.total;
      controls.append(previous, element("small", `${data.total} results`), next);
      rows.append(controls);
    }
    card.addEventListener("toggle", () => {
      if (card.open) loadResults().catch(error => { $("feedback").textContent = error.message; });
    });
    card.append(summary, list, rows);
    $("runs").append(card);
    card.open = opened.has(run.id);
  }
  $("previousRuns").disabled = runOffset === 0;
  $("nextRuns").disabled = runOffset + 25 >= taskPage.total;
  $("runsPage").textContent = runs.length ? `${runOffset + 1}–${Math.min(runOffset + 25, taskPage.total)} of ${taskPage.total}` : "0 tasks";
  if (!runs.length) $("runs").append(element("p", "No sync tasks recorded yet.", "empty"));
}
async function refresh() {
  const { connection = {} } = await chrome.storage.local.get("connection");
  $("destination").textContent = `Current destination: ${connection.storage_path || "Connect to the receiver to inspect its destination."}`;
  if (active === "archives") await archives(); else await runs();
}
function tab(name) {
  active = name;
  clearTimeout(catalogTimer);
  for (const value of ["archives", "runs"]) {
    $(`${value}Panel`).hidden = value !== name;
    $(`${value}Tab`).setAttribute("aria-selected", String(value === name));
  }
  return refresh();
}
bind("archivesTab", () => tab("archives"));
bind("runsTab", () => tab("runs"));
bind("refresh", () => active === "archives" ? archives(true) : refresh());
bind("previousRuns", () => { runOffset = Math.max(0, runOffset - 25); return runs(); });
bind("nextRuns", () => { runOffset += 25; return runs(); });
let refreshTimer;
chrome.storage.onChanged.addListener((changes, area) => {
  if (area !== "local" || active !== "runs" || (!changes.status && !changes.runs)) return;
  clearTimeout(refreshTimer);
  refreshTimer = setTimeout(() => runs().catch(error => { $("feedback").textContent = error.message; }), 300);
});
bind("searchButton", () => { offset = 0; return archives(); });
bind("previous", () => { offset = Math.max(0, offset - 25); return archives(); });
bind("next", () => { offset += 25; return archives(); });
$("account").addEventListener("change", () => $("searchButton").click());
$("search").addEventListener("keydown", event => { if (event.key === "Enter") $("searchButton").click(); });
try { await send({ type: "health" }); await refresh(); } catch (error) { $("feedback").textContent = error.message; }
