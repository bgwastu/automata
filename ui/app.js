import { $, appLink, renderApps, renderHeader, renderOverview, renderSnapshots, snapshotMessage, status, time } from "/api/dashboard/view.js";

const views = ["overview", "logs", "task", "backups"];
const tabs = [...document.querySelectorAll("[data-view]")];
const layout = document.querySelector(".layout");
const mobile = matchMedia("(max-width: 680px)");
const appApi = (name) => `/api/apps/${encodeURIComponent(name)}`;
let view, current, activeView, defaultApp, panelKey, cleanupPanel;
let refreshing = false;
let refreshAgain = false;
let actionPending = false;
let snapshotApp;

function route() {
  const hash = new URLSearchParams(location.hash.slice(1));
  return { name: hash.get("app"), tab: hash.get("view") };
}

function navigate(hash, focus = false) {
  if (location.hash !== hash) history.pushState(null, "", hash || location.pathname + location.search);
  render();
  if (focus) {
    const target = route().name ? (current ? $("app-name") : $("unavailable")) : $("apps-title");
    target.focus();
  }
}

function goTo(name, tab) {
  navigate(appLink(name, tab));
}

function orderedApps() {
  const rank = { bad: 0, busy: 1, good: 2, muted: 3 };
  return [...view.apps].sort((a, b) => rank[status(a).tone] - rank[status(b).tone] || a.name.localeCompare(b.name));
}

function render() {
  const { name, tab } = route();
  if (!view) return;

  const apps = orderedApps();
  if (!apps.some((app) => app.name === defaultApp)) defaultApp = apps[0]?.name;
  current = apps.find((app) => app.name === (name || (!mobile.matches && defaultApp)));
  activeView = views.includes(tab) && (tab !== "backups" || current?.backup) ? tab : "overview";
  layout.toggleAttribute("data-app-route", !!name && !!current);

  renderHeader(view);
  renderApps(apps, current?.name);
  $("sync").disabled = actionPending;
  $("workspace").hidden = !current;
  $("unavailable").hidden = !name || !!current;
  if (name && !current) $("unavailable").textContent = `App “${name}” is unavailable.`;

  if (current) renderWorkspace(current);
  const key = current && JSON.stringify([
    current.name, activeView,
    activeView === "logs" ? current.rev : activeView === "backups" ? current.backup.last : null,
  ]);
  if (key !== panelKey) {
    cleanupPanel?.();
    panelKey = key;
    cleanupPanel = current && openPanel(current, activeView);
  }
}

function renderWorkspace(app) {
  const state = status(app);
  $("app-name").textContent = app.name;
  $("app-status").textContent = state.label;
  $("app-status").className = `status ${state.tone}`;
  $("app-error").hidden = !app.error;
  $("app-error").textContent = app.error || "";
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
  $("snapshots").hidden = activeView !== "backups";
  if (activeView === "overview") renderOverview(app, view.infra);
  if (activeView === "backups") {
    const { last, next, schedule } = app.backup;
    $("backup-summary").textContent = [
      last && `${last.ok ? "Last backup" : "Backup failed"} ${time(last.at)}${!last.ok && last.message ? `: ${last.message}` : ""}`,
      next ? `Next backup ${time(next)}` : `Schedule ${schedule}`,
    ].filter(Boolean).join(" / ");
    for (const button of $("snapshot-list").querySelectorAll("button")) button.disabled = $("backup").disabled;
  }
}

function openPanel(app, tab) {
  if (tab === "backups") {
    const request = new AbortController();
    loadSnapshots(app, request.signal);
    return () => request.abort();
  }
  if (tab === "logs" || tab === "task") return followOutput(app, tab);
}

