const COMMANDS = [
  ["Keys & strings", [
    ["SET", "Set a key, optional EX / PX expiry"],
    ["GET", "Return a string value"],
    ["DEL", "Remove one or more keys"],
    ["EXPIRE", "Set a time to live in seconds"],
    ["TTL", "Seconds remaining, or -1 / -2"],
    ["PERSIST", "Remove a key's expiry"],
    ["TYPE", "Report the value type stored at a key"],
    ["DBSIZE", "Count keys currently stored"],
  ]],
  ["Connection", [
    ["PING", "Health check; replies PONG"],
    ["ECHO", "Return the argument unchanged"],
    ["INFO", "Server, memory, keyspace, and replication figures"],
  ]],
  ["Lists", [
    ["RPUSH", "Append one or more values"],
    ["LPUSH", "Prepend one or more values"],
    ["RPOP", "Remove and return the last element"],
    ["LPOP", "Remove and return the first element"],
    ["LLEN", "Length of a list"],
    ["LRANGE", "Slice of a list, inclusive"],
  ]],
  ["Hashes", [
    ["HSET", "Set one or more fields"],
    ["HGET", "Return one field"],
    ["HGETALL", "Return every field and value"],
    ["HDEL", "Delete one or more fields"],
  ]],
  ["Streams", [
    ["XADD", "Append an entry"],
    ["XLEN", "Number of entries"],
    ["XRANGE", "Entries in id order"],
    ["XREVRANGE", "Entries in reverse id order"],
    ["XDEL", "Delete entries by id"],
    ["XTRIM", "Cap a stream with MAXLEN"],
    ["XREAD", "Read entries after an id"],
  ]],
  ["Sorted sets", [
    ["ZADD", "Add members with scores"],
    ["ZSCORE", "Score of one member"],
    ["ZRANK", "Rank of one member"],
    ["ZRANGE", "Members by rank"],
    ["ZCARD", "Number of members"],
    ["ZREM", "Remove members"],
    ["ZINCRBY", "Add to a member's score"],
    ["ZRANGEBYSCORE", "Members inside a score range"],
    ["ZCOUNT", "Count members inside a score range"],
  ]],
  ["Pub/sub", [
    ["PUBLISH", "Send a message to a channel"],
    ["SUBSCRIBE", "Listen on a channel (a poor fit for this bridge)"],
    ["UNSUBSCRIBE", "Leave a channel"],
  ]],
  ["Transactions", [
    ["MULTI", "Start a transaction"],
    ["EXEC", "Run the queued commands"],
    ["DISCARD", "Drop the queued commands"],
    ["WATCH", "Watch keys for changes before EXEC"],
    ["UNWATCH", "Clear watched keys"],
  ]],
  ["Persistence", [
    ["SAVE", "Write an RDB snapshot on this thread"],
    ["BGSAVE", "Write an RDB snapshot in the background"],
    ["WAIT", "Block until replicas acknowledge writes"],
  ]],
];

const TYPE_COLORS = {
  string: "var(--string)",
  list: "var(--list)",
  hash: "var(--hash)",
  stream: "var(--stream)",
  zset: "var(--zset)",
};

const log = document.querySelector("#log");
const form = document.querySelector("#composer");
const input = document.querySelector("#command");
const status = document.querySelector("#status");
const commandList = document.querySelector("#command-list");
const commandSearch = document.querySelector("#command-search");
const STORAGE_KEY = "flashdb.playground.history";
const MAX_HISTORY = 40;

function el(tag, className, text) {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (text != null) node.textContent = text;
  return node;
}

function loadHistory() {
  try {
    const raw = localStorage.getItem(STORAGE_KEY);
    const parsed = raw ? JSON.parse(raw) : [];
    return Array.isArray(parsed) ? parsed.slice(-MAX_HISTORY) : [];
  } catch {
    return [];
  }
}

function saveHistory(items) {
  try {
    localStorage.setItem(STORAGE_KEY, JSON.stringify(items.slice(-MAX_HISTORY)));
  } catch {
    // Private mode or a full disk. The session still works without persistence.
  }
}

let history = loadHistory();
let socket = null;
let retryMs = 800;
let connected = false;
const queue = [];
let inflight = null;
let instanceTimer = null;
let refreshing = false;

function renderCommands(query) {
  const needle = query.trim().toLowerCase();
  commandList.replaceChildren();
  let shown = 0;
  for (const [group, rows] of COMMANDS) {
    const matches = rows.filter(([name, desc]) => {
      if (!needle) return true;
      return name.toLowerCase().includes(needle) || desc.toLowerCase().includes(needle);
    });
    if (matches.length === 0) continue;
    shown += matches.length;
    const section = el("section", "group");
    section.append(el("h2", null, group));
    const card = el("div", "rows-card");
    for (const [name, desc] of matches) {
      const row = el("div", "cmd-row");
      row.append(el("div", "cmd-name", name), el("div", "cmd-desc", desc));
      card.append(row);
    }
    section.append(card);
    commandList.append(section);
  }
  if (shown === 0) {
    commandList.append(el("p", "empty-search", "No commands match."));
  }
}

