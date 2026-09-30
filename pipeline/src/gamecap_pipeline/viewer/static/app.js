"use strict";
// gamecap viewer: session list + segment-by-segment playback with a per-frame
// input overlay. Frame k of a segment is shown at video time k / rate_hz.

const $ = (id) => document.getElementById(id);
const video = $("video");
const overlay = $("overlay");
const spark = $("spark");

const st = {
  sources: [], source: null, sessions: [], session: null, segs: [], segIdx: 0,
  tl: null, tlCache: new Map(), frame: -1, loadToken: 0, modePref: "auto", h264Fallback: false,
};

const HEVC_OK = !!video.canPlayType('video/mp4; codecs="hvc1.1.6.L93.B0"');
$("codecNote").textContent = HEVC_OK ? "HEVC playback supported" : "no HEVC in this browser: using H.264";

function store(k, v) { try { v === undefined ? localStorage.removeItem(k) : localStorage.setItem(k, v); } catch (_) {} }
function load(k) { try { return localStorage.getItem(k); } catch (_) { return null; } }

async function api(path) {
  const r = await fetch(path);
  const body = await r.json().catch(() => ({}));
  if (!r.ok) throw new Error(body.error || `${r.status} ${r.statusText}`);
  return body;
}
const q = (o) => new URLSearchParams(o).toString();
const fmtBytes = (b) => b > 1e9 ? (b / 1e9).toFixed(2) + " GB" : b > 1e6 ? (b / 1e6).toFixed(1) + " MB" : (b / 1e3).toFixed(0) + " KB";
function fmtDur(s) {
  s = Math.round(s); const h = Math.floor(s / 3600), m = Math.floor(s / 60) % 60, x = s % 60;
  return h ? `${h}:${String(m).padStart(2, "0")}:${String(x).padStart(2, "0")}` : `${m}:${String(x).padStart(2, "0")}`;
}
function fmtStart(iso) {
  if (!iso) return "";
  const d = new Date(iso);
  return d.toLocaleString(undefined, { year: "numeric", month: "short", day: "numeric", hour: "2-digit", minute: "2-digit" });
}
function esc(s) { return String(s).replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[c]); }

// ---------------------------------------------------------------- sources / sessions
async function init() {
  const hash = parseHash();
  st.modePref = (hash.m && ["auto", "raw", "remux", "h264"].includes(hash.m) ? hash.m : null) || load("gv.mode") || "auto";
  $("modeSel").value = st.modePref;
  try {
    st.sources = await api("/api/sources");
  } catch (e) {
    $("sessionList").innerHTML = `<div class="listMsg">Can't reach the server: ${esc(e.message)}</div>`;
    return;
  }
  const sel = $("sourceSel");
  sel.innerHTML = st.sources.map((s) => `<option value="${esc(s.name)}">${esc(s.name)} (${esc(s.kind)})</option>`).join("");
  const want = hash.s || load("gv.source");
  if (want && st.sources.some((s) => s.name === want)) sel.value = want;
  await selectSource(sel.value);
  if (hash.id && hash.s === st.source) openSession(hash.id, { seg: +hash.seg || 0, frame: +hash.f || 0 });
}

async function selectSource(name, refresh = false) {
  st.source = name;
  store("gv.source", name);
  const list = $("sessionList");
  list.innerHTML = `<div class="listMsg">Loading sessions…</div>`;
  try {
    st.sessions = await api("/api/sessions?" + q({ source: name, ...(refresh ? { refresh: 1 } : {}) }));
  } catch (e) {
    list.innerHTML = `<div class="listMsg">Error: ${esc(e.message)}</div>`;
    return;
  }
  renderSessions();
}

function renderSessions() {
  const f = $("filter").value.trim().toLowerCase();
  const rows = st.sessions.filter((s) => !f || `${s.game_id} ${s.user} ${s.session}`.toLowerCase().includes(f));
  const list = $("sessionList");
  if (!rows.length) {
    list.innerHTML = `<div class="listMsg">${st.sessions.length ? "No match." : "No sessions in this source."}</div>`;
    return;
  }
  list.innerHTML = rows.map((s) => `
    <div class="sess${st.session && st.session.id === s.id ? " active" : ""}" data-id="${esc(s.id)}">
      <div><span class="g">${esc(s.game_id || "?")}</span>${s.user ? `<span class="u">${esc(s.user)}</span>` : ""}</div>
      <div class="m">${esc(fmtStart(s.start) || s.session)}</div>
      <div class="m">${fmtDur(s.duration_s)} · ${s.segments} seg · ${s.frames} frames${s.dropped ? ` · ${s.dropped} dropped` : ""} · ${fmtBytes(s.bytes)}</div>
    </div>`).join("");
}

