export const $ = (id) => document.getElementById(id);
const escape = (value) => String(value ?? "").replace(/[&<>"]/g, (char) => (
  { "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" }[char]
));

export const icon = (name) => `<svg class="icon" aria-hidden="true"><use href="#i-${name}"/></svg>`;
const time = (ms) => new Date(ms).toLocaleString();
export const appLink = (name, view = "overview", container) => `#${new URLSearchParams({
  app: name, ...(view !== "overview" && { view }), ...(container && { container }),
})}`;
const age = (at) => {
  const minutes = Math.max(0, Math.floor((Date.now() - at) / 60_000));
  if (!minutes) return "Just now";
  if (minutes < 60) return `${minutes}m ago`;
  if (minutes < 1440) return `${Math.floor(minutes / 60)}h ago`;
  return `${Math.floor(minutes / 1440)}d ago`;
};
const when = (at) => `<time datetime="${new Date(at).toISOString()}" title="${escape(time(at))}">${age(at)}</time>`;

const github = (repo, sha, label = sha ? sha.slice(0, 7) : repo) => {
  if (!/^[\w.-]+\/[\w.-]+$/.test(repo) || (sha && !/^[a-f0-9]+$/i.test(sha))) return `<code>${escape(label)}</code>`;
  const url = `https://github.com/${repo}${sha ? `/commit/${sha}` : ""}`;
  return `<a href="${url}" target="_blank" rel="noopener"><code>${escape(label)}</code></a>`;
};

const failing = (container) => container.health === "unhealthy" || container.state === "restarting" ||
  (container.state === "exited" && !container.status.startsWith("Exited (0)"));

export function status(app, runtimeError) {
  if (runtimeError) return { tone: "muted", label: "Runtime unknown" };
  const problem = app.containers.find(failing);
  const running = app.containers.filter((container) => container.state === "running").length;
  const total = app.containers.length;
  if (problem) return { tone: "bad", label: `${problem.service} ${problem.health === "unhealthy" ? "unhealthy" : problem.state}` };
  if (app.containers.some((container) => container.health === "starting")) return { tone: "busy", label: "Starting" };
  if (running === total && total > 0) return { tone: "good", label: "Running" };
  if (running) return { tone: "busy", label: `${running} of ${total} running` };
  return app.rev ? { tone: "bad", label: "Stopped" } : { tone: "muted", label: "Not deployed" };
}

export const statusHtml = ({ tone, label }) =>
  `<span class="status ${tone}">${icon({ good: "check", bad: "warning", busy: "clock", muted: "minus" }[tone])}${escape(label)}</span>`;

const markup = new WeakMap();
function updateHtml(node, html) {
  if (markup.get(node) === html) return;
  const active = document.activeElement;
  const key = active && node.contains(active) && (active.getAttribute("href") || active.dataset.restore);
  node.innerHTML = html;
  markup.set(node, html);
  if (key) {
    const target = [...node.querySelectorAll("a, button")].find((item) => item.getAttribute("href") === key || item.dataset.restore === key);
    target?.focus({ preventScroll: true });
  }
}

