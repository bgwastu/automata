import { $, icon } from "/api/dashboard/view.js";

function entry(raw) {
  const match = raw.match(/^(.+?)\s+\|\s?(\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d(?:\.\d+)?Z)(?: (.*))?$/);
  if (!match) return { raw, message: raw };
  const [, source, timestamp, message = ""] = match;
  return { raw, source: source.trim(), timestamp, message };
}

function line(record) {
  const row = document.createElement("div");
  row.className = record.source ? "log-entry" : "log-entry raw";
  if (record.source) {
    const when = document.createElement("time");
    when.dateTime = record.timestamp;
    when.title = record.timestamp + " (UTC)";
    when.textContent = record.timestamp.slice(11, 19);
    const service = document.createElement("b");
    service.textContent = record.source;
    row.append(when, service);
  }
  row.raw = record.raw;
  const message = document.createElement("span");
  message.textContent = record.message || " ";
  row.append(message);
  return row;
}

export function followOutput(url, tab, refresh) {
  const log = $("log");
  const search = $("log-search");
  const pause = $("log-pause");
  const copy = $("log-copy");
  const status = $("stream-status");
  const records = [];
  const known = new Set();
  const cap = 4000;
  let cursor = 0;
  let paused = false;
  let frame = 0;
  let connected = false;
  let closed = false;
  let reconnecting = false;
  let copyResult = "";
  const matches = (record) => record.raw.toLowerCase().includes(search.value.trim().toLowerCase());

  function showStatus() {
    const parts = [];
    if (paused) parts.push("Paused");
    if (reconnecting) parts.push("Connection lost. Reconnecting…");
    else if (!connected) parts.push("Connecting…");
    else if (!records.length) parts.push(tab === "logs" ? "No container output yet." : "No task output yet.");
    else if (log.hidden && search.value) parts.push("No matching output.");
    else if (!paused) parts.push("Live");
    if (copyResult) parts.push(copyResult);
    const text = parts.join(" ");
    if (status.textContent !== text) status.textContent = text;
    copy.disabled = log.hidden;
  }

  function draw(reset = false, follow = false) {
    if (reset) {
      log.replaceChildren();
      cursor = 0;
    }
    const atEnd = follow || log.scrollTop + log.clientHeight >= log.scrollHeight - 4;
    const chunk = document.createDocumentFragment();
    for (; cursor < records.length; cursor += 1) {
      const record = records[cursor];
      if (matches(record)) {
        record.node = line(record);
        chunk.append(record.node);
      }
    }
    log.append(chunk);
    log.hidden = !log.childElementCount;
    if (atEnd) log.scrollTop = log.scrollHeight;
    showStatus();
  }

  const renderPause = () => { pause.innerHTML = paused ? icon("play") + "Resume" : icon("pause") + "Pause"; };
  log.hidden = true;
  log.replaceChildren();
  search.value = "";
  search.disabled = !url;
  pause.disabled = !url;
  renderPause();
  showStatus();
  if (!url) {
    status.textContent = "This app has not been deployed.";
    return;
  }

  const stream = new EventSource(url);
  stream.onopen = () => {
    connected = true;
    reconnecting = false;
    showStatus();
  };
  stream.onerror = () => {
    connected = false;
    reconnecting = true;
    showStatus();
    refresh();
  };
  stream.onmessage = ({ data }) => {
    const record = tab === "logs" ? entry(data) : { raw: data, message: data };
    if (record.timestamp && known.has(data)) return;
    if (record.timestamp) known.add(data);
    records.push(record);
    if (!frame) frame = requestAnimationFrame(() => {
      frame = 0;
      const removed = records.splice(0, Math.max(records.length - cap, 0));
      const before = log.scrollHeight;
      const atEnd = log.scrollTop + log.clientHeight >= before - 4;
      for (const old of removed) {
        known.delete(old.raw);
        if (!paused) old.node?.remove();
      }
      cursor = Math.max(0, cursor - removed.length);
      if (paused) return;
      if (!atEnd && before > log.scrollHeight) log.scrollTop -= before - log.scrollHeight;
      draw(false, atEnd);
    });
  };
  stream.addEventListener("clear", () => {
    records.length = 0;
    known.clear();
    if (!paused) draw(true);
  });
  search.oninput = () => {
    copyResult = "";
    if (paused) {
      const query = search.value.trim().toLowerCase();
      for (const row of log.children) row.hidden = !row.raw.toLowerCase().includes(query);
      log.hidden = ![...log.children].some((row) => !row.hidden);
      showStatus();
    } else draw(true, true);
  };
  pause.onclick = () => {
    paused = !paused;
    renderPause();
    if (paused) showStatus();
    else draw(true, true);
  };
  copy.onclick = async () => {
    try {
      await navigator.clipboard.writeText([...log.children].filter((row) => !row.hidden).map((row) => row.raw).join("\n"));
      copyResult = "Copied.";
    } catch {
      copyResult = "Could not copy output.";
    }
    if (!closed) showStatus();
  };
  return () => {
    closed = true;
    stream.close();
    cancelAnimationFrame(frame);
    search.oninput = null;
    pause.onclick = null;
    copy.onclick = null;
  };
}