$("sessionList").addEventListener("click", (e) => {
  const el = e.target.closest(".sess");
  if (!el) return;
  document.body.classList.remove("showSidebar");
  openSession(el.dataset.id, { seg: 0, frame: 0 });
});
$("filter").addEventListener("input", renderSessions);
$("sourceSel").addEventListener("change", (e) => selectSource(e.target.value));
$("refreshBtn").addEventListener("click", () => selectSource(st.source, true));
$("menuBtn").addEventListener("click", () => document.body.classList.toggle("showSidebar"));
$("modeSel").addEventListener("change", (e) => {
  st.modePref = e.target.value; st.h264Fallback = false;
  store("gv.mode", st.modePref);
  if (st.session) loadSeg(st.segIdx, { frame: Math.max(0, st.frame), autoplay: !video.paused });
});

async function openSession(id, { seg = 0, frame = 0 } = {}) {
  let s;
  try {
    s = await api("/api/session?" + q({ source: st.source, id }));
  } catch (e) {
    $("empty").hidden = false; $("player").hidden = true;
    $("empty").textContent = `Could not open ${id}: ${e.message}`;
    return;
  }
  st.session = s; st.segs = s.segment_list; st.tlCache.clear();
  $("empty").hidden = true; $("player").hidden = false;
  $("sessTitle").textContent = `${s.game_id || "?"}${s.user ? " · " + s.user : ""}`;
  $("sessMeta").textContent = `${s.session} · ${fmtStart(s.start)} · ${fmtDur(s.duration_s)} · ${s.segments} segments · ${s.width}×${s.height} @ ${s.rate_hz} Hz`;
  renderSessions();
  buildSegStrip();
  loadSeg(Math.min(seg, st.segs.length - 1), { frame, autoplay: false });
}

// ---------------------------------------------------------------- segments / video
// auto: HEVC-capable browsers get "remux" (faststart copy: real duration, instant
// seeking); raw fragmented MP4 has no index, so seeking re-reads the file.
function modeFor() {
  if (st.modePref !== "auto") return st.modePref;
  const ff = st.sources.length && st.sources[0].ffmpeg;
  if (!ff) return "raw";
  return HEVC_OK && !st.h264Fallback ? "remux" : "h264";
}
const videoUrl = (i, mode) => "/video?" + q({ source: st.source, id: st.session.id, seg: st.segs[i].name, mode });

function timeline(i) {
  const seg = st.segs[i];
  if (!st.tlCache.has(seg.name)) {
    const p = api("/api/timeline?" + q({ source: st.source, id: st.session.id, seg: seg.name }));
    p.catch(() => st.tlCache.delete(seg.name));
    st.tlCache.set(seg.name, p);
  }
  return st.tlCache.get(seg.name);
}

async function loadSeg(i, { frame = 0, autoplay = false } = {}) {
  if (i < 0 || i >= st.segs.length) return;
  const token = ++st.loadToken;
  st.segIdx = i; st.frame = -1;
  showMsg("Loading…");
  let tl;
  try {
    tl = await timeline(i);
  } catch (e) {
    if (token === st.loadToken) showMsg(`Timeline failed: ${e.message}`);
    return;
  }
  if (token !== st.loadToken) return;
  st.tl = tl;
  buildSpark();
  updateSegStrip();
  renderInfo();
  const mode = modeFor();
  video.dataset.mode = mode;
  if (mode === "h264" || mode === "remux") showMsg(`Preparing ${mode} video (first view of a segment converts it on the server)…`);
  video.src = videoUrl(i, mode);
  video.playbackRate = +$("speedSel").value;
  video.addEventListener("loadeddata", function once() {
    video.removeEventListener("loadeddata", once);
    if (token !== st.loadToken) return;
    hideMsg();
    if (frame > 0) seekFrame(frame); else draw(0);
    if (autoplay) video.play().catch(() => {});
  });
  if (i + 1 < st.segs.length) {
    timeline(i + 1).catch(() => {});
    // warm the server's remux/transcode cache for the next segment
    if (mode !== "raw") fetch(videoUrl(i + 1, mode), { method: "HEAD" }).catch(() => {});
  }
  setHash();
}

