const $ = (id) => document.getElementById(id);
const esc = (s) => String(s ?? "").replace(/[&<>"]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" })[c]);

let view = null;
let selected = localStorage.getItem("selected");
let tab = localStorage.getItem("tab") || "task";
let stream = null;

function ago(ms) {
  if (!ms) return "";
  const s = Math.round((Date.now() - ms) / 1000);
  if (s < 0) return in_(-s);
  if (s < 60) return "just now";
  if (s < 3600) return `${Math.floor(s / 60)}m ago`;
  if (s < 86400) return `${Math.floor(s / 3600)}h ago`;
  return `${Math.floor(s / 86400)}d ago`;
}
function in_(s) {
  if (s < 3600) return `in ${Math.max(1, Math.floor(s / 60))}m`;
  if (s < 86400) return `in ${Math.floor(s / 3600)}h`;
  return `in ${Math.floor(s / 86400)}d`;
}

// The session ran out, the password changed, or the Cloudflare Access token expired. Loading `/`
// shows the login page or sends the browser through Access again.
const signIn = () => location.assign("/");

async function post(path) {
  const res = await fetch(path, { method: "POST", headers: { "x-automata": "1" } });
  if (res.status === 401) return signIn();
  if (!res.ok && res.status !== 409) alert(await res.text() || res.statusText);
}

function status(app) {
  if (app.busy) return ["busy", app.busy];
  const cs = app.containers;
  if (app.failed && !app.rev) return ["bad", "failed"];
  if (!cs.length) return app.rev ? ["bad", "stopped"] : ["muted", "not deployed"];
  const bad = cs.find(failing);
  if (bad) return ["bad", `${bad.service} ${bad.health === "unhealthy" ? "unhealthy" : bad.state}`];
  const up = cs.filter((c) => c.state === "running").length;
  if (app.failed) return ["bad", "last deploy failed"];
  if (cs.some((c) => c.health === "starting")) return ["busy", "starting"];
  return ["ok", up === cs.length ? "running" : `running ${up} of ${cs.length}`];
}

const failing = (c) => c.state === "restarting" || c.health === "unhealthy" || (c.state === "exited" && !c.status.startsWith("Exited (0)"));

const gh = (repo, path, text) => `<a class="mono" href="https://github.com/${esc(repo)}${path}" target="_blank" rel="noopener">${esc(text)}</a>`;
// "owner/repo" or "owner/repo#branch"; a local folder has no GitHub page.
const infraRepo = () => (/^[\w.-]+\/[\w.-]+/.test(view.infra.repo) ? view.infra.repo.split("#")[0] : null);

function container(app, c) {
  const cls = failing(c) ? "bad" : c.state !== "running" ? "muted" : c.health === "starting" ? "busy" : "ok";
  // Docker's status repeats the health in brackets; it has its own column.
  const uptime = c.status.replace(/\s*\((healthy|unhealthy|health: starting)\)/, "");
  // Images Compose built from source are named after the project, which says nothing new.
  const image = c.image.startsWith(`${app.name}-`) ? "built from source" : c.image;
  return `<div class="ctr">
    <span class="dot ${cls}"></span>
    <span class="svc">${esc(c.service)}</span>
    <span class="img mono" title="${esc(c.image)}">${esc(image)}</span>
    <span class="${cls === "ok" ? "muted" : cls}">${esc(c.health && c.health !== "healthy" ? c.health : uptime)}</span>
  </div>`;
}

