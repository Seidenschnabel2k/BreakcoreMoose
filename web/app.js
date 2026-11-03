// Breakcore Moose web player. No framework, no build step: the bot embeds
// this file. All playback state lives on the server; this page renders what
// the WebSocket sends and turns clicks into small JSON messages.
"use strict";

const $ = (sel) => document.querySelector(sel);
const el = (tag, cls, text) => {
  const e = document.createElement(tag);
  if (cls) e.className = cls;
  if (text != null) e.textContent = text;
  return e;
};

function fmt(secs) {
  if (secs == null || !isFinite(secs)) return "--:--";
  secs = Math.max(0, Math.floor(secs));
  const h = Math.floor(secs / 3600), m = Math.floor(secs / 60) % 60, s = secs % 60;
  const ss = String(s).padStart(2, "0");
  return h ? `${h}:${String(m).padStart(2, "0")}:${ss}` : `${m}:${ss}`;
}

// ---------------------------------------------------------------- state

const S = {
  guild: null,
  ws: null,
  queue: [],          // [{id, track}]
  priority: 0,        // how many queue entries are priority
  now: null,          // {entry_id, track, position_secs, paused}
  nowAt: 0,           // performance.now() when `now` arrived
  volume: 100,
  repeat: "off",
  selected: new Set(),
  anchor: null,       // last clicked entry id, for shift-select
  showRemaining: false,
  seeking: false,
  volumeTouched: 0,
  levels: new Array(20).fill(0),
  source: "youtube",
  kind: "song",
  crumb: null,        // {id, name} when browsing into a Jellyfin container
  jellyfin: false,
};

// ---------------------------------------------------------------- toasts

function toast(text, error = false) {
  const t = el("div", "toast" + (error ? " error" : ""), text);
  $("#toasts").append(t);
  setTimeout(() => t.remove(), error ? 6000 : 3500);
}

// ---------------------------------------------------------------- gate

async function boot() {
  const match = location.pathname.match(/^\/g\/(\d+)/);
  let me;
  try {
    const r = await fetch("/api/me");
    if (r.status === 401) return showLogin();
    me = await r.json();
  } catch {
    return showGate("CAN'T REACH THE BOT.\nRETRYING…", [], () => setTimeout(boot, 3000));
  }
  S.jellyfin = me.jellyfin;
  if (match) {
    const guild = me.guilds.find((g) => g.id === match[1]);
    if (!guild) return showGate("YOU'RE NOT IN THAT SERVER\n(OR THE BOT ISN'T).", serverButtons(me.guilds));
    return start(guild, me.user);
  }
  if (me.guilds.length === 1) return location.replace(`/g/${me.guilds[0].id}`);
  if (!me.guilds.length) return showGate(`HI ${me.user.name.toUpperCase()}.\nNONE OF YOUR SERVERS HAS THE BOT.`, []);
  showGate(`HI ${me.user.name.toUpperCase()}.\nPICK A SERVER:`, serverButtons(me.guilds));
}

function serverButtons(guilds) {
  return guilds.map((g) => {
    const b = el("button", "btn", g.name);
    b.onclick = () => (location.href = `/g/${g.id}`);
    return b;
  });
}

function showLogin() {
  const b = el("button", "btn", "LOG IN WITH FLUXER");
  b.onclick = () => (location.href = "/auth/login?next=" + encodeURIComponent(location.pathname));
  showGate("BREAKCORE MOOSE\n\nLOG IN TO CONTROL THE MUSIC.\nONLY MEMBERS OF THE SERVER CAN.", [b]);
}

function showGate(text, buttons, then) {
  $("#app").hidden = true;
  $("#gate").hidden = false;
  $("#gate-text").textContent = text;
  $("#gate-actions").replaceChildren(...buttons);
  if (then) then();
}

function start(guild, user) {
  S.guild = guild;
  document.title = `${guild.name} · Breakcore Moose`;
  $("#gate").hidden = true;
  $("#app").hidden = false;
  $("#who").textContent = user.name;
  $("#tab-jellyfin").disabled = !S.jellyfin;
  $("#tab-jellyfin").title = S.jellyfin ? "" : "Jellyfin isn't set up on the bot";
  connect();
  requestAnimationFrame(frame);
}

