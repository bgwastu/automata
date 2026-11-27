export const $ = (id) => document.getElementById(id);
const escape = (value) => String(value ?? "").replace(/[&<>"]/g, (char) => (
  { "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" }[char]
));

export const time = (ms) => new Date(ms).toLocaleString();
export const appLink = (name, view = "overview") => `#${new URLSearchParams({ app: name, ...(view !== "overview" && { view }) })}`;

const github = (repo, sha) => {
  const label = sha ? sha.slice(0, 7) : repo;
  if (!/^[\w.-]+\/[\w.-]+$/.test(repo) || (sha && !/^[a-f0-9]+$/i.test(sha))) return `<code>${escape(label)}</code>`;
  const url = `https://github.com/${repo}${sha ? `/commit/${sha}` : ""}`;
  return `<a href="${url}" target="_blank" rel="noopener"><code>${escape(label)}</code></a>`;
};

const failing = (container) => container.health === "unhealthy" || container.state === "restarting" ||
  (container.state === "exited" && !container.status.startsWith("Exited (0)"));

export function status(app) {
  const problem = app.containers.find(failing);
  const running = app.containers.filter((container) => container.state === "running").length;
  if (app.failed) return { tone: "bad", label: app.rev ? "Last deploy failed" : "Deploy failed", running };
  if (problem) return { tone: "bad", label: `${problem.service} ${problem.health === "unhealthy" ? "unhealthy" : problem.state}`, running };
  if (app.busy) return { tone: "busy", label: app.busy, running };
  if (app.error) return { tone: "bad", label: "Needs attention", running };
  if (!app.rev) return { tone: "muted", label: "Not deployed", running };
  if (app.containers.some((container) => container.health === "starting")) return { tone: "busy", label: "Starting", running };
  if (!running) return { tone: "bad", label: "Stopped", running };
  if (running < app.containers.length) return { tone: "busy", label: "Partially running", running };
  return { tone: "good", label: "Running", running };
}

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
  updateHtml($("infra"), `${github(repo)}${infra.commit ? github(repo, infra.commit) : ""}`);
  $("synced").textContent = infra.synced_at ? `Synced ${time(infra.synced_at)}` : "";
  $("tunnel").textContent = view.tunnel_error ? "Tunnel error" : view.domain ? "Tunnel up" : "";
  $("tunnel").className = view.tunnel_error ? "bad" : "good";
  $("system-task").textContent = view.busy || "";
  $("user").textContent = auth.user || "";
  $("account").hidden = !(auth.password || auth.access === "on");
  $("version").textContent = `v${view.version}${view.latest ? ` / ${view.latest} available` : ""}`;
  const alerts = [
    infra.error && `Infra: ${infra.error}`,
    view.tunnel_error && `Tunnel: ${view.tunnel_error}`,
    view.self_error && `Self-update: ${view.self_error}`,
    auth.access === "failed" && `Cloudflare Access: ${auth.access_error}`,
    auth.access === "off" && !auth.password && "The dashboard is open to anyone who can reach it. Set DASHBOARD_PASSWORD or configure Cloudflare Access.",
    ...view.invalid.map((item) => `apps/${item.name}: ${item.error}`),
  ].filter(Boolean);
  $("alerts").hidden = !alerts.length;
  updateHtml($("alerts"), alerts.map((message) => `<li>${escape(message)}</li>`).join(""));
}

export function renderApps(apps, selected) {
  const list = $("apps");
  const existing = new Map([...list.children].map((row) => [row.dataset.name, row]));
  const focus = document.activeElement;
  apps.forEach((app, index) => {
    let row = existing.get(app.name);
    if (!row) {
      row = document.createElement("li");
      row.dataset.name = app.name;
      row.innerHTML = '<a><strong></strong><span class="meta"><span></span><span class="muted"></span></span></a>';
      row.firstElementChild.href = appLink(app.name);
    }
    existing.delete(app.name);
    const link = row.firstElementChild;
    const { tone, label, running } = status(app);
    link.firstElementChild.textContent = app.name;
    const [state, counts] = link.lastElementChild.children;
    state.className = tone;
    state.textContent = label;
    counts.textContent = app.containers.length ? `${running}/${app.containers.length} containers` : "";
    if (app.name === selected) link.setAttribute("aria-current", "true");
    else link.removeAttribute("aria-current");
    if (list.children[index] !== row) list.insertBefore(row, list.children[index] || null);
  });
  for (const row of existing.values()) row.remove();
  if (list.contains(focus) && document.activeElement !== focus) focus.focus({ preventScroll: true });
  $("app-count").textContent = apps.length ? String(apps.length) : "";
  $("empty").hidden = apps.length > 0;
  $("empty").textContent = "No apps yet. Add an app under apps/ in the infra repository.";
}

function containerRow(container) {
  const { service, health, state, image, status } = container;
  const cls = failing(container) ? "bad" : health === "starting" ? "busy" : state === "running" ? "good" : "muted";
  return `<tr><td>${escape(service)}</td><td class="${cls}">${escape(health || state)}</td>
    <td><code>${escape(image)}</code></td><td>${escape(status.replace(/\s*\((healthy|unhealthy|health: starting)\)/, ""))}</td></tr>`;
}

export function renderOverview(app, infra) {
  const urls = app.urls.map((url) => {
    // Wildcard routes have no canonical host to visit.
    if (url.startsWith("*.")) return `<li>${escape(url)}</li>`;
    return `<li><a href="https://${escape(url)}" target="_blank" rel="noopener">${escape(url)}</a></li>`;
  });
  const sourceRepo = app.source?.split(/[#@]/)[0];
  const release = [
    sourceRepo && app.commit && `<span>Source ${github(sourceRepo, app.commit)}</span>`,
    app.infra_commit && `<span>Infra ${github(infra.repo.split("#")[0], app.infra_commit)}</span>`,
    app.rev && !app.infra_commit && !app.commit && `<span>Revision <code>${escape(app.rev)}</code></span>`,
    app.deployed_at && `<time>${escape(time(app.deployed_at))}</time>`,
  ].filter(Boolean);
  const source = app.image ? `<code>${escape(app.image)}</code>` : app.source ? github(sourceRepo) : "Infra repository";
  const imageCheck = app.images && `<dt>Image check</dt><dd class="${app.images.ok ? "" : "bad"}">${escape(app.images.message || (app.images.ok ? "Completed" : "Failed"))}</dd>`;
  updateHtml($("overview"), `<h3>Deployment</h3><dl class="facts">
    <dt>Routes</dt><dd>${urls.length ? `<ul class="routes">${urls.join("")}</ul>` : '<span class="muted">No public routes</span>'}</dd>
    <dt>${app.image ? "Image" : "Source"}</dt><dd>${source}</dd>
    <dt>Release</dt><dd class="release">${release.join("") || '<span class="muted">Not deployed</span>'}</dd>
    ${imageCheck || ""}
  </dl><h3>Containers</h3>${app.containers.length ? `<div class="table-scroll">
    <table><thead><tr><th>Service</th><th>Health</th><th>Image</th><th>Status</th></tr></thead>
    <tbody>${app.containers.map(containerRow).join("")}</tbody></table></div>` : '<p class="muted">No containers.</p>'}`);
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
