const log = document.querySelector("#log");
const form = document.querySelector("#composer");
const input = document.querySelector("#command");
const status = document.querySelector("#status");
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
let waiting = false;

function render() {
  log.replaceChildren();
  if (history.length === 0) {
    log.append(
      el(
        "p",
        "empty",
        "Try PING, or SET greeting hello. Tap a past command to put it back in the box.",
      ),
    );
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

function connect() {
  const proto = location.protocol === "https:" ? "wss" : "ws";
  socket = new WebSocket(`${proto}://${location.host}/ws`);
  status.textContent = "Connecting…";
  socket.addEventListener("open", () => {
    status.textContent = "Connected";
    retryMs = 800;
  });
  socket.addEventListener("close", () => {
    status.textContent = "Disconnected — retrying";
    socket = null;
    waiting = false;
    window.setTimeout(connect, retryMs);
    retryMs = Math.min(retryMs * 2, 5000);
  });
  socket.addEventListener("message", (event) => {
    waiting = false;
    const last = history[history.length - 1];
    if (last && last.reply == null) {
      last.reply = String(event.data);
      saveHistory(history);
      render();
    }
  });
}

form.addEventListener("submit", (event) => {
  event.preventDefault();
  const cmd = input.value.trim();
  if (!cmd || waiting) return;
  if (!socket || socket.readyState !== WebSocket.OPEN) {
    status.textContent = "Not connected yet";
    return;
  }
  history.push({ cmd, reply: null });
  if (history.length > MAX_HISTORY) history = history.slice(-MAX_HISTORY);
  saveHistory(history);
  render();
  waiting = true;
  socket.send(cmd);
  input.value = "";
});

if ("serviceWorker" in navigator) {
  navigator.serviceWorker.register("/sw.js").catch(() => {});
}

// iOS keeps the layout viewport full-height when the keyboard opens, so a
// bottom composer sits underneath it. visualViewport is the area still
// visible; shift the composer up by the covered strip.
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

render();
connect();