function showScreen(name) {
  for (const screen of document.querySelectorAll(".screen")) {
    const on = screen.id === `screen-${name}`;
    screen.hidden = !on;
  }
  for (const tab of document.querySelectorAll(".tabs button")) {
    const on = tab.dataset.screen === name;
    tab.setAttribute("aria-selected", on ? "true" : "false");
  }
  if (name === "instance") refreshInstance();
  if (name === "console") input.focus();
}

document.querySelector(".tabs").addEventListener("click", (event) => {
  const tab = event.target.closest("button[data-screen]");
  if (!tab) return;
  location.hash = tab.dataset.screen;
});

function screenFromHash() {
  const name = location.hash.replace(/^#/, "");
  return ["home", "commands", "instance", "console"].includes(name) ? name : "home";
}

commandSearch.addEventListener("input", () => renderCommands(commandSearch.value));

function render() {
  log.replaceChildren();
  if (history.length === 0) {
    log.append(el("p", "empty", "Try PING, or SET greeting hello. Tap a past command to put it back in the box."));
    return;
  }
  for (const item of history) {
    const entry = el("article", "entry");
    const button = el("button", "cmd", item.cmd);
    button.type = "button";
    button.addEventListener("click", () => {
      input.value = item.cmd;
      input.focus();
    });
    entry.append(button);
    if (item.reply != null) {
      const reply = el("pre", "reply", item.reply);
      if (String(item.reply).startsWith("(error)")) reply.classList.add("error");
      entry.append(reply);
    }
    log.append(entry);
  }
  log.scrollTop = log.scrollHeight;
}

function setStatus(text) {
  status.textContent = text;
  const line = document.querySelector("#instance-link");
  const label = document.querySelector("#instance-status");
  label.textContent = connected ? `Up ${location.host}` : text;
  line.classList.toggle("is-down", !connected);
}

function pump() {
  if (inflight || queue.length === 0) return;
  if (!socket || socket.readyState !== WebSocket.OPEN) return;
  inflight = queue.shift();
  socket.send(inflight.command);
}

function rpc(command) {
  return new Promise((resolve) => {
    queue.push({ command, resolve });
    pump();
  });
}

function failQueued(message) {
  if (inflight) {
    inflight.resolve(message);
    inflight = null;
  }
  while (queue.length) queue.shift().resolve(message);
}

function playgroundSocketUrl() {
  const query = new URLSearchParams(location.search).get("ws");
  if (query) return query;
  if (window.FLASHDB_WS_URL) return window.FLASHDB_WS_URL;
  const proto = location.protocol === "https:" ? "wss" : "ws";
  return `${proto}://${location.host}/ws`;
}

function connect() {
  socket = new WebSocket(playgroundSocketUrl());
  connected = false;
  setStatus("Connecting…");
  socket.addEventListener("open", () => {
    connected = true;
    retryMs = 800;
    setStatus("Connected");
    pump();
    if (!document.querySelector("#screen-instance").hidden) refreshInstance();
  });
  socket.addEventListener("close", () => {
    connected = false;
    setStatus("Disconnected — retrying");
    failQueued("(error) disconnected");
    socket = null;
    window.setTimeout(connect, retryMs);
    retryMs = Math.min(retryMs * 2, 5000);
  });
  socket.addEventListener("message", (event) => {
    const current = inflight;
    inflight = null;
    if (current) current.resolve(String(event.data));
    pump();
  });
}

function parseInfo(text) {
  const map = {};
  if (!text || text.startsWith("(error)")) return { error: text, map };
  for (const line of text.split(/\r?\n/)) {
    if (!line || line.startsWith("#")) continue;
    const cut = line.indexOf(":");
    if (cut > 0) map[line.slice(0, cut).trim()] = line.slice(cut + 1).trim();
  }
  return { error: null, map };
}

function parseInteger(text) {
  const match = /^\(integer\)\s+(-?\d+)/.exec(text || "");
  return match ? Number(match[1]) : null;
}

function formatBytes(value) {
  const n = Number(value);
  if (!Number.isFinite(n)) return null;
  if (n < 1024) return `${n} B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KB`;
  return `${(n / (1024 * 1024)).toFixed(1)} MB`;
}

function formatUptime(seconds) {
  const n = Number(seconds);
  if (!Number.isFinite(n)) return null;
  const minutes = Math.floor(n / 60);
  if (minutes < 60) return `${minutes}m`;
  return `${Math.floor(minutes / 60)}h ${minutes % 60}m`;
}

function formatSave(unix) {
  const n = Number(unix);
  if (!Number.isFinite(n) || n <= 0) return null;
  const date = new Date(n * 1000);
  if (Number.isNaN(date.getTime())) return null;
  return date.toLocaleString();
}

function typeCounts(map, total) {
  const named = ["string", "list", "hash", "stream", "zset"].map((type) => {
    const raw = map[`${type}_keys`] ?? map[`keys_${type}`] ?? map[type];
    const count = Number(raw);
    return { type, count: Number.isFinite(count) ? count : null };
  });
  if (named.some((item) => item.count != null)) {
    return named.map((item) => ({ type: item.type, count: item.count || 0 }));
  }
  if (total != null) return [{ type: "keys", count: total, color: "var(--flash)" }];
  return [];
}

function paintTypes(counts) {
  const bar = document.querySelector("#type-bar");
  const legend = document.querySelector("#type-legend");
  bar.replaceChildren();
  legend.replaceChildren();
  const sum = counts.reduce((acc, item) => acc + item.count, 0);
  if (sum <= 0) {
    legend.append(el("li", null, "0 keys"));
    return;
  }
  for (const item of counts) {
    if (item.count <= 0) continue;
    const segment = document.createElement("span");
    segment.style.width = `${(item.count / sum) * 100}%`;
    segment.style.background = item.color || TYPE_COLORS[item.type] || "var(--flash)";
    bar.append(segment);
    const label = el("li");
    const swatch = document.createElement("i");
    swatch.style.background = segment.style.background;
    label.append(swatch, document.createTextNode(`${item.type} ${item.count}`));
    legend.append(label);
  }
}

function textOrDash(node, value) {
  node.textContent = value == null || value === "" ? "—" : String(value);
}

function applyMetrics(dbsizeText, infoText) {
  const note = document.querySelector("#instance-note");
  const keys = parseInteger(dbsizeText);
  const info = parseInfo(infoText);
  const problems = [];
  if (dbsizeText.startsWith("(error)")) problems.push(dbsizeText.replace(/^\(error\)\s*/, ""));
  if (info.error) problems.push(info.error.replace(/^\(error\)\s*/, ""));
  if (problems.length) {
    note.hidden = false;
    note.textContent = problems.join(" · ");
  } else {
    note.hidden = true;
    note.textContent = "";
  }
  textOrDash(document.querySelector("#stat-keys"), keys);
  const memory = info.map.used_memory_human || formatBytes(info.map.used_memory_rss || info.map.used_memory);
  textOrDash(document.querySelector("#stat-memory"), memory);
  const role = info.map.role;
  textOrDash(document.querySelector("#stat-role"), role ? role[0].toUpperCase() + role.slice(1) : null);
  const replicas = info.map.connected_slaves ?? info.map.connected_replicas;
  document.querySelector("#stat-replicas").textContent = replicas == null ? "—" : `${replicas} connected replicas`;
  const saved = formatSave(info.map.rdb_last_save_time);
  textOrDash(document.querySelector("#stat-save"), saved ? saved : "—");
  document.querySelector("#stat-save-note").textContent = saved ? "Last successful snapshot" : "No snapshot yet";
  textOrDash(document.querySelector("#row-file"), info.map.dbfilename);
  if (role === "slave" || role === "replica") {
    const host = info.map.master_host;
    const port = info.map.master_port;
    textOrDash(document.querySelector("#row-master"), host ? `${host}:${port || ""}` : null);
  } else if (role === "master") {
    document.querySelector("#row-master").textContent = "— (this is master)";
  } else {
    document.querySelector("#row-master").textContent = "—";
  }
  textOrDash(document.querySelector("#row-replicas"), replicas);
  textOrDash(document.querySelector("#row-uptime"), formatUptime(info.map.uptime_in_seconds));
  paintTypes(typeCounts(info.map, keys));
}

async function refreshInstance() {
  if (refreshing || !connected) return;
  refreshing = true;
  try {
    const dbsize = await rpc("DBSIZE");
    const info = await rpc("INFO");
    applyMetrics(dbsize, info);
  } finally {
    refreshing = false;
  }
}

form.addEventListener("submit", async (event) => {
  event.preventDefault();
  const cmd = input.value.trim();
  if (!cmd) return;
  if (!socket || socket.readyState !== WebSocket.OPEN) {
    setStatus("Not connected yet");
    return;
  }
  const item = { cmd, reply: null };
  history.push(item);
  if (history.length > MAX_HISTORY) history = history.slice(-MAX_HISTORY);
  saveHistory(history);
  render();
  input.value = "";
  item.reply = await rpc(cmd);
  saveHistory(history);
  render();
});

function liftComposer() {
  const viewport = window.visualViewport;
  if (!viewport) return;
  const covered = Math.max(0, window.innerHeight - viewport.offsetTop - viewport.height);
  document.documentElement.style.setProperty("--keyboard-inset", `${covered}px`);
}
if (window.visualViewport) {
  window.visualViewport.addEventListener("resize", liftComposer);
  window.visualViewport.addEventListener("scroll", liftComposer);
  liftComposer();
}

if ("serviceWorker" in navigator) {
  navigator.serviceWorker.register("/sw.js").catch(() => {});
}

window.addEventListener("hashchange", () => showScreen(screenFromHash()));
renderCommands("");
render();
showScreen(screenFromHash());
connect();
instanceTimer = window.setInterval(() => {
  if (!document.querySelector("#screen-instance").hidden) refreshInstance();
}, 3000);