video.addEventListener("error", () => {
  const mode = video.dataset.mode;
  if (st.modePref === "auto" && !st.h264Fallback && mode !== "h264") {
    st.h264Fallback = true;
    if (modeFor() === mode) { showMsg(`Video failed (${mode}).`); return; }
    $("codecNote").textContent = "HEVC failed to play here: using H.264";
    loadSeg(st.segIdx, { frame: Math.max(0, st.frame), autoplay: true });
    return;
  }
  const err = video.error ? `code ${video.error.code} ${video.error.message || ""}` : "";
  showMsg(`Video failed (${mode}) ${err}. Try another Video mode in the header.`);
});
video.addEventListener("ended", () => {
  if (st.segIdx + 1 < st.segs.length) loadSeg(st.segIdx + 1, { autoplay: true });
});
video.addEventListener("play", () => { $("playBtn").innerHTML = "&#10074;&#10074;"; });
video.addEventListener("pause", () => { $("playBtn").innerHTML = "&#9654;"; setHash(); });
video.addEventListener("seeked", () => draw(frameAt(video.currentTime)));
video.addEventListener("timeupdate", () => { if (!HAS_RVFC) draw(frameAt(video.currentTime)); });

function showMsg(t) { const m = $("videoMsg"); m.textContent = t; m.hidden = false; }
function hideMsg() { $("videoMsg").hidden = true; }

const rate = () => (st.tl ? st.tl.rate_hz : 20);
function frameAt(t) {
  if (!st.tl) return 0;
  return Math.max(0, Math.min(st.tl.n - 1, Math.floor(t * rate() + 1e-3)));
}
function seekFrame(k) {
  if (!st.tl) return;
  k = Math.max(0, Math.min(st.tl.n - 1, k));
  video.currentTime = (k + 0.5) / rate();
  draw(k);
}
function step(d) {
  video.pause();
  const k = st.frame + d;
  if (k < 0 && st.segIdx > 0) {
    const prev = st.segs[st.segIdx - 1].manifest.frame_count;
    return loadSeg(st.segIdx - 1, { frame: prev - 1 });
  }
  if (st.tl && k >= st.tl.n && st.segIdx + 1 < st.segs.length) return loadSeg(st.segIdx + 1, { frame: 0 });
  seekFrame(k);
}

// Per-frame sync: requestVideoFrameCallback where available, else rAF polling.
const HAS_RVFC = "requestVideoFrameCallback" in HTMLVideoElement.prototype;
if (HAS_RVFC) {
  const cb = (_now, meta) => { draw(frameAt(meta.mediaTime)); video.requestVideoFrameCallback(cb); };
  video.requestVideoFrameCallback(cb);
} else {
  const loop = () => { if (!video.paused) draw(frameAt(video.currentTime)); requestAnimationFrame(loop); };
  requestAnimationFrame(loop);
}

$("playBtn").addEventListener("click", () => (video.paused ? video.play().catch(() => {}) : video.pause()));
$("stepBack").addEventListener("click", () => step(-1));
$("stepFwd").addEventListener("click", () => step(1));
$("prevSeg").addEventListener("click", () => loadSeg(st.segIdx - 1));
$("nextSeg").addEventListener("click", () => loadSeg(st.segIdx + 1));
$("speedSel").addEventListener("change", (e) => { video.playbackRate = +e.target.value; });

document.addEventListener("keydown", (e) => {
  if (!st.session || e.ctrlKey || e.metaKey || e.altKey) return;
  const tag = (e.target.tagName || "").toLowerCase();
  if (tag === "input" || tag === "select" || tag === "textarea") return;
  const k = e.key;
  if (k === " ") { e.preventDefault(); video.paused ? video.play().catch(() => {}) : video.pause(); }
  else if (k === ",") step(-1);
  else if (k === ".") step(1);
  else if (k === "[") loadSeg(st.segIdx - 1);
  else if (k === "]") loadSeg(st.segIdx + 1);
  else if (k === "ArrowLeft") { e.preventDefault(); seekFrame(st.frame - rate()); }
  else if (k === "ArrowRight") { e.preventDefault(); seekFrame(st.frame + rate()); }
});

// ---------------------------------------------------------------- hash state
function parseHash() {
  const p = new URLSearchParams(location.hash.slice(1));
  return { s: p.get("s"), id: p.get("id"), seg: p.get("seg"), f: p.get("f"), m: p.get("m") };
}
function setHash() {
  if (!st.session) return;
  const h = q({ s: st.source, id: st.session.id, seg: st.segIdx, f: Math.max(0, st.frame) });
  history.replaceState(null, "", "#" + h);
}