// ---------------------------------------------------------------- socket

let backoff = 500;

function connect() {
  const proto = location.protocol === "https:" ? "wss:" : "ws:";
  const ws = new WebSocket(`${proto}//${location.host}/api/g/${S.guild.id}/ws`);
  S.ws = ws;
  ws.onopen = () => (backoff = 500);
  ws.onmessage = (e) => receive(JSON.parse(e.data));
  ws.onclose = (e) => {
    if (S.ws !== ws) return;
    S.ws = null;
    setMarquee("CONNECTION LOST · RECONNECTING…");
    setTimeout(async () => {
      // A dead session shows the login again instead of looping.
      const r = await fetch("/api/me").catch(() => null);
      if (r && r.status === 401) return showLogin();
      connect();
    }, backoff);
    backoff = Math.min(backoff * 2, 15000);
  };
}

function send(msg) {
  if (S.ws && S.ws.readyState === WebSocket.OPEN) {
    S.ws.send(JSON.stringify(msg));
    return true;
  }
  toast("Not connected.", true);
  return false;
}

function receive(m) {
  switch (m.t) {
    case "hello":
      S.jellyfin = m.jellyfin;
      $("#volume").max = m.max_volume;
      break;
    case "queue": {
      S.queue = m.entries;
      S.priority = m.priority;
      const ids = new Set(S.queue.map((e) => e.id));
      for (const id of S.selected) if (!ids.has(id)) S.selected.delete(id);
      renderPlaylist();
      break;
    }
    case "state":
      S.now = m.now;
      S.nowAt = performance.now();
      if (performance.now() - S.volumeTouched > 1500) {
        S.volume = m.volume;
        $("#volume").value = m.volume;
      }
      S.repeat = m.repeat;
      S.queueSecs = m.queue_secs;
      renderState(m);
      renderPlaylist();
      break;
    case "levels":
      S.levels = m.v;
      break;
    case "notice":
      toast(m.text);
      break;
    case "error":
      toast(m.text, true);
      break;
    case "resync":
      renderPlaylist();
      break;
  }
}

// ---------------------------------------------------------------- main window

function position() {
  if (!S.now) return 0;
  const p = S.now.position_secs + (S.now.paused ? 0 : (performance.now() - S.nowAt) / 1000);
  const d = S.now.track.duration_secs;
  return d ? Math.min(p, d) : p;
}

let marqueeText = "";
function setMarquee(text) {
  if (text === marqueeText) return;
  marqueeText = text;
  const m = $("#marquee");
  const still = text.length < 34;
  m.textContent = still ? text.trim() : text + text;
  m.classList.toggle("still", still);
  m.style.setProperty("--marquee-time", `${Math.max(8, text.length * 0.22)}s`);
}

function renderState(m) {
  const now = S.now;
  const icon = $("#state-icon");
  icon.className = "state-icon " + (!now ? "stopped" : now.paused ? "paused" : "playing");
  $("#time").classList.toggle("blink", !!(now && now.paused));
  if (now) {
    const t = now.track;
    setMarquee(`${t.title} (${fmt(t.duration_secs)})  ***  requested by ${t.requested_by}  ***  `);
  } else {
    setMarquee(S.queue.length ? "STOPPED · PRESS PLAY" : "BREAKCORE MOOSE · ADD SOMETHING FROM THE LIBRARY  ***  ");
  }
  $("#seek").disabled = !now || !now.track.duration_secs;
  $("#vol-label").innerHTML = `<b>${m.volume}</b>%`;
  $("#voice").textContent = m.voice ? `♪ ${m.voice.name || m.voice.id}` : "NOT IN VOICE";
  $("#voice").classList.toggle("lit", !!m.voice);
  const rep = $("#repeat");
  rep.classList.toggle("on", S.repeat !== "off");
  $("#repeat-mode").textContent = { off: "", queue: "ALL", track: "ONE" }[S.repeat];
}