function releaseInfo(app) {
  if (!app.rev) return `<span class="muted">not deployed</span>`;
  const parts = [];
  const repo = app.source?.split(/[#@]/)[0];
  if (repo && app.commit) parts.push(`<span>${gh(repo, `/commit/${app.commit}`, `${repo.split("/")[1]}@${app.commit.slice(0, 7)}`)}</span>`);
  const infra = infraRepo();
  if (app.infra_commit) {
    const sha = app.infra_commit.slice(0, 7);
    parts.push(infra ? `<span>infra ${gh(infra, `/commit/${app.infra_commit}`, sha)}</span>` : `<span class="mono">infra ${esc(sha)}</span>`);
  } else if (!app.commit) {
    parts.push(`<span class="mono" title="Hash of the app's folder in the infra repo">${esc(app.rev)}</span>`);
  }
  parts.push(`<span class="muted" title="${esc(new Date(app.deployed_at).toLocaleString())}">${ago(app.deployed_at)}</span>`);
  return parts.join("");
}

function backupInfo(b) {
  if (!b) return `<span class="muted">off</span>`;
  const last = b.last
    ? `<span class="${b.last.ok ? "ok" : "bad"}" title="${esc(b.last.message || "")}">${b.last.ok ? "done" : "failed"} ${ago(b.last.at)}</span>`
    : `<span class="muted">none yet</span>`;
  const next = b.next ? `<span class="muted" title="${esc(b.schedule)}">next ${in_((b.next - Date.now()) / 1000)}</span>` : "";
  return last + next;
}

function card(a) {
  const [cls, text] = status(a);
  const urls = a.urls.length
    ? a.urls.map((u) => `<a href="https://${esc(u.replace(/^\*\./, ""))}" target="_blank" rel="noopener">${esc(u)}</a>`).join("")
    : `<span class="muted">no domain</span>`;
  const containers = a.containers.length
    ? a.containers.map((c) => container(a, c)).join("")
    : `<span class="muted">${a.image ? esc(a.image) : "no containers"}</span>`;
  return `<article class="card${a.name === selected ? " sel" : ""}" data-name="${esc(a.name)}" tabindex="0">
    <div class="head">
      <strong>${esc(a.name)}</strong>
      <span class="pill ${cls}"><span class="dot ${cls}"></span>${esc(text)}</span>
    </div>
    ${a.error ? `<div class="err" title="${esc(a.error)}">${esc(a.error)}</div>` : ""}
    <dl>
      <dt>URLs</dt><dd class="urls">${urls}</dd>
      <dt>Containers</dt><dd class="ctrs">${containers}</dd>
      <dt>Release</dt><dd class="group">${releaseInfo(a)}</dd>
      <dt>Backup</dt><dd class="group">${backupInfo(a.backup)}</dd>
    </dl>
    <div class="foot">
      <button class="btn" data-open="logs">Logs</button>
      <button class="btn" data-open="task">Last task</button>
      ${a.backup ? `<button class="btn" data-open="backups">Backups</button>` : ""}
    </div>
  </article>`;
}

function render() {
  const v = view;
  const infra = infraRepo();
  $("infra").innerHTML = infra
    ? `${gh(infra, "", v.infra.repo)}${v.infra.commit ? gh(infra, `/commit/${v.infra.commit}`, v.infra.commit.slice(0, 7)) : ""}`
    : `<span class="mono">${esc(v.infra.repo)}</span>`;
  const auth = v.auth;
  const access = auth.access === "on";
  $("user").textContent = auth.user || "";
  $("logout").hidden = !(auth.password || access);
  $("synced").textContent = v.infra.synced_at ? `synced ${ago(v.infra.synced_at)}` : "";
  $("status").innerHTML = [
    v.busy ? `<span class="busy">${esc(v.busy)}</span>` : "",
    v.tunnel_error ? `<span class="bad">tunnel down</span>` : `<span class="ok">tunnel up</span>`,
    `<span class="muted">v${esc(v.version)}</span>`,
    v.latest ? `<span class="busy">${esc(v.latest)} available</span>` : "",
  ].join("");

  const alerts = [];
  if (v.infra.error) alerts.push(`infra: ${v.infra.error}`);
  if (v.tunnel_error) alerts.push(`tunnel: ${v.tunnel_error}`);
  if (v.self_error) alerts.push(`self-update: ${v.self_error}`);
  if (auth.access === "failed") alerts.push(`cloudflare access: ${auth.access_error}`);
  if (auth.access === "off" && !auth.password)
    alerts.push("The dashboard is open to anyone who can reach it. Add a Cloudflare Access application for its domain, or set DASHBOARD_PASSWORD.");
  for (const i of v.invalid) alerts.push(`apps/${i.name}: ${i.error}`);
  $("alerts").innerHTML = alerts.map((a) => `<div>${esc(a)}</div>`).join("");

  if (selected && !v.apps.some((a) => a.name === selected)) select(null);
  $("empty").hidden = v.apps.length > 0;
  $("apps").innerHTML = v.apps.map(card).join("");
  renderPane();
}

function current() {
  return view?.apps.find((a) => a.name === selected);
}

function renderPane() {
  const app = current();
  $("pane").hidden = !app;
  if (!app) return;
  $("pane-name").textContent = app.name;
  const img = app.images ? `images ${app.images.ok ? "checked" : "check failed"} ${ago(app.images.at)}${app.images.message ? `: ${app.images.message}` : ""}` : "";
  $("pane-source").innerHTML = [app.source || app.image, img]
    .filter(Boolean)
    .map((t) => `<span>${esc(t)}</span>`)
    .join("");
  const busy = !!app.busy;
  $("actions").innerHTML = [
    `<button class="btn" data-act="restart" ${busy || !app.rev ? "disabled" : ""} title="r">Restart</button>`,
    app.backup ? `<button class="btn" data-act="backup" ${busy || !app.rev ? "disabled" : ""} title="b">Back up now</button>` : "",
  ].join("");
  for (const b of $("tabs").querySelectorAll("button")) {
    b.classList.toggle("on", b.dataset.tab === tab);
    b.hidden = b.dataset.tab === "backups" && !app.backup;
  }
}

function select(name) {
  selected = name;
  name ? localStorage.setItem("selected", name) : localStorage.removeItem("selected");
  if (view) render();
  open();
}

function open() {
  stream?.close();
  stream = null;
  const out = $("out");
  out.textContent = "";
  const app = current();
  if (!app) return;
  if (tab === "backups") return snapshots(app.name);
  if (tab === "logs" && !app.rev) {
    out.innerHTML = `<span class="muted">not deployed</span>`;
    return;
  }
  stream = new EventSource(`/api/apps/${encodeURIComponent(app.name)}/${tab}`);
  // Until the first line arrives, say what is going on instead of showing nothing.
  let blank = true;
  out.innerHTML = `<span class="muted">${tab === "logs" ? "loading logs" : "loading"}</span>`;
  const which = stream;
  setTimeout(() => {
    if (!blank || stream !== which) return;
    out.innerHTML = tab === "logs"
      ? `<span class="muted">No output yet. ${esc(app.name)} has not written anything to stdout or stderr. New lines show up here as they are written.</span>`
      : `<span class="muted">No task has run for ${esc(app.name)} since Automata started.</span>`;
  }, 1500);
  let pending = [];
  let scheduled = false;
  const flush = () => {
    scheduled = false;
    if (blank) (blank = false), (out.textContent = "");
    const stick = out.scrollTop + out.clientHeight >= out.scrollHeight - 4;
    out.append(pending.join("\n") + "\n");
    pending = [];
    if (out.childNodes.length > 4000) out.textContent = out.textContent.split("\n").slice(-3000).join("\n");
    if (stick) out.scrollTop = out.scrollHeight;
  };
  stream.onmessage = (e) => {
    pending.push(e.data);
    if (!scheduled) (scheduled = true), requestAnimationFrame(flush);
  };
  stream.addEventListener("clear", () => {
    pending = [];
    blank = false;
    out.textContent = "";
  });
}

async function snapshots(name) {
  const out = $("out");
  out.innerHTML = `<span class="muted">loading snapshots</span>`;
  const res = await fetch(`/api/apps/${encodeURIComponent(name)}/snapshots`);
  if (res.status === 401) return signIn();
  if (selected !== name || tab !== "backups") return;
  if (!res.ok) return (out.innerHTML = `<span class="bad">${esc(await res.text())}</span>`);
  const list = await res.json();
  if (!list.length) return (out.innerHTML = `<span class="muted">no snapshots yet</span>`);
  const size = (b) => (b == null ? "" : b > 1e9 ? `${(b / 1e9).toFixed(1)} GB` : `${(b / 1e6).toFixed(1)} MB`);
  out.innerHTML = `<table>${list
    .map((s) => `<tr><td>${esc(s.id.split("/")[0])}</td><td>${esc(new Date(s.time).toLocaleString())}</td><td class="muted">${size(s.size)}</td><td><button class="btn" data-restore="${esc(s.id)}">restore</button></td></tr>`)
    .join("")}</table>`;
}

$("apps").addEventListener("click", (e) => {
  if (e.target.closest("a")) return;
  const card = e.target.closest("[data-name]");
  if (!card) return;
  const b = e.target.closest("button[data-open]");
  if (b) {
    tab = b.dataset.open;
    localStorage.setItem("tab", tab);
    return select(card.dataset.name);
  }
  // Selecting text in a card should not toggle it.
  if (getSelection().toString()) return;
  select(card.dataset.name === selected ? null : card.dataset.name);
});
$("apps").addEventListener("keydown", (e) => {
  const card = e.target.closest("[data-name]");
  if (card && e.key === "Enter" && e.target === card) select(card.dataset.name);
});
$("close").addEventListener("click", () => select(null));
$("tabs").addEventListener("click", (e) => {
  const b = e.target.closest("button[data-tab]");
  if (b) setTab(b.dataset.tab);
});
$("actions").addEventListener("click", (e) => {
  const b = e.target.closest("button[data-act]");
  if (b) act(b.dataset.act);
});
$("out").addEventListener("click", (e) => {
  const b = e.target.closest("button[data-restore]");
  const app = current();
  if (!b || !app) return;
  if (!confirm(`Stop ${app.name} and replace its volumes with snapshot ${b.dataset.restore}?`)) return;
  post(`/api/apps/${encodeURIComponent(app.name)}/restore?snapshot=${encodeURIComponent(b.dataset.restore)}`);
  setTab("task");
});
$("sync").addEventListener("click", () => post("/api/sync"));

function setTab(t) {
  tab = t;
  localStorage.setItem("tab", t);
  renderPane();
  open();
}

function act(a) {
  const app = current();
  if (!app || app.busy || !app.rev) return;
  if (a === "backup" && !app.backup) return;
  post(`/api/apps/${encodeURIComponent(app.name)}/${a}`);
  setTab("task");
}

document.addEventListener("keydown", (e) => {
  if (e.metaKey || e.ctrlKey || e.altKey || e.target.closest("input, textarea")) return;
  const names = view?.apps.map((a) => a.name) ?? [];
  const i = names.indexOf(selected);
  const keys = {
    j: () => names.length && select(names[Math.min(i + 1, names.length - 1)]),
    k: () => names.length && select(names[Math.max(i - 1, 0)]),
    Escape: () => select(null),
    l: () => current() && setTab("logs"),
    t: () => current() && setTab("task"),
    s: () => post("/api/sync"),
    r: () => act("restart"),
    b: () => act("backup"),
  };
  keys[e.key]?.();
});

async function refresh() {
  const res = await fetch("/api/state");
  if (res.status === 401) return signIn();
  if (!res.ok) return;
  const first = !view;
  view = await res.json();
  render();
  if (first) open();
}

const events = new EventSource("/api/events");
events.addEventListener("change", refresh);
// An EventSource hides the status code, so ask again to find out whether it was a 401.
events.addEventListener("error", refresh);
$("logout").addEventListener("click", async () => {
  if (view?.auth.password) await fetch("/logout", { method: "POST", headers: { "x-automata": "1" } });
  // Cloudflare answers this path itself and ends the Access session.
  if (view?.auth.access === "on") return location.assign("/cdn-cgi/access/logout");
  signIn();
});
setInterval(() => view && render(), 30000);