// ---------------------------------------------------------------- segment strip + sparkline
function buildSegStrip() {
  const el = $("segStrip");
  el.innerHTML = st.segs.map((s, i) => {
    const m = s.manifest;
    return `<div data-i="${i}" style="flex:${Math.max(1, m.frame_count + (m.dropped_frames || 0))}" title="${esc(s.name)}: ${m.frame_count} frames">${i}<span class="ph" hidden></span></div>`;
  }).join("");
}
$("segStrip").addEventListener("click", (e) => {
  const el = e.target.closest("[data-i]");
  if (!el) return;
  const i = +el.dataset.i;
  const r = el.getBoundingClientRect();
  const frac = (e.clientX - r.left) / r.width;
  const n = st.segs[i].manifest.frame_count;
  if (i === st.segIdx) seekFrame(Math.floor(frac * n));
  else loadSeg(i, { frame: Math.floor(frac * n), autoplay: !video.paused });
});
function updateSegStrip() {
  const kids = $("segStrip").children;
  for (let i = 0; i < kids.length; i++) {
    kids[i].classList.toggle("cur", i === st.segIdx);
    kids[i].querySelector(".ph").hidden = i !== st.segIdx;
  }
}

let sparkBase = null;
function buildSpark() {
  const tl = st.tl;
  const dpr = window.devicePixelRatio || 1;
  const w = Math.max(100, Math.round(spark.clientWidth * dpr)), h = Math.round(56 * dpr);
  spark.width = w; spark.height = h;
  const c = document.createElement("canvas"); c.width = w; c.height = h;
  const g = c.getContext("2d");
  g.fillStyle = "#181b21"; g.fillRect(0, 0, w, h);
  const n = tl.n;
  const maxEv = Math.max(1, ...tl.ev);
  for (let x = 0; x < w; x++) {
    const a = Math.floor((x / w) * n), b = Math.max(a + 1, Math.floor(((x + 1) / w) * n));
    let ev = 0, unf = false, held = 0, mv = 0;
    for (let k = a; k < b && k < n; k++) {
      ev = Math.max(ev, tl.ev[k]);
      if (!tl.focused[k]) unf = true;
      held = Math.max(held, tl.keys[k].length + tl.mb[k].length + tl.pad[k].length);
      mv = Math.max(mv, Math.hypot(tl.dx[k], tl.dy[k]));
    }
    if (unf) { g.fillStyle = "rgba(242,179,61,.28)"; g.fillRect(x, 0, 1, h); }
    const eh = (Math.log1p(ev) / Math.log1p(maxEv)) * (h - 10 * dpr);
    g.fillStyle = "#4fb3ff"; g.fillRect(x, h - 6 * dpr - eh, 1, eh);
    if (held) { g.fillStyle = `rgba(229,72,61,${Math.min(1, 0.35 + held * 0.2)})`; g.fillRect(x, h - 5 * dpr, 1, 5 * dpr); }
    if (mv > 0) { g.fillStyle = "rgba(255,255,255,.18)"; g.fillRect(x, 0, 1, 3 * dpr); }
  }
  sparkBase = c;
  drawSparkHead();
}
function drawSparkHead() {
  if (!sparkBase || !st.tl) return;
  const g = spark.getContext("2d");
  g.drawImage(sparkBase, 0, 0);
  const x = ((Math.max(0, st.frame) + 0.5) / st.tl.n) * spark.width;
  g.fillStyle = "#fff"; g.fillRect(Math.round(x) - 1, 0, 2, spark.height);
}
spark.addEventListener("click", (e) => {
  if (!st.tl) return;
  const r = spark.getBoundingClientRect();
  seekFrame(Math.floor(((e.clientX - r.left) / r.width) * st.tl.n));
});
let resizeT = 0;
window.addEventListener("resize", () => {
  clearTimeout(resizeT);
  resizeT = setTimeout(() => { if (st.tl) { buildSpark(); sizeOverlay(); const f = st.frame; st.frame = -1; draw(f); } }, 150);
});

// ---------------------------------------------------------------- overlay
const OW = 1100, OH = 236, U = 27; // logical canvas size, key unit
const KB_X = 12, KB_Y = 14;
const KEYS = [];
function row(y, items) { for (const [label, code, x, w = 1] of items) KEYS.push({ label, code, x, y, w }); }
row(0, [["Esc", 0x01, 0], ["F1", 0x3b, 2], ["F2", 0x3c, 3], ["F3", 0x3d, 4], ["F4", 0x3e, 5], ["F5", 0x3f, 6.5], ["F6", 0x40, 7.5],
  ["F7", 0x41, 8.5], ["F8", 0x42, 9.5], ["F9", 0x43, 11], ["F10", 0x44, 12], ["F11", 0x57, 13], ["F12", 0x58, 14],
  ["Prt", 0xe037, 15.25], ["Scr", 0x46, 16.25], ["Brk", 0xe11d, 17.25]]);