function frame() {
  // Time display and seek bar, from the last state plus the clock.
  const now = S.now;
  const t = $("#time");
  if (now) {
    const p = position(), d = now.track.duration_secs;
    const shown = S.showRemaining && d ? "-" + fmt(d - p) : fmt(p);
    t.textContent = shown.padStart(6, " ");
    if (!S.seeking && d) $("#seek").value = Math.round((p / d) * 1000);
  } else {
    t.textContent = "  :  ";
    if (!S.seeking) $("#seek").value = 0;
  }
  drawVis();
  requestAnimationFrame(frame);
}

// Spectrum bars with falling peak caps.
const vis = { bars: new Array(20).fill(0), peaks: new Array(20).fill(0), hold: new Array(20).fill(0) };
function drawVis() {
  const c = $("#vis"), g = c.getContext("2d");
  const W = c.width, H = c.height, n = vis.bars.length;
  const bw = Math.floor(W / n);
  g.fillStyle = "#050806";
  g.fillRect(0, 0, W, H);
  // Faint grid, like an unlit LCD.
  g.fillStyle = "#0b160e";
  for (let y = H - 2; y > 0; y -= 2) g.fillRect(0, y, W, 1);
  const active = S.now && !S.now.paused;
  for (let i = 0; i < n; i++) {
    const target = active ? (S.levels[i] || 0) / 255 : 0;
    // Rise fast, fall slow.
    vis.bars[i] = target > vis.bars[i] ? target : Math.max(target, vis.bars[i] - 0.035);
    if (vis.bars[i] >= vis.peaks[i]) { vis.peaks[i] = vis.bars[i]; vis.hold[i] = 18; }
    else if (vis.hold[i] > 0) vis.hold[i]--;
    else vis.peaks[i] = Math.max(0, vis.peaks[i] - 0.012);
    const h = Math.round(vis.bars[i] * (H - 2));
    const x = i * bw + 1;
    for (let y = 0; y < h; y += 2) {
      const f = y / H;
      g.fillStyle = f > 0.8 ? "#ff4a2a" : f > 0.55 ? "#ffd23a" : "#33ff66";
      g.fillRect(x, H - 1 - y, bw - 2, 1);
    }
    if (vis.peaks[i] > 0.02) {
      g.fillStyle = "#c8ffd4";
      g.fillRect(x, H - 1 - Math.round(vis.peaks[i] * (H - 2)), bw - 2, 1);
    }
  }
}

$("#time").onclick = () => (S.showRemaining = !S.showRemaining);

document.querySelectorAll("[data-op]").forEach((b) => {
  b.addEventListener("click", () => {
    $("#misc-menu").hidden = true;
    send({ op: b.dataset.op });
  });
});

$("#repeat").onclick = () => {
  const next = { off: "queue", queue: "track", track: "off" }[S.repeat];
  S.repeat = next;
  $("#repeat").classList.toggle("on", next !== "off");
  $("#repeat-mode").textContent = { off: "", queue: "ALL", track: "ONE" }[next];
  send({ op: "repeat", mode: next });
};

const seek = $("#seek");
seek.addEventListener("pointerdown", () => (S.seeking = true));
seek.addEventListener("input", () => {
  S.seeking = true;
  const d = S.now && S.now.track.duration_secs;
  if (d) $("#time").textContent = fmt((seek.value / 1000) * d).padStart(6, " ");
});
seek.addEventListener("change", () => {
  const d = S.now && S.now.track.duration_secs;
  if (d) {
    const secs = Math.min(Math.floor((seek.value / 1000) * d), d - 1);
    send({ op: "seek", secs });
    // Show the new position at once; the next state confirms it.
    S.now.position_secs = secs;
    S.nowAt = performance.now();
  }
  S.seeking = false;
});

let volTimer = null;
$("#volume").addEventListener("input", (e) => {
  S.volumeTouched = performance.now();
  const v = Number(e.target.value);
  $("#vol-label").innerHTML = `<b>${v}</b>%`;
  clearTimeout(volTimer);
  volTimer = setTimeout(() => send({ op: "volume", percent: v }), 120);
});

// ---------------------------------------------------------------- playlist

