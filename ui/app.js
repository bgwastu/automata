import { $, appLink, renderApps, renderHeader, renderLogContainers, renderOverview, renderSnapshots, snapshotMessage, status, statusHtml, time } from "/api/dashboard/view.js";

import { followOutput } from "/api/dashboard/logs.js";

const views = ["overview", "logs", "task", "backups"];
const tabs = [...document.querySelectorAll("[data-view]")];
const appApi = (name) => `/api/apps/${encodeURIComponent(name)}`;
let view, current, activeView, panelKey, cleanupPanel, syncRequest;
let refreshing = false;
let refreshAgain = false;
let actionPending = false;
let snapshotApp;
let stale = false;

function route() {
  const hash = new URLSearchParams(location.hash.slice(1));
  return { name: hash.get("app"), tab: hash.get("view"), container: hash.get("container") };
}

function navigate(hash, focus = false) {
  if (location.hash !== hash) history.pushState(null, "", hash || location.pathname + location.search);
  render();
  if (focus) {
    const target = route().name ? (current ? $("app-name") : $("unavailable")) : $("apps-title");
    target.focus();
    window.scrollTo(0, 0);
  }
}

function goTo(name, tab) {
  navigate(appLink(name, tab));
}

function orderedApps() {
  const rank = (app) => {
    const runtime = status(app, view.runtime_error);
    if (runtime.tone === "bad" || app.failed || app.error) return 0;
    if (runtime.tone === "busy" || app.busy?.startsWith("deploy")) return 1;
    return runtime.tone === "good" ? 2 : 3;
  };
  return [...view.apps].sort((a, b) => rank(a) - rank(b) || a.name.localeCompare(b.name));
}

function render() {
  const { name, tab, container } = route();
  if (!view) return;

  const apps = orderedApps();
  current = apps.find((app) => app.name === name);
  activeView = views.includes(tab) && (tab !== "backups" || current?.backup) ? tab : "overview";

  renderHeader(view);
  if (!name) renderApps(apps, view);
  $("inventory").hidden = !!name;
  const pending = view.infra.requested > view.infra.completed || syncRequest > view.infra.completed;
  $("sync").disabled = actionPending || pending;
  $("sync-label").textContent = pending ? "Syncing…" : "Sync";
  if (syncRequest !== undefined && view.infra.completed >= syncRequest) {
    syncRequest = undefined;
    const applied = view.infra.applied?.commit?.slice(0, 7);
    const detail = applied ? " Last applied commit: " + applied + "." : "";
    feedback((view.infra.result ? "Sync completed." : "Sync completed with issues.") + detail, view.infra.result ? "" : "bad");
  }
  $("workspace").hidden = !current;
  $("unavailable").hidden = !name || !!current;
  const home = document.querySelector(".home-link");
  if (!name) home.setAttribute("aria-current", "page");
  else home.removeAttribute("aria-current");
  if (name && !current) $("unavailable").textContent = `App “${name}” is unavailable.`;

  const id = activeView === "logs" ? container : null;
  const selected = id && current?.containers.find((item) => item.id === id);
  if (current) renderWorkspace(current, id);
  const key = current && JSON.stringify([
    current.name, activeView, id,
    activeView === "logs" ? (id ? selected?.state : current.rev) : activeView === "backups" ? current.backup.last : null,
  ]);
  if (key !== panelKey) {
    cleanupPanel?.();
    panelKey = key;
    cleanupPanel = current && openPanel(current, activeView, id, selected);
  }
}

function renderWorkspace(app, container) {
  $("app-name").textContent = app.name;
  $("app-status").innerHTML = statusHtml(status(app, view.runtime_error));
  $("app-error").hidden = !app.error && !app.failed;
  $("incident-title").textContent = app.failed ? "Latest deploy failed" : "App needs attention";
  $("incident-detail").textContent = app.error || "";
  $("incident-output").hidden = !app.failed;
  $("incident-output").href = appLink(app.name, "task");
  $("restart").disabled = actionPending || !!app.busy || !app.rev;
  $("backup").hidden = !app.backup;
  $("backup").disabled = $("restart").disabled;
  for (const link of tabs) {
    const tab = link.dataset.view;
    link.href = appLink(app.name, tab);
    link.hidden = tab === "backups" && !app.backup;
    if (tab === activeView) link.setAttribute("aria-current", "page");
    else link.removeAttribute("aria-current");
  }
  $("overview").hidden = activeView !== "overview";
  $("output").hidden = !["logs", "task"].includes(activeView);
  $("log-container-label").hidden = activeView !== "logs";
  if (activeView === "logs") renderLogContainers(app, container);
  $("snapshots").hidden = activeView !== "backups";
  if (activeView === "overview") renderOverview(app, view);
  if (activeView === "backups") {
    const { last, next, schedule } = app.backup;
    $("backup-summary").textContent = [
      last && `${last.ok ? "Last backup" : "Backup failed"} ${time(last.at)}${!last.ok && last.message ? `: ${last.message}` : ""}`,
      next ? `Next backup ${time(next)}` : `Schedule ${schedule}`,
    ].filter(Boolean).join(" / ");
    for (const button of $("snapshot-list").querySelectorAll("button")) button.disabled = $("backup").disabled;
  }
}

function openPanel(app, tab, id, container) {
  if (tab === "backups") {
    const request = new AbortController();
    loadSnapshots(app, request.signal);
    return () => request.abort();
  }
  if (tab === "logs" || tab === "task") {
    if (tab === "task") return followOutput(`${appApi(app.name)}/task`, tab, refresh);
    const query = new URLSearchParams({ details: "true", ...(id && { container: id }) });
    const url = (id ? container : app.rev) ? `${appApi(app.name)}/logs?${query}` : null;
    return followOutput(url, tab, refresh, {
      source: container?.name,
      unavailable: id ? "This container is unavailable. Choose another container or view all app containers." : "This app has not been deployed.",
    });
  }
}