row(1.25, [["`", 0x29, 0], ...[..."1234567890"].map((c, i) => [c, 0x02 + i, 1 + i]), ["-", 0x0c, 11], ["=", 0x0d, 12], ["Bksp", 0x0e, 13, 2],
  ["Ins", 0xe052, 15.25], ["Hm", 0xe047, 16.25], ["PgU", 0xe049, 17.25]]);
row(2.25, [["Tab", 0x0f, 0, 1.5], ...[..."QWERTYUIOP"].map((c, i) => [c, 0x10 + i, 1.5 + i]), ["[", 0x1a, 11.5], ["]", 0x1b, 12.5], ["\\", 0x2b, 13.5, 1.5],
  ["Del", 0xe053, 15.25], ["End", 0xe04f, 16.25], ["PgD", 0xe051, 17.25]]);
row(3.25, [["Caps", 0x3a, 0, 1.75], ...[..."ASDFGHJKL"].map((c, i) => [c, 0x1e + i, 1.75 + i]), [";", 0x27, 10.75], ["'", 0x28, 11.75], ["Enter", 0x1c, 12.75, 2.25]]);
row(4.25, [["Shift", 0x2a, 0, 2.25], ...[..."ZXCVBNM"].map((c, i) => [c, 0x2c + i, 2.25 + i]), [",", 0x33, 9.25], [".", 0x34, 10.25], ["/", 0x35, 11.25],
  ["Shift", 0x36, 12.25, 2.75], ["↑", 0xe048, 16.25]]);
row(5.25, [["Ctrl", 0x1d, 0, 1.25], ["Win", 0xe05b, 1.25, 1.25], ["Alt", 0x38, 2.5, 1.25], ["Space", 0x39, 3.75, 6.25], ["Alt", 0xe038, 10, 1.25],
  ["Win", 0xe05c, 11.25, 1.25], ["Menu", 0xe05d, 12.5, 1.25], ["Ctrl", 0xe01d, 13.75, 1.25], ["←", 0xe04b, 15.25], ["↓", 0xe050, 16.25], ["→", 0xe04d, 17.25]]);
const LAYOUT_CODES = new Set(KEYS.map((k) => k.code));
const PAD_NAMES = ["A", "B", "Y", "X", "C", "Z", "LB", "LT", "RB", "RT", "Select", "Start", "Mode", "LS", "RS", "Up", "Down", "Left", "Right"];
const MB_NAMES = ["LMB", "RMB", "MMB", "M4", "M5"];

function sizeOverlay() {
  const dpr = window.devicePixelRatio || 1;
  const cssW = Math.max(300, overlay.clientWidth || OW);
  const cssH = (cssW * OH) / OW;
  overlay.style.height = cssH + "px";
  overlay.width = Math.round(cssW * dpr);
  overlay.height = Math.round(cssH * dpr);
}
sizeOverlay();

function rr(g, x, y, w, h, r) {
  g.beginPath(); g.moveTo(x + r, y); g.arcTo(x + w, y, x + w, y + h, r); g.arcTo(x + w, y + h, x, y + h, r);
  g.arcTo(x, y + h, x, y, r); g.arcTo(x, y, x + w, y, r); g.closePath();
}
const C = { key: "#2a2f39", keyLine: "#3a404c", on: "#e5483d", txt: "#aab0bd", txtOn: "#fff", dim: "#5b6270", blue: "#4fb3ff" };