function renderPlaylist() {
  if (drag && drag.kind === "entry") return; // don't yank rows out from under a drag
  const list = $("#playlist");
  const rows = [];
  if (S.now) {
    const li = el("li", "current");
    li.dataset.current = "1";
    li.append(el("span", "handle", S.now.paused ? "❚❚" : "▶"), el("span", "t", S.now.track.title),
      el("span", "by", S.now.track.requested_by), el("span", "d", fmt(S.now.track.duration_secs)));
    li.title = "Now playing";
    rows.push(li);
  }
  S.queue.forEach((e, i) => {
    const li = el("li", (i < S.priority ? "prio " : "") + (S.selected.has(e.id) ? "selected" : ""));
    li.dataset.id = e.id;
    li.append(el("span", "handle", "≡"), el("span", "n", `${i + 1}.`), el("span", "t", e.track.title),
      el("span", "by", e.track.requested_by), el("span", "d", fmt(e.track.duration_secs)));
    li.title = `${e.track.title}\nadded by ${e.track.requested_by}` + (i < S.priority ? "\n(priority: plays before the rest)" : "");
    rows.push(li);
  });
  if (!rows.length) {
    const li = el("li", "section", "PLAYLIST EMPTY · DRAG SONGS IN FROM THE LIBRARY");
    rows.push(li);
  }
  list.replaceChildren(...rows);
  const total = (S.queueSecs || 0) + (S.now ? Math.max(0, (S.now.track.duration_secs || 0) - position()) : 0);
  $("#pl-total").textContent = `${S.queue.length} / ${fmt(total)}`;
}

function entryIndex(id) {
  return S.queue.findIndex((e) => e.id === id);
}

function select(id, ev) {
  if (ev.shiftKey && S.anchor != null) {
    const a = entryIndex(S.anchor), b = entryIndex(id);
    if (a >= 0 && b >= 0) {
      if (!ev.ctrlKey && !ev.metaKey) S.selected.clear();
      for (let i = Math.min(a, b); i <= Math.max(a, b); i++) S.selected.add(S.queue[i].id);
    }
  } else if (ev.ctrlKey || ev.metaKey) {
    S.selected.has(id) ? S.selected.delete(id) : S.selected.add(id);
    S.anchor = id;
  } else {
    S.selected.clear();
    S.selected.add(id);
    S.anchor = id;
  }
  updateSelection();
}

// Only flips classes, so the rows (and a double-click in progress) stay put.
function updateSelection() {
  $("#playlist").querySelectorAll("li[data-id]").forEach((li) => {
    li.classList.toggle("selected", S.selected.has(Number(li.dataset.id)));
  });
}

function removeSelected() {
  if (!S.selected.size) return;
  const ids = [...S.selected];
  S.queue = S.queue.filter((e) => !S.selected.has(e.id));
  S.selected.clear();
  renderPlaylist();
  send({ op: "remove", ids });
}

$("#playlist").addEventListener("dblclick", (e) => {
  const li = e.target.closest("li[data-id]");
  if (li) send({ op: "play_entry", id: Number(li.dataset.id) });
});

$("#pl-wrap").addEventListener("keydown", (e) => {
  if (e.key === "Delete" || e.key === "Backspace") { e.preventDefault(); removeSelected(); }
  else if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === "a") {
    e.preventDefault();
    S.queue.forEach((q) => S.selected.add(q.id));
    renderPlaylist();
  } else if (e.key === "Enter" && S.selected.size === 1) {
    send({ op: "play_entry", id: [...S.selected][0] });
  } else if (e.key === "Escape") {
    S.selected.clear();
    renderPlaylist();
  }
});

$("#pl-rem").onclick = removeSelected;
$("#pl-sel").onclick = () => { S.queue.forEach((q) => S.selected.add(q.id)); updateSelection(); $("#pl-wrap").focus(); };
$("#pl-misc").onclick = (e) => { e.stopPropagation(); $("#misc-menu").hidden = !$("#misc-menu").hidden; };
document.addEventListener("click", (e) => { if (!e.target.closest(".menu-wrap")) $("#misc-menu").hidden = true; });
$("#misc-none").onclick = () => { S.selected.clear(); updateSelection(); $("#misc-menu").hidden = true; };
$("#misc-clear").onclick = () => {
  $("#misc-menu").hidden = true;
  if (S.queue.length && confirm(`Remove all ${S.queue.length} tracks from the playlist?`)) send({ op: "clear" });
};

