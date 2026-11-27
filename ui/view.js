export const $ = (id) => document.getElementById(id);
const escape = (value) => String(value ?? "").replace(/[&<>"]/g, (char) => (
  { "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" }[char]
));

export const icon = (name) => `<svg class="icon" aria-hidden="true"><use href="#i-${name}"/></svg>`;
export const time = (ms) => new Date(ms).toLocaleString();
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

const github = (repo, sha) => {
  const label = sha ? sha.slice(0, 7) : repo;
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
  if (running) return { tone: running === total ? "good" : "busy", label: `${running}/${total} running` };
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
  updateHtml($("infra"), `${github(repo)}${fetched ? ` <span class="cell-meta fetched">Fetched ${github(repo, infra.commit)}</span>` : ""}`);
  const syncPending = infra.requested > infra.completed;
  const sync = infra.syncing ? { tone: "busy", label: "Syncing" } :
    syncPending ? { tone: "busy", label: "Sync queued" } :
    infra.result === false ? { tone: "bad", label: "Sync incomplete" } :
    infra.result === true ? { tone: "good", label: "In sync" } : { tone: "muted", label: "Waiting for first sync" };
  updateHtml($("sync-state"), `${statusHtml(sync)}${applied ? ` <span class="applied">Applied ${github(repo, applied)}</span>` : ""}`);
  const synced = $("synced");
  synced.textContent = infra.synced_at ? `Checked ${age(infra.synced_at).toLowerCase()}` : "";
  synced.title = infra.synced_at ? time(infra.synced_at) : "";
  updateHtml($("tunnel"), view.tunnel_error ? statusHtml({ tone: "bad", label: "Tunnel error" }) :
    view.domain ? statusHtml({ tone: "good", label: "Tunnel configured" }) : "");
  $("user").textContent = auth.user || "";
  $("account").hidden = !(auth.password || auth.access === "on");
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
  `<a class="route-link" href="https://${escape(url)}" target="_blank" rel="noopener"><span>${escape(url)}</span>${icon("external")}</a>`;
const routesList = (urls) => `<ul class="routes">${urls.map((url) => `<li>${routeLink(url)}</li>`).join("")}</ul>`;
const routesCell = ([first, ...rest]) => first
  ? routeLink(first) + (rest.length ? `<details class="route-details"><summary>${rest.length} more ${rest.length === 1 ? "route" : "routes"}</summary>${routesList(rest)}</details>` : "")
  : '<span class="muted">No routes</span>';

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
  return `<span class="deploy-ok">${icon("check")}<span>${deployed || at || "Deployed"}${deployed && at ? `<span class="cell-meta">${at}</span>` : ""}</span></span>`;
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
      row.innerHTML = '<th scope="row" role="rowheader"></th><td role="cell"></td><td role="cell"></td><td role="cell" class="route-cell"></td>';
      updateHtml(row.cells[0], `<a class="app-link" href="${appLink(app.name)}">${icon("box")}<span>${escape(app.name)}</span></a>`);
    }
    existing.delete(app.name);
    const runtime = status(app, view.runtime_error);
    if (runtime.tone === "bad" || app.failed || app.error) attention += 1;
    updateHtml(row.cells[1], statusHtml(runtime));
    updateHtml(row.cells[2], deployCell(app, repo));
    updateHtml(row.cells[3], routesCell(app.urls));
    if (list.children[index] !== row) list.insertBefore(row, list.children[index] || null);
  });
  for (const row of existing.values()) row.remove();
  if (list.contains(focus) && document.activeElement !== focus) focus.focus({ preventScroll: true });
  $("app-summary").textContent = apps.length ? `${apps.length} configured${attention ? `, ${attention} need${attention === 1 ? "s" : ""} attention` : ""}` : "";
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
  return `<tr role="row"><th role="rowheader" scope="row">${escape(service)}
    ${name === service ? "" : `<span class="cell-meta">${escape(name)}</span>`}</th><td role="cell">${statusHtml({ tone, label: health || state })}</td>
    <td role="cell" class="image-cell"><code>${escape(image)}</code></td><td role="cell" class="uptime-cell">${escape(status.replace(/\s*\((healthy|unhealthy|health: starting)\)/, ""))}</td>
    <td role="cell" class="logs-cell"><a class="logs-link" href="${appLink(app.name, "logs", id)}" aria-label="Logs for ${escape(name)}">${icon("terminal")}<span>Logs</span></a></td></tr>`;
}

export function renderOverview(app, view) {
  const sourceRepo = app.source?.split(/[#@]/)[0];
  const release = [
    sourceRepo && app.commit && `<span>Source ${github(sourceRepo, app.commit)}</span>`,
    app.infra_commit && `<span>Infra ${github(view.infra.repo.split("#")[0], app.infra_commit)}</span>`,
    app.rev && !app.infra_commit && !app.commit && `<span>Revision <code>${escape(app.rev)}</code></span>`,
    app.deployed_at && `<time>${escape(time(app.deployed_at))}</time>`,
  ].filter(Boolean);
  const source = app.image ? `<code>${escape(app.image)}</code>` : app.source ? github(sourceRepo) : "Infra repository";
  const imageCheck = app.images && `<dt>Image check</dt><dd class="${app.images.ok ? "" : "bad"}">${escape(app.images.message || (app.images.ok ? "Completed" : "Failed"))}</dd>`;
  updateHtml($("overview"), `<h2>Deployment</h2><dl class="facts">
    <dt>Routes</dt><dd>${app.urls.length ? routesList(app.urls) : '<span class="muted">No routes configured</span>'}</dd>
    <dt>${app.image ? "Configured image" : "Source"}</dt><dd>${source}</dd>
    <dt>Deployed release</dt><dd class="release">${release.join("") || '<span class="muted">Not deployed</span>'}</dd>
    ${imageCheck || ""}
  </dl><h2>Containers</h2>${app.containers.length ? `<div class="table-scroll">
    <table class="container-table" role="table"><thead role="rowgroup"><tr role="row"><th scope="col">Service</th><th scope="col">Health</th><th scope="col">Image</th><th scope="col">Status</th><th scope="col" class="logs-cell">Output</th></tr></thead>
    <tbody role="rowgroup">${app.containers.map((container) => containerRow(app, container)).join("")}</tbody></table></div>` :
    `<p class="muted">${view.runtime_error ? "Container status is unavailable." : "No containers."}</p>`}`);
}

export function renderSnapshots(list) {
  const size = (bytes) => bytes == null ? "—" : bytes >= 1e9 ? `${(bytes / 1e9).toFixed(1)} GB` : `${(bytes / 1e6).toFixed(1)} MB`;
  const rows = list.map((item) => `<tr><td>${escape(item.id.split("/")[0])}</td><td>${escape(time(item.time))}</td>
    <td>${size(item.size)}</td><td><button type="button" data-restore="${escape(item.id)}">Restore</button></td></tr>`);
  updateHtml($("snapshot-list"), rows.length ? `<div class="table-scroll"><table><thead>
    <tr><th>Retention</th><th>Created</th><th>Size</th><th>Action</th></tr></thead><tbody>${rows.join("")}</tbody></table></div>` :
    '<p class="muted">No snapshots yet.</p>');
}

export function snapshotMessage(message, tone) {
  updateHtml($("snapshot-list"), `<p class="${tone}">${escape(message)}</p>`);
}