function draw(k) {
  if (!st.tl || k === st.frame) return;
  st.frame = k;
  const tl = st.tl;
  if (overlay.width === 0 || Math.abs(overlay.width - overlay.clientWidth * (window.devicePixelRatio || 1)) > 2) sizeOverlay();
  const g = overlay.getContext("2d");
  const s = overlay.width / OW;
  g.setTransform(s, 0, 0, s, 0, 0);
  g.clearRect(0, 0, OW, OH);
  g.textAlign = "center"; g.textBaseline = "middle";

  const held = new Set(tl.keys[k]);
  const pressedNow = new Set(tl.pressed[k]);
  // keyboard
  for (const key of KEYS) {
    const x = KB_X + key.x * U, y = KB_Y + key.y * U, w = key.w * U - 3, h = U - 3;
    const on = held.has(key.code);
    rr(g, x, y, w, h, 4);
    g.fillStyle = on ? C.on : C.key; g.fill();
    g.strokeStyle = on && pressedNow.has(tl.key_names[key.code]) ? "#fff" : C.keyLine; g.lineWidth = 1; g.stroke();
    g.fillStyle = on ? C.txtOn : C.txt;
    g.font = `${key.label.length > 3 ? 10 : 12}px system-ui, sans-serif`;
    g.fillText(key.label, x + w / 2, y + h / 2 + 0.5);
  }
  const extra = [...held].filter((c) => !LAYOUT_CODES.has(c)).map((c) => tl.key_names[c] || "0x" + c.toString(16));
  if (extra.length) {
    g.textAlign = "left"; g.fillStyle = C.on; g.font = "12px system-ui, sans-serif";
    g.fillText("also held: " + extra.join(" "), KB_X, KB_Y + 6.6 * U);
    g.textAlign = "center";
  }

  // mouse body
  const mx = 530, my = 18, mw = 76, mh = 120;
  const mb = new Set(tl.mb[k]);
  rr(g, mx, my, mw, mh, 34); g.fillStyle = C.key; g.fill(); g.strokeStyle = C.keyLine; g.stroke();
  g.save(); rr(g, mx, my, mw, mh, 34); g.clip();
  if (mb.has(0)) { g.fillStyle = C.on; g.fillRect(mx, my, mw / 2 - 1, 50); }
  if (mb.has(1)) { g.fillStyle = C.on; g.fillRect(mx + mw / 2 + 1, my, mw / 2, 50); }
  g.restore();
  g.strokeStyle = C.keyLine; g.beginPath(); g.moveTo(mx + mw / 2, my); g.lineTo(mx + mw / 2, my + 50); g.moveTo(mx, my + 50); g.lineTo(mx + mw, my + 50); g.stroke();
  rr(g, mx + mw / 2 - 6, my + 12, 12, 24, 5); g.fillStyle = mb.has(2) ? C.on : "#3a404c"; g.fill();
  for (const [b, yy] of [[4, 64], [3, 88]]) { rr(g, mx - 9, my + yy, 7, 18, 3); g.fillStyle = mb.has(b) ? C.on : C.key; g.fill(); g.strokeStyle = C.keyLine; g.stroke(); }
  const wh = tl.wheel[k];
  if (wh) {
    g.fillStyle = C.blue; g.beginPath();
    const cx = mx + mw / 2, ty = wh > 0 ? my - 2 : my + mh + 12;
    if (wh > 0) { g.moveTo(cx - 8, ty + 6); g.lineTo(cx + 8, ty + 6); g.lineTo(cx, ty - 6); }
    else { g.moveTo(cx - 8, ty - 6); g.lineTo(cx + 8, ty - 6); g.lineTo(cx, ty + 6); }
    g.fill();
    g.font = "11px system-ui"; g.fillText(`wheel ${wh > 0 ? "+" : ""}${wh}`, cx, my + mh + 26);
  }
  g.fillStyle = C.dim; g.font = "11px system-ui"; g.fillText("mouse", mx + mw / 2, my + mh + (wh ? 40 : 18));

  // mouse motion
  const cx = 700, cy = 80, R = 58;
  g.strokeStyle = C.keyLine; g.lineWidth = 1.5; g.beginPath(); g.arc(cx, cy, R, 0, Math.PI * 2); g.stroke();
  g.beginPath(); g.moveTo(cx - R, cy); g.lineTo(cx + R, cy); g.moveTo(cx, cy - R); g.lineTo(cx, cy + R); g.strokeStyle = "#252a33"; g.stroke();
  // faint trail of the previous frames' motion
  for (let j = 1; j <= 6 && k - j >= 0; j++) {
    const [ex, ey] = arrowEnd(tl.dx[k - j], tl.dy[k - j], R);
    if (ex === 0 && ey === 0) continue;
    g.strokeStyle = `rgba(79,179,255,${0.22 - j * 0.03})`; g.lineWidth = 3; g.beginPath(); g.moveTo(cx, cy); g.lineTo(cx + ex, cy + ey); g.stroke();
  }
  const [ex, ey] = arrowEnd(tl.dx[k], tl.dy[k], R);
  if (ex || ey) {
    g.strokeStyle = C.blue; g.lineWidth = 4; g.beginPath(); g.moveTo(cx, cy); g.lineTo(cx + ex, cy + ey); g.stroke();
    g.fillStyle = C.blue; g.beginPath(); g.arc(cx + ex, cy + ey, 5, 0, Math.PI * 2); g.fill();
  }
  g.fillStyle = C.txt; g.font = "12px ui-monospace, Consolas, monospace";
  g.fillText(`dx ${fmtSigned(tl.dx[k])}  dy ${fmtSigned(tl.dy[k])}`, cx, cy + R + 16);
  g.fillStyle = C.dim; g.font = "11px system-ui"; g.fillText("mouse motion / frame", cx, cy + R + 32);

  // gamepad
  drawPad(g, tl, k);
  drawSparkHead();
  renderText(k);
  const strip = $("segStrip").children[st.segIdx];
  if (strip) { const ph = strip.querySelector(".ph"); ph.style.left = ((k + 0.5) / tl.n) * 100 + "%"; }
}