$("#pl-add").onclick = () => { $("#add-form").hidden = false; $("#add-input").focus(); };
$("#add-cancel").onclick = () => { $("#add-form").hidden = true; };
$("#add-form").onsubmit = (e) => {
  e.preventDefault();
  const text = $("#add-input").value.trim();
  if (!text) return;
  if (send({ op: "add", items: [{ type: "query", text }] })) toast("Loading…");
  $("#add-input").value = "";
  $("#add-form").hidden = true;
};

// ---------------------------------------------------------------- drag and drop
//
// One pointer-based system for both playlist reordering and dragging search
// results in. Mouse drags start after a few pixels; touch drags start on the
// ≡ handle at once, or anywhere on a row after a long press.

let drag = null; // {kind: "entry"|"result", id|item, label, startX, startY, active, pointerId}

function beginPending(e, info) {
  if (e.button !== undefined && e.button !== 0) return;
  const touch = e.pointerType !== "mouse";
  const onHandle = !!e.target.closest(".handle");
  drag = { ...info, startX: e.clientX, startY: e.clientY, active: false, pointerId: e.pointerId, touch };
  if (touch && onHandle) activate(e);
  else if (touch) drag.timer = setTimeout(() => drag && !drag.active && activate(e), 350);
}

function activate(e) {
  drag.active = true;
  clearTimeout(drag.timer);
  document.body.classList.add("drag-active");
  const g = $("#drag-ghost");
  g.textContent = drag.label;
  g.hidden = false;
  if (drag.row) drag.row.classList.add("dragging");
  if (navigator.vibrate && drag.touch) navigator.vibrate(15);
  moveGhost(e.clientX, e.clientY);
}

function moveGhost(x, y) {
  const g = $("#drag-ghost");
  g.style.left = x + 12 + "px";
  g.style.top = y + 8 + "px";
}

// Where in the playlist a drop at (x, y) lands: `after` is the entry id the
// item goes behind (null = top), `lineY` the y for the indicator. `skip` is
// the row being dragged, which doesn't count as a neighbour.
function dropTarget(x, y, skip) {
  const wrap = $("#pl-wrap");
  const box = wrap.getBoundingClientRect();
  if (x < box.left || x > box.right || y < box.top || y > box.bottom) return null;
  const rows = [...$("#playlist").querySelectorAll("li[data-id]")].filter((r) => r !== skip);
  let after = null, lineY = null;
  const current = $("#playlist").querySelector("li.current");
  lineY = current ? current.offsetTop + current.offsetHeight : 0;
  for (const r of rows) {
    const rb = r.getBoundingClientRect();
    if (y > rb.top + rb.height / 2) {
      after = Number(r.dataset.id);
      lineY = r.offsetTop + r.offsetHeight;
    } else break;
  }
  return { after, lineY };
}

let autoScroll = 0;
function edgeScroll(y) {
  const wrap = $("#pl-wrap"), b = wrap.getBoundingClientRect();
  autoScroll = y < b.top + 24 ? -8 : y > b.bottom - 24 ? 8 : 0;
}
setInterval(() => { if (drag && drag.active && autoScroll) $("#pl-wrap").scrollTop += autoScroll; }, 16);

document.addEventListener("pointermove", (e) => {
  if (!drag || e.pointerId !== drag.pointerId) return;
  if (!drag.active) {
    const far = Math.hypot(e.clientX - drag.startX, e.clientY - drag.startY) > (drag.touch ? 10 : 4);
    if (!far) return;
    if (drag.touch) { clearTimeout(drag.timer); drag = null; return; } // it was a scroll
    activate(e);
  }
  e.preventDefault();
  moveGhost(e.clientX, e.clientY);
  edgeScroll(e.clientY);
  const t = dropTarget(e.clientX, e.clientY, drag.row);
  const line = $("#drop-line");
  if (t) {
    line.hidden = false;
    line.style.top = t.lineY - 1 + "px";
  } else line.hidden = true;
});