export function renderHeader(view) {
  const { infra, auth } = view;
  const repo = infra.repo.split("#")[0];
  const applied = infra.applied?.commit;
  const fetched = infra.commit && infra.commit !== applied;
  updateHtml($("infra"), `${github(repo)}${applied ? `<span class="commit-ref" title="Last applied commit">${github(repo, applied, "@" + applied.slice(0, 7))}</span>` : ""}`);
  const syncPending = infra.requested > infra.completed;
  const sync = infra.syncing ? { tone: "busy", label: "Syncing" } :
    syncPending ? { tone: "busy", label: "Sync queued" } :
    infra.result === false ? { tone: "bad", label: "Sync incomplete" } :
    infra.result === true ? null : { tone: "muted", label: "Waiting for first sync" };
  updateHtml($("sync-state"), sync
    ? statusHtml(sync) + (fetched ? `<span class="muted">Fetched ${github(repo, infra.commit)}</span>` : "")
    : "");
  const synced = $("synced");
  synced.textContent = infra.synced_at ? `${infra.result ? "Synced" : "Checked"} ${age(infra.synced_at).toLowerCase()}` : "";
  synced.title = infra.synced_at ? time(infra.synced_at) : "";
  if (infra.synced_at) synced.dateTime = new Date(infra.synced_at).toISOString();
  else synced.removeAttribute("datetime");
  $("logout").hidden = !auth.password;
  $("version").textContent = `Automata ${view.version}${view.latest ? ` / ${view.latest} available` : ""}`;
  const alerts = [
    infra.error && `Infra: ${infra.error}`,
    view.runtime_error && `Runtime status unavailable: ${view.runtime_error}`,
    view.tunnel_error && `Tunnel: ${view.tunnel_error}`,
    view.self_error && `Self-update: ${view.self_error}`,
    auth.access === "failed" && `Cloudflare Access: ${auth.access_error}`,
    auth.access === "off" && !auth.password && "The dashboard is open to anyone who can reach it. Set DASHBOARD_PASSWORD or configure Cloudflare Access.",
    ...view.invalid.map((item) => `apps/${item.name}: ${item.error}`),
  ].filter(Boolean);
  $("alerts").hidden = !alerts.length;
  updateHtml($("alerts"), alerts.map((message) => `<li>${escape(message)}</li>`).join(""));
}

const routeLink = (url) => url.startsWith("*.") ? escape(url) :
  `<a class="route-link" href="https://${escape(url)}" target="_blank" rel="noopener"><span>${escape(url).split("/").join("/<wbr>")}</span>${icon("external")}</a>`;
const routesList = (urls) => `<ul class="routes">${urls.map((url) => `<li>${routeLink(url)}</li>`).join("")}</ul>`;