function followOutput(app, tab) {
  const log = $("log");
  const message = $("stream-status");
  log.hidden = true;
  log.replaceChildren();
  log.scrollTop = 0;
  if (tab === "logs" && !app.rev) {
    message.textContent = "This app has not been deployed.";
    return;
  }

  message.textContent = "Connecting…";
  const stream = new EventSource(`${appApi(app.name)}/${tab}`);
  let pending = [];
  let frame = 0;
  let lines = 0;
  let emptyTimer;
  const showStatus = (text) => {
    if (message.textContent !== text) message.textContent = text;
  };
  const empty = () => showStatus(tab === "logs" ? "No container output yet. New lines will appear here." : "No task output yet.");
  const flush = () => {
    frame = 0;
    if (!pending.length) return;
    log.hidden = false;
    const atEnd = log.scrollTop + log.clientHeight >= log.scrollHeight - 4;
    if (pending.length > 4000) pending = pending.slice(-4000);
    const text = document.createTextNode(pending.join("\n") + "\n");
    log.append(text);
    lines += pending.length;
    text.lines = pending.length;
    pending = [];
    const beforeTrim = log.scrollHeight;
    while (lines > 4000 && log.firstChild !== log.lastChild) {
      lines -= log.firstChild.lines;
      log.firstChild.remove();
    }
    if (atEnd) log.scrollTop = log.scrollHeight;
    else if (beforeTrim > log.scrollHeight) log.scrollTop -= beforeTrim - log.scrollHeight;
    showStatus("Live");
  };

  stream.onopen = () => {
    if (lines) showStatus("Live");
    else emptyTimer = setTimeout(empty, 600);
  };
  stream.onmessage = (event) => {
    clearTimeout(emptyTimer);
    pending.push(event.data);
    if (!frame) frame = requestAnimationFrame(flush);
  };
  stream.addEventListener("clear", () => {
    pending = [];
    lines = 0;
    log.hidden = true;
    log.replaceChildren();
    empty();
  });
  stream.onerror = () => {
    clearTimeout(emptyTimer);
    showStatus("Connection lost. Reconnecting…");
    refresh();
  };
  return () => {
    stream.close();
    clearTimeout(emptyTimer);
    cancelAnimationFrame(frame);
  };
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
  return true;
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

function sync() {
  if (!$("sync").disabled) action("/api/sync", "Sync requested.");
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
$("snapshot-list").addEventListener("click", (event) => {
  const button = event.target.closest("[data-restore]");
  if (!button || !current || $("restart").disabled) return;
  const { restore } = button.dataset;
  if (confirm(`Stop ${current.name} and replace its volumes with snapshot ${restore}?`)) {
    appAction("restore", "Restore requested.", `?${new URLSearchParams({ snapshot: restore })}`);
  }
});

$("logout").addEventListener("click", async () => {
  try {
    if (view?.auth.password && !await post("/logout")) return;
    location.assign(view?.auth.access === "on" ? "/cdn-cgi/access/logout" : "/");
  } catch (error) {
    feedback(error.message, "bad");
  }
});

document.addEventListener("click", (event) => {
  const link = event.target.closest('.app-list a, .tabs a, .back, .brand');
  if (!link || event.button !== 0 || event.metaKey || event.ctrlKey || event.altKey || event.shiftKey) return;
  event.preventDefault();
  navigate(link.hash, !link.matches(".tabs a"));
});
window.addEventListener("hashchange", render);
mobile.addEventListener("change", render);

document.addEventListener("keydown", (event) => {
  if (!view || event.metaKey || event.ctrlKey || event.altKey || event.shiftKey ||
      event.target.closest("input, textarea, select, [contenteditable]")) return;
  const key = event.key;
  if (key === "Escape" && route().name && !$("account").open) navigate("", true);
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
      render();
    } catch {
      if (!view) $("empty").textContent = "Could not load apps. Retrying…";
    }
  } while (refreshAgain);
  refreshing = false;
}

const events = new EventSource("/api/events");
events.addEventListener("change", refresh);
events.addEventListener("error", refresh);
refresh();
setInterval(refresh, 30000);