async function loadSnapshots(app, signal) {
  if (snapshotApp !== app.name) snapshotMessage("Loading snapshots…", "muted");
  snapshotApp = app.name;
  try {
    const response = await fetch(`${appApi(app.name)}/snapshots`, { signal });
    if (response.status === 401) return signIn();
    if (!response.ok) throw Error(await response.text() || response.statusText);
    const snapshots = await response.json();
    if (signal.aborted) return;
    renderSnapshots(snapshots);
    for (const button of $("snapshot-list").querySelectorAll("button")) button.disabled = $("backup").disabled;
  } catch (error) {
    if (!signal.aborted) snapshotMessage(`Could not load snapshots. ${error.message}`, "bad");
  }
}

const signIn = () => location.assign("/");

function feedback(text, tone = "") {
  $("feedback").textContent = text;
  $("feedback").className = `feedback ${tone}`;
  $("feedback").hidden = !text;
}

async function post(path) {
  const response = await fetch(path, { method: "POST", headers: { "x-automata": "1" } });
  if (response.status === 401) {
    signIn();
    return false;
  }
  if (!response.ok) throw Error(await response.text() || response.statusText);
  return response;
}

async function action(path, label, onSuccess) {
  if (actionPending) return;
  actionPending = true;
  feedback("");
  render();
  try {
    if (await post(path)) {
      feedback(label);
      onSuccess?.();
      refresh();
    }
  } catch (error) {
    feedback(error.message, "bad");
  } finally {
    actionPending = false;
    render();
  }
}

async function sync() {
  if ($("sync").disabled) return;
  actionPending = true;
  feedback("");
  render();
  try {
    const response = await post("/api/sync");
    if (response) syncRequest = (await response.json()).request;
  } catch (error) {
    feedback(error.message, "bad");
  } finally {
    actionPending = false;
    render();
    refresh();
  }
}

function appAction(kind, label, query = "") {
  if (!current || $("restart").disabled) return;
  const { name } = current;
  const hash = location.hash;
  action(`${appApi(name)}/${kind}${query}`, label, () => {
    if (current?.name === name && location.hash === hash) goTo(name, "task");
  });
}

$("sync").addEventListener("click", sync);
$("restart").addEventListener("click", () => appAction("restart", "Restart requested."));
$("backup").addEventListener("click", () => appAction("backup", "Backup requested."));
$("log-container").addEventListener("change", (event) => {
  if (current && activeView === "logs") navigate(appLink(current.name, "logs", event.target.value));
});
$("snapshot-list").addEventListener("click", (event) => {
  const button = event.target.closest("[data-restore]");
  if (!button || !current || $("restart").disabled) return;
  const { restore } = button.dataset;
  if (confirm(`Stop ${current.name} and replace its volumes with snapshot ${restore}?`)) {
    appAction("restore", "Restore requested.", `?${new URLSearchParams({ snapshot: restore })}`);
  }
});

$("logout").addEventListener("click", async () => {
  if (!view?.auth.password) return;
  try {
    if (!await post("/logout")) return;
    location.assign(view.auth.access === "on" ? "/cdn-cgi/access/logout" : "/");
  } catch (error) {
    feedback(error.message, "bad");
  }
});

document.addEventListener("click", (event) => {
  const link = event.target.closest('a[href^="#"]');
  if (!link || event.button !== 0 || event.metaKey || event.ctrlKey || event.altKey || event.shiftKey) return;
  event.preventDefault();
  navigate(link.hash, !link.matches(".tabs a"));
});
window.addEventListener("hashchange", render);

document.addEventListener("keydown", (event) => {
  if (!view || event.metaKey || event.ctrlKey || event.altKey || event.shiftKey ||
      event.target.closest("input, textarea, select, [contenteditable]")) return;
  const key = event.key;
  if (key === "Escape" && route().name) navigate("", true);
  if (key === "s") sync();
  if (key === "r") appAction("restart", "Restart requested.");
  if (key === "b" && current?.backup) appAction("backup", "Backup requested.");
  if (key === "l" && current) goTo(current.name, "logs");
  if (key === "t" && current) goTo(current.name, "task");
  if (key === "j" || key === "k") {
    const apps = orderedApps();
    const index = apps.findIndex((app) => app.name === current?.name);
    const next = apps[Math.max(0, Math.min(apps.length - 1, index + (key === "j" ? 1 : -1)))];
    if (next) navigate(appLink(next.name), true);
  }
});

async function refresh() {
  if (refreshing) {
    refreshAgain = true;
    return;
  }
  refreshing = true;
  do {
    refreshAgain = false;
    try {
      const response = await fetch("/api/state", { cache: "no-store" });
      if (response.status === 401) return signIn();
      if (!response.ok) throw Error(response.statusText);
      view = await response.json();
      if (stale) {
        stale = false;
        if ($("feedback").textContent === "Could not refresh dashboard data. Retrying…") feedback("");
      }
      render();
    } catch {
      if (!view) $("empty").textContent = "Could not load apps. Retrying…";
      else {
        stale = true;
        feedback("Could not refresh dashboard data. Retrying…", "bad");
      }
    }
  } while (refreshAgain);
  refreshing = false;
}

const events = new EventSource("/api/events");
events.addEventListener("change", refresh);
events.addEventListener("error", refresh);
refresh();
setInterval(refresh, 30000);
