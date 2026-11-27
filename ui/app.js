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

async function post(path) {
  const res = await fetch(path, { method: "POST", headers: { "x-automata": "1" } });
  if (!res.ok && res.status !== 409) alert(await res.text() || res.statusText);
}

function status(app) {
  if (app.busy) return ["busy", app.busy];
  const cs = app.containers;
  if (app.failed && !app.rev) return ["bad", "failed"];
  if (!cs.length) return app.rev ? ["bad", "stopped"] : ["muted", "not deployed"];
  const bad = cs.find((c) => c.state === "restarting" || c.health === "unhealthy" || (c.state === "exited" && !c.status.startsWith("Exited (0)")));
  if (bad) return ["bad", `${bad.service} ${bad.health === "unhealthy" ? "unhealthy" : bad.state}`];
  const up = cs.filter((c) => c.state === "running").length;
  if (app.failed) return ["bad", `running, ${app.failed} failed`];
  if (cs.some((c) => c.health === "starting")) return ["busy", "starting"];
  return ["ok", up === cs.length ? "running" : `running ${up}/${cs.length}`];
}

function backupCell(b) {
  if (!b) return `<span class="muted">off</span>`;
  const last = b.last ? (b.last.ok ? `<span class="ok">ok</span> ${ago(b.last.at)}` : `<span class="bad">failed</span> ${ago(b.last.at)}`) : `<span class="muted">none yet</span>`;
  return `${last} <span class="muted">· next ${b.next ? in_((b.next - Date.now()) / 1000) : "?"}</span>`;
}

function release(app) {
  if (!app.rev) return `<span class="muted">none</span>`;
  const repo = app.source?.split(/[#@]/)[0];
  return repo && app.commit
    ? `<a class="mono" href="https://github.com/${esc(repo)}/commit/${esc(app.commit)}" target="_blank">${esc(app.rev)}</a>`
    : `<span class="mono">${esc(app.rev)}</span>`;
}

function render() {
  const v = view;
  const infra = v.infra.commit ? `${v.infra.repo}@${v.infra.commit.slice(0, 7)}` : v.infra.repo;
  $("infra").textContent = infra;
  $("synced").textContent = v.infra.synced_at ? `synced ${ago(v.infra.synced_at)}` : "";
  $("tunnel").innerHTML = [
    v.busy ? `<span class="busy">${esc(v.busy)}</span>` : "",
    v.tunnel_error ? `<span class="bad">tunnel down</span>` : `<span class="ok">tunnel up</span>`,
    `<span class="muted">v${esc(v.version)}${v.latest ? `, ${esc(v.latest)} available` : ""}</span>`,
  ].join(" ");

  const alerts = [];
  if (v.infra.error) alerts.push(`infra: ${v.infra.error}`);
  if (v.tunnel_error) alerts.push(`tunnel: ${v.tunnel_error}`);
  if (v.self_error) alerts.push(`self-update: ${v.self_error}`);
  for (const i of v.invalid) alerts.push(`apps/${i.name}: ${i.error}`);
  $("alerts").innerHTML = alerts.map((a) => `<div>${esc(a)}</div>`).join("");

  if (selected && !v.apps.some((a) => a.name === selected)) select(null);
  $("empty").hidden = v.apps.length > 0;
  $("apps").innerHTML = v.apps
    .map((a) => {
      const [cls, text] = status(a);
      const domain = a.domains[0]
        ? `<a href="https://${esc(a.domains[0].replace(/^\*\./, ""))}" target="_blank">${esc(a.domains[0])}</a>${a.domains.length > 1 ? ` <span class="muted">+${a.domains.length - 1}</span>` : ""}`
        : `<span class="muted">none</span>`;
      const sel = a.name === selected ? " sel" : "";
      const row = `<tr data-name="${esc(a.name)}" class="${sel}${a.error ? " has-err" : ""}">
        <td>${esc(a.name)}</td><td class="${cls}">${esc(text)}</td><td>${domain}</td>
        <td>${release(a)}</td><td>${ago(a.deployed_at)}</td><td>${backupCell(a.backup)}</td></tr>`;
      const err = a.error ? `<tr data-name="${esc(a.name)}" class="err${sel}"><td colspan="6" title="${esc(a.error)}">${esc(a.error)}</td></tr>` : "";
      return row + err;
    })
    .join("");
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
  const img = app.images ? `images ${app.images.ok ? "checked" : "failed"} ${ago(app.images.at)}${app.images.message ? `: ${app.images.message}` : ""}` : "";
  $("pane-source").textContent = [app.source, img].filter(Boolean).join(" · ");
  const busy = !!app.busy;
  $("actions").innerHTML = [
    `<button data-act="restart" ${busy || !app.rev ? "disabled" : ""} title="r">restart</button>`,
    app.backup ? `<button data-act="backup" ${busy || !app.rev ? "disabled" : ""} title="b">back up now</button>` : "",
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
  let pending = [];
  let scheduled = false;
  const flush = () => {
    scheduled = false;
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
    out.textContent = "";
  });
}

async function snapshots(name) {
  const out = $("out");
  out.innerHTML = `<span class="muted">loading snapshots</span>`;
  const res = await fetch(`/api/apps/${encodeURIComponent(name)}/snapshots`);
  if (selected !== name || tab !== "backups") return;
  if (!res.ok) return (out.innerHTML = `<span class="bad">${esc(await res.text())}</span>`);
  const list = await res.json();
  if (!list.length) return (out.innerHTML = `<span class="muted">no snapshots yet</span>`);
  const size = (b) => (b == null ? "" : b > 1e9 ? `${(b / 1e9).toFixed(1)} GB` : `${(b / 1e6).toFixed(1)} MB`);
  out.innerHTML = `<table>${list
    .map((s) => `<tr><td>${esc(s.id)}</td><td>${esc(new Date(s.time).toLocaleString())}</td><td class="muted">${size(s.size)}</td><td><button data-restore="${esc(s.id)}">restore</button></td></tr>`)
    .join("")}</table>`;
}

$("apps").addEventListener("click", (e) => {
  if (e.target.closest("a")) return;
  const tr = e.target.closest("tr[data-name]");
  if (tr) select(tr.dataset.name === selected ? null : tr.dataset.name);
});
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
  if (!res.ok) return;
  const first = !view;
  view = await res.json();
  render();
  if (first) open();
}

new EventSource("/api/events").addEventListener("change", refresh);
setInterval(() => view && render(), 30000);