function deployCell(app, repo) {
  const link = appLink(app.name, "task");
  const last = app.commit || app.infra_commit;
  const deployed = last ? github(app.commit ? app.source?.split(/[#@]/)[0] || "" : repo, last) : "";
  const at = app.deployed_at ? when(app.deployed_at) : "";
  const successful = [deployed, at].filter(Boolean).join(" ");
  if (app.busy?.startsWith("deploy")) {
    return statusHtml({ tone: "busy", label: app.busy.endsWith("queued") ? "Deploy queued" : "Deploying" }) +
      (successful ? `<span class="cell-meta">Last success ${successful}</span>` : "");
  }
  if (app.failed || app.error) {
    const label = app.failed ? "Deploy failed" : "Needs attention";
    return `<a class="status bad" href="${app.failed ? link : appLink(app.name)}">${icon("warning")}${label}</a>` +
      (successful ? `<span class="cell-meta">Last success ${successful}</span>` : "");
  }
  if (!app.rev) return '<span class="muted">No deploy yet</span>';
  return `<span class="deploy-ok">${at ? `<span><span class="deploy-label">Deployed </span>${at}</span>` : ""}${deployed || (at ? "" : "Deployed")}</span>`;
}

export function renderApps(apps, view) {
  const list = $("apps");
  const existing = new Map([...list.children].map((row) => [row.dataset.name, row]));
  const focus = document.activeElement;
  const repo = view.infra.repo.split("#")[0];
  let attention = 0;
  apps.forEach((app, index) => {
    let row = existing.get(app.name);
    if (!row) {
      row = document.createElement("tr");
      row.dataset.name = app.name;
      row.setAttribute("role", "row");
      row.innerHTML = '<th scope="row" role="rowheader"></th><td role="cell" class="runtime-cell"></td><td role="cell" class="deploy-cell"></td><td role="cell" class="route-cell"></td>';
      updateHtml(row.cells[0], `<a class="app-link" href="${appLink(app.name)}"><span class="app-glyph">${icon("box")}</span><span>${escape(app.name)}</span>${icon("right")}</a>`);
    }
    existing.delete(app.name);
    const runtime = status(app, view.runtime_error);
    if (runtime.tone === "bad" || app.failed || app.error) attention += 1;
    updateHtml(row.cells[1], statusHtml(runtime));
    updateHtml(row.cells[2], deployCell(app, repo));
    updateHtml(row.cells[3], app.urls.length ? routesList(app.urls) : '<span class="muted">No routes</span>');
    if (list.children[index] !== row) list.insertBefore(row, list.children[index] || null);
  });
  for (const row of existing.values()) row.remove();
  if (list.contains(focus) && document.activeElement !== focus) focus.focus({ preventScroll: true });
  $("app-summary").textContent = attention ? `${attention} of ${apps.length} need attention` : apps.length ? `${apps.length} managed` : "";
  $("app-table").hidden = !apps.length;
  $("empty").hidden = !!apps.length;
  $("empty").textContent = "No apps yet. Add an app under apps/ in the infra repository.";
}

export function renderLogContainers(app, selected) {
  const containers = [...app.containers].sort((a, b) => a.name.localeCompare(b.name));
  const options = containers.map(({ id, name, state }) => `<option value="${escape(id)}">${escape(`${name} (${state})`)}</option>`);
  if (selected && !containers.some(({ id }) => id === selected)) {
    options.push(`<option value="${escape(selected)}" disabled>Container unavailable</option>`);
  }
  const select = $("log-container");
  updateHtml(select, '<option value="">All app containers</option>' + options.join(""));
  select.value = selected || "";
}

function containerRow(app, container) {
  const { id, name, service, health, state, image, status } = container;
  const tone = failing(container) ? "bad" : health === "starting" ? "busy" : state === "running" ? "good" : "muted";
  const label = health || state;
  return `<li class="container-row"><div class="container-name"><strong>${escape(service)}</strong>
    ${name === service ? "" : `<span class="cell-meta">${escape(name)}</span>`}</div>
    <div class="container-state">${statusHtml({ tone, label: label.charAt(0).toUpperCase() + label.slice(1) })}
      <span class="cell-meta">${escape(status.replace(/\s*\((healthy|unhealthy|health: starting)\)/, ""))}</span></div>
    <code class="container-image">${escape(image)}</code>
    <a class="logs-link" href="${appLink(app.name, "logs", id)}" aria-label="Logs for ${escape(name)}">${icon("terminal")}Logs</a></li>`;
}

export function renderOverview(app, view) {
  const sourceRepo = app.source?.split(/[#@]/)[0];
  const release = [
    sourceRepo && app.commit && `<span>Source ${github(sourceRepo, app.commit)}</span>`,
    app.infra_commit && `<span>Infra ${github(view.infra.repo.split("#")[0], app.infra_commit)}</span>`,
    app.rev && !app.infra_commit && !app.commit && `<span>Revision <code>${escape(app.rev)}</code></span>`,
    app.deployed_at && `<span class="release-time">${calendarTime(app.deployed_at, new Date())}</span>`,
  ].filter(Boolean);
  const source = app.image ? `<code>${escape(app.image)}</code>` : app.source ? github(sourceRepo) : "Infra repository";
  const imageCheck = app.images && `<dt>Image check</dt><dd class="${app.images.ok ? "" : "bad"}">${escape(app.images.message || (app.images.ok ? "Completed" : "Failed"))}</dd>`;
  updateHtml($("overview"), `<section aria-labelledby="deployment-title"><h2 id="deployment-title">Deployment</h2><dl class="facts">
    <dt>Routes</dt><dd>${app.urls.length ? routesList(app.urls) : '<span class="muted">No routes configured</span>'}</dd>
    <dt>${app.image ? "Configured image" : "Source"}</dt><dd>${source}</dd>
    <dt>Release</dt><dd class="release">${release.join("") || '<span class="muted">Not deployed</span>'}</dd>
    ${imageCheck || ""}
  </dl></section><section aria-labelledby="containers-title"><h2 id="containers-title">Containers</h2>
    ${app.containers.length ? `<ul class="container-list" role="list">${app.containers.map((container) => containerRow(app, container)).join("")}</ul>` :
    `<p class="muted">${view.runtime_error ? "Container status is unavailable." : "No containers."}</p>`}</section>`);
}

const dayFormat = new Intl.DateTimeFormat(undefined, { month: "short", day: "numeric" });
const dateFormat = new Intl.DateTimeFormat(undefined, { year: "numeric", month: "short", day: "numeric" });
const clockFormat = new Intl.DateTimeFormat(undefined, { hour: "numeric", minute: "2-digit" });
const exactFormat = new Intl.DateTimeFormat(undefined, {
  year: "numeric", month: "short", day: "numeric", hour: "numeric", minute: "2-digit", second: "2-digit", timeZoneName: "short",
});

function relativeDay(date, now) {
  const localDay = (d) => Date.UTC(d.getFullYear(), d.getMonth(), d.getDate());
  const days = (localDay(date) - localDay(now)) / 86_400_000;
  return ({ "-1": "Yesterday", 0: "Today", 1: "Tomorrow" })[days] ||
    (date.getFullYear() === now.getFullYear() ? dayFormat : dateFormat).format(date);
}

function calendarTime(at, now) {
  const date = new Date(at);
  const day = now ? relativeDay(date, now) : dateFormat.format(date);
  return `<time datetime="${date.toISOString()}" title="${escape(exactFormat.format(date))}">${escape(day)} <span class="clock">at ${escape(clockFormat.format(date))}</span></time>`;
}

export function renderBackupSummary({ last, next, schedule }) {
  const now = new Date();
  const failed = last?.ok === false;
  const previous = failed ? "Last backup failed" : "Last backup";
  updateHtml($("backup-summary"), `<div>
    <dt class="${failed ? "bad" : ""}">${icon(failed ? "warning" : last ? "check" : "archive")}${previous}</dt>
    <dd>${last ? calendarTime(last.at, now) : "No backups yet"}</dd>
    ${failed && last.message ? `<dd class="backup-detail">${escape(last.message)}</dd>` : ""}
  </div><div>
    <dt>${icon("clock")}Next backup</dt>
    <dd>${next != null ? calendarTime(next, now) : "No upcoming run"}</dd>
    ${next == null && schedule ? `<dd class="backup-detail">Schedule <code>${escape(schedule)}</code></dd>` : ""}
  </div>`);
}

export function renderSnapshots(list) {
  const size = (bytes) => bytes == null ? "—" : bytes >= 1e9 ? `${(bytes / 1e9).toFixed(1)} GB` : `${(bytes / 1e6).toFixed(1)} MB`;
  const rows = list.map((item) => `<tr role="row">
    <td class="snapshot-created" role="cell">${calendarTime(item.time)}</td>
    <td class="snapshot-tier" role="cell">${escape(item.id.split("/")[0])}</td>
    <td class="snapshot-size" role="cell">${size(item.size)}</td>
    <td class="snapshot-action" role="cell"><button type="button" data-restore="${escape(item.id)}">Restore</button></td></tr>`);
  updateHtml($("snapshot-list"), rows.length ? `<table class="snapshot-table" role="table" aria-label="Backup snapshots">
    <thead role="rowgroup"><tr role="row"><th scope="col">Created</th><th scope="col">Retention</th><th scope="col">Size</th><th class="snapshot-action" scope="col">Action</th></tr></thead>
    <tbody role="rowgroup">${rows.join("")}</tbody></table>` :
    '<p class="muted">No snapshots yet.</p>');
}

export function snapshotMessage(message, tone) {
  updateHtml($("snapshot-list"), `<p class="${tone}">${escape(message)}</p>`);
}