function fmtSigned(v) { const r = Math.round(v); return (r > 0 ? "+" : r < 0 ? "" : " ") + String(r).padStart(4, " "); }
function arrowEnd(dx, dy, R) {
  const m = Math.hypot(dx, dy);
  if (!m) return [0, 0];
  const L = R * Math.min(1, Math.log1p(m) / Math.log1p(400));
  return [(dx / m) * L, (dy / m) * L];
}

function drawPad(g, tl, k) {
  const x0 = 800;
  const on = new Set(tl.pad[k]);
  const ax = tl.axes ? tl.axes[k] : [0, 0, 0, 0, 0, 0, 0, 0];
  const alpha = tl.has_gamepad ? 1 : 0.35;
  g.save(); g.globalAlpha = alpha;
  const bar = (x, label, v) => {
    rr(g, x, 14, 70, 12, 4); g.fillStyle = C.key; g.fill(); g.strokeStyle = C.keyLine; g.stroke();
    if (v > 0.01) { rr(g, x, 14, 70 * Math.min(1, v), 12, 4); g.fillStyle = C.on; g.fill(); }
    g.fillStyle = C.txt; g.font = "10px system-ui"; g.fillText(label, x + 35, 20.5);
  };
  bar(x0 + 20, "LT", Math.max(ax[2] || 0, on.has(7) ? 1 : 0));
  bar(x0 + 200, "RT", Math.max(ax[5] || 0, on.has(9) ? 1 : 0));
  const btnRect = (x, y, w, h, b, label) => {
    rr(g, x, y, w, h, 5); g.fillStyle = on.has(b) ? C.on : C.key; g.fill(); g.strokeStyle = C.keyLine; g.stroke();
    g.fillStyle = on.has(b) ? C.txtOn : C.txt; g.font = "10px system-ui"; g.fillText(label, x + w / 2, y + h / 2 + 0.5);
  };
  btnRect(x0 + 20, 32, 70, 16, 6, "LB");
  btnRect(x0 + 200, 32, 70, 16, 8, "RB");
  const stick = (x, y, sx, sy, b, label) => {
    g.beginPath(); g.arc(x, y, 28, 0, Math.PI * 2); g.fillStyle = C.key; g.fill();
    g.strokeStyle = on.has(b) ? C.on : C.keyLine; g.lineWidth = on.has(b) ? 3 : 1; g.stroke(); g.lineWidth = 1;
    const px = x + sx * 22, py = y - sy * 22;
    g.beginPath(); g.arc(px, py, 9, 0, Math.PI * 2); g.fillStyle = Math.hypot(sx, sy) > 0.15 ? C.blue : "#4a5160"; g.fill();
    g.fillStyle = C.dim; g.font = "10px system-ui"; g.fillText(label, x, y + 40);
  };
  stick(x0 + 45, 90, ax[0] || 0, ax[1] || 0, 13, "L stick");
  stick(x0 + 185, 160, ax[3] || 0, ax[4] || 0, 14, "R stick");
  // d-pad (buttons and/or dpad axes)
  const dx = ax[6] || 0, dy = ax[7] || 0;
  const dcx = x0 + 105, dcy = 160;
  const dpad = [[15, 0, -1, dy > 0.5], [16, 0, 1, dy < -0.5], [17, -1, 0, dx < -0.5], [18, 1, 0, dx > 0.5]];
  for (const [b, ox, oy, axisOn] of dpad) {
    const lit = on.has(b) || axisOn;
    rr(g, dcx + ox * 15 - 7, dcy + oy * 15 - 7, 14, 14, 3); g.fillStyle = lit ? C.on : C.key; g.fill(); g.strokeStyle = C.keyLine; g.stroke();
  }
  // face buttons
  const fcx = x0 + 250, fcy = 90;
  const face = [[2, 0, -1, "Y", "#e8c547"], [1, 1, 0, "B", "#e5483d"], [0, 0, 1, "A", "#57c77a"], [3, -1, 0, "X", "#4fb3ff"]];
  for (const [b, ox, oy, label, col] of face) {
    const x = fcx + ox * 20, y = fcy + oy * 20;
    g.beginPath(); g.arc(x, y, 10, 0, Math.PI * 2); g.fillStyle = on.has(b) ? col : C.key; g.fill(); g.strokeStyle = C.keyLine; g.stroke();
    g.fillStyle = on.has(b) ? "#111" : C.txt; g.font = "bold 10px system-ui"; g.fillText(label, x, y + 0.5);
  }
  // select / mode / start
  for (const [b, x, y, label] of [[10, x0 + 118, 88, "Sel"], [12, x0 + 145, 70, "●"], [11, x0 + 172, 88, "Start"]]) {
    g.beginPath(); g.arc(x, y, 8, 0, Math.PI * 2); g.fillStyle = on.has(b) ? C.on : C.key; g.fill(); g.strokeStyle = C.keyLine; g.stroke();
    g.fillStyle = C.dim; g.font = "9px system-ui"; if (label !== "●") g.fillText(label, x, y + 16);
  }
  g.restore();
  g.fillStyle = C.dim; g.font = "11px system-ui";
  g.fillText(tl.has_gamepad ? "gamepad" : "gamepad (no input in this segment)", x0 + 145, 222);
}