// Keep the page from scrolling under an active touch drag.
document.addEventListener("touchmove", (e) => { if (drag && drag.active) e.preventDefault(); }, { passive: false });

function endDrag(e, cancelled) {
  if (!drag || e.pointerId !== drag.pointerId) return;
  const d = drag;
  clearTimeout(d.timer);
  drag = null;
  autoScroll = 0;
  document.body.classList.remove("drag-active");
  $("#drag-ghost").hidden = true;
  $("#drop-line").hidden = true;
  if (d.row) d.row.classList.remove("dragging");
  if (!d.active) return; // a plain click; the click handler selects
  if (cancelled) { renderPlaylist(); return; }
  const t = dropTarget(e.clientX, e.clientY, d.row);
  if (!t) { renderPlaylist(); return; }
  if (d.kind === "entry") {
    if (t.after === d.id) { renderPlaylist(); return; }
    // Move at once; the server's next queue is the truth.
    const from = entryIndex(d.id);
    const [moved] = S.queue.splice(from, 1);
    const to = t.after == null ? 0 : entryIndex(t.after) + 1;
    S.queue.splice(to, 0, moved);
    renderPlaylist();
    send({ op: "move", id: d.id, after: t.after });
  } else {
    renderPlaylist();
    if (send({ op: "add", items: [d.item], after: t.after ?? undefined, top: t.after == null })) {
      toast(d.item.type === "hit" ? "Adding…" : "Loading…");
    }
  }
  suppressClick = true;
  setTimeout(() => (suppressClick = false), 0);
}
let suppressClick = false;
document.addEventListener("pointerup", (e) => endDrag(e, false));
document.addEventListener("pointercancel", (e) => endDrag(e, true));

$("#playlist").addEventListener("pointerdown", (e) => {
  const li = e.target.closest("li[data-id]");
  if (!li) return;
  const id = Number(li.dataset.id);
  const entry = S.queue.find((q) => q.id === id);
  beginPending(e, { kind: "entry", id, row: li, label: entry ? entry.track.title : "" });
});
$("#playlist").addEventListener("click", (e) => {
  if (suppressClick) return;
  const li = e.target.closest("li[data-id]");
  if (li) { select(Number(li.dataset.id), e); $("#pl-wrap").focus({ preventScroll: true }); }
});

// ---------------------------------------------------------------- library

const placeholders = {
  youtube: "Search YouTube Music",
  soundcloud: "Search SoundCloud",
  jellyfin: "Search your Jellyfin library (empty: browse)",
};

$("#tabs").addEventListener("click", (e) => {
  const tab = e.target.closest(".tab");
  if (!tab || tab.disabled) return;
  document.querySelectorAll(".tab").forEach((t) => t.classList.toggle("active", t === tab));
  S.source = tab.dataset.source;
  S.crumb = null;
  $("#crumb").hidden = true;
  $("#kinds").hidden = S.source !== "jellyfin";
  $("#search-input").placeholder = placeholders[S.source];
  $("#results").replaceChildren();
  $("#results-empty").hidden = false;
  if (S.source === "jellyfin") search();
  else if ($("#search-input").value.trim()) search();
});

$("#kinds").addEventListener("click", (e) => {
  const chip = e.target.closest(".chip");
  if (!chip) return;
  document.querySelectorAll(".chip").forEach((c) => c.classList.toggle("active", c === chip));
  S.kind = chip.dataset.kind;
  S.crumb = null;
  $("#crumb").hidden = true;
  search();
});

$("#search-form").onsubmit = (e) => { e.preventDefault(); S.crumb = null; $("#crumb").hidden = true; search(); };

let searchSeq = 0;
async function search() {
  const q = $("#search-input").value.trim();
  if (!q && S.source !== "jellyfin") return;
  const seq = ++searchSeq;
  $("#results-empty").hidden = false;
  $("#results-empty").textContent = "SEARCHING…";
  $("#results").replaceChildren();
  const params = new URLSearchParams({ q });
  if (S.source === "jellyfin") params.set("kind", S.kind);
  try {
    const r = await fetch(`/api/g/${S.guild.id}/search/${S.source}?${params}`);
    if (seq !== searchSeq) return;
    if (!r.ok) throw new Error(await r.text());
    showResults(await r.json());
  } catch (err) {
    if (seq !== searchSeq) return;
    $("#results-empty").textContent = String(err.message || err);
  }
}