function renderText(k) {
  const tl = st.tl;
  const segStart = st.segs.slice(0, st.segIdx).reduce((a, s) => a + s.manifest.frame_count + (s.manifest.dropped_frames || 0), 0);
  const t = k / tl.rate_hz, ts = (segStart + k) / tl.rate_hz;
  $("frameInfo").textContent = `seg ${st.segIdx + 1}/${st.segs.length} · frame ${k}/${tl.n - 1} · ${t.toFixed(2)} s · session ${fmtDur(ts)}`;
  const heldNames = tl.keys[k].map((c) => tl.key_names[c] || "0x" + c.toString(16));
  const mbNames = tl.mb[k].map((b) => MB_NAMES[b] || "M" + b);
  const padNames = tl.pad[k].map((b) => "Pad " + (PAD_NAMES[b] || b));
  const parts = [
    `held: <b>${esc([...heldNames, ...mbNames, ...padNames].join(" ") || "-")}</b>`,
    `pressed: <b>${esc(tl.pressed[k].join(" ") || "-")}</b>`,
    `events: ${tl.ev[k]}`,
  ];
  $("textState").innerHTML = parts.join(" &nbsp;·&nbsp; ");
  const badges = [];
  if (!tl.focused[k]) badges.push(`<span class="badge warn">UNFOCUSED · inputs gated</span>`);
  if (tl.repeated[k]) badges.push(`<span class="badge dim">repeat frame</span>`);
  badges.push(`<span class="badge dim">${esc(video.dataset.mode || "")}</span>`);
  $("badges").innerHTML = badges.join("");
}

function renderInfo() {
  const seg = st.segs[st.segIdx];
  const m = seg.manifest;
  const ep = m.encoder_params || {};
  const rows = [
    ["segment", `${seg.name} (${st.segIdx + 1} of ${st.segs.length})`],
    ["session", m.session_id], ["game", m.game_id], ["frames", `${m.frame_count} (+${m.dropped_frames} dropped, ${((100 * m.dropped_frames) / Math.max(1, m.frame_count + m.dropped_frames)).toFixed(2)}%)`],
    ["repeated", `${st.tl.repeated.reduce((a, b) => a + b, 0)} frames`],
    ["inputs", `${st.tl.events} events`],
    ["video", `${m.width}×${m.height} @ ${m.rate_hz} Hz, ${m.encoder}${ep.qp ? " qp " + ep.qp : ""}${ep.scale_path ? ", " + ep.scale_path : ""}`],
    ["latency offset", `${(m.latency_offset_ns / 1e6).toFixed(1)} ms`],
    ["gpu", m.gpu], ["os", m.os], ["client", m.client_version], ["ffmpeg", ep.ffmpeg_version || ""],
    ["files", Object.entries(seg.files).map(([k, v]) => `${k} ${fmtBytes(v)}`).join(", ")],
  ];
  $("info").innerHTML = rows.map(([k, v]) => `<div class="k">${esc(k)}</div><div class="v">${esc(v ?? "")}</div>`).join("");
}

init();