async function openContainer(row) {
  const seq = ++searchSeq;
  S.crumb = { id: row.id, name: row.title, kind: row.kind };
  $("#crumb").hidden = false;
  $("#crumb-name").textContent = row.title;
  $("#results").replaceChildren();
  $("#results-empty").hidden = false;
  $("#results-empty").textContent = "LOADING…";
  try {
    const r = await fetch(`/api/g/${S.guild.id}/jellyfin/${row.id}`);
    if (seq !== searchSeq) return;
    if (!r.ok) throw new Error(await r.text());
    const data = await r.json();
    showResults(data.rows);
  } catch (err) {
    if (seq !== searchSeq) return;
    $("#results-empty").textContent = String(err.message || err);
  }
}

$("#crumb-back").onclick = () => { S.crumb = null; $("#crumb").hidden = true; search(); };
$("#crumb-add").onclick = () => S.crumb && addItem({ type: "jellyfin", id: S.crumb.id });
$("#crumb-mix").onclick = () => S.crumb && addItem({ type: "jellyfin", id: S.crumb.id, mix: true });

function itemFor(row) {
  return row.token ? { type: "hit", token: row.token } : { type: "jellyfin", id: row.id };
}

function addItem(item, next = false) {
  if (send({ op: "add", items: [item], next })) toast(item.type === "hit" ? "Adding…" : "Loading…");
}

function showResults(rows) {
  const list = $("#results");
  $("#results-empty").hidden = rows.length > 0;
  $("#results-empty").textContent = "NO RESULTS.";
  list.replaceChildren(...rows.map((row) => {
    const li = el("li");
    li._row = row;
    const img = row.thumbnail ? el("img") : el("span", "noimg");
    if (row.thumbnail) { img.src = row.thumbnail; img.loading = "lazy"; img.alt = ""; img.referrerPolicy = "no-referrer"; }
    const meta = el("div", "meta");
    meta.append(el("span", "t", row.title), el("span", "s", row.subtitle || ""));
    li.append(el("span", "handle", "≡"), img, meta);
    if (row.kind !== "song") li.append(el("span", "kind", row.kind.toUpperCase()));
    if (row.duration_secs) li.append(el("span", "d", fmt(row.duration_secs)));
    const acts = el("div", "acts");
    const button = (label, title, fn) => {
      const b = el("button", "btn small", label);
      b.title = title;
      b.addEventListener("pointerdown", (e) => e.stopPropagation());
      b.onclick = (e) => { e.stopPropagation(); fn(); };
      acts.append(b);
    };
    if (row.kind !== "song") button("▸", "Open", () => openContainer(row));
    button("+", "Add to the end of the playlist", () => addItem(itemFor(row)));
    button("»", "Play next (priority)", () => addItem(itemFor(row), true));
    if (row.id) button("MIX", "Add an instant mix based on this", () => addItem({ type: "jellyfin", id: row.id, mix: true }));
    li.append(acts);
    li.title = row.title + (row.subtitle ? `\n${row.subtitle}` : "") + "\nDrag into the playlist, or double-click to add.";
    return li;
  }));
}

$("#results").addEventListener("pointerdown", (e) => {
  const li = e.target.closest("li");
  if (!li || !li._row) return;
  beginPending(e, { kind: "result", item: itemFor(li._row), label: li._row.title, row: null });
});
$("#results").addEventListener("dblclick", (e) => {
  const li = e.target.closest("li");
  if (!li || !li._row || e.target.closest(".acts")) return;
  if (li._row.kind !== "song") openContainer(li._row);
  else addItem(itemFor(li._row));
});

// Space toggles play/pause when not typing.
document.addEventListener("keydown", (e) => {
  if (e.target.closest("input, textarea")) return;
  if (e.code === "Space" && !$("#app").hidden) {
    e.preventDefault();
    send({ op: S.now && !S.now.paused ? "pause" : "play" });
  }
});

boot();
