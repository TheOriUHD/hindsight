const { invoke, convertFileSrc } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const $ = (s) => document.querySelector(s);
const el = (tag, cls, text) => {
  const e = document.createElement(tag);
  if (cls) e.className = cls;
  if (text != null) e.textContent = text;
  return e;
};

let settings = null;
let status = null;
let monitors = [];
let clips = [];
let gameFilter = null;

/* ---------- formatting ---------- */
const fmtTime = (s, tenths = true) => {
  if (!isFinite(s) || s < 0) s = 0;
  const m = Math.floor(s / 60);
  const sec = s - m * 60;
  return tenths ? `${m}:${sec.toFixed(1).padStart(4, "0")}` : `${m}:${String(Math.floor(sec)).padStart(2, "0")}`;
};
const fmtSize = (b) => (b >= 1e9 ? `${(b / 1e9).toFixed(1)} GB` : `${Math.max(1, Math.round(b / 1e6))} MB`);
const fmtClock = (unix) => new Date(unix * 1000).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
function dayLabel(unix) {
  const d = new Date(unix * 1000);
  const today = new Date();
  const yesterday = new Date(Date.now() - 864e5);
  if (d.toDateString() === today.toDateString()) return "Today";
  if (d.toDateString() === yesterday.toDateString()) return "Yesterday";
  const sameYear = d.getFullYear() === today.getFullYear();
  return d.toLocaleDateString([], { weekday: "long", month: "long", day: "numeric", ...(sameYear ? {} : { year: "numeric" }) });
}
const codecName = (enc) => {
  if (!enc) return "–";
  const [codec, vendor] = enc.split("_");
  const c = { av1: "AV1", hevc: "HEVC", h264: "H.264" }[codec] || codec;
  const v = { nvenc: "NVENC", amf: "AMF", mf: "Media Foundation", qsv: "Quick Sync" }[vendor] || vendor;
  return `${c} (${v})`;
};
const icon = (name) => `<svg class="ic"><use href="#i-${name}"/></svg>`;

/* ---------- toast ---------- */
let toastTimer;
function toast(text, { error = false, progress = null, sticky = false } = {}) {
  const t = $("#toast");
  $("#toast-text").textContent = text;
  t.hidden = false;
  t.classList.toggle("error", error);
  t.classList.toggle("progress", progress != null);
  if (progress != null) $("#toast-bar").style.width = `${Math.round(progress * 100)}%`;
  clearTimeout(toastTimer);
  if (!sticky) toastTimer = setTimeout(() => (t.hidden = true), error ? 6000 : 2400);
}

/* ---------- navigation ---------- */
let view = "clips";
function show(name) {
  if (view === "player" && name !== "player") closePlayer();
  view = name;
  document.querySelectorAll(".view").forEach((v) => v.classList.toggle("active", v.id === `view-${name}`));
  document.querySelectorAll(".nav-item").forEach((b) => b.classList.toggle("active", b.dataset.view === (name === "player" ? "clips" : name)));
}
document.querySelectorAll(".nav-item").forEach((b) =>
  b.addEventListener("click", () => {
    if (b.dataset.view === "clips") {
      gameFilter = null;
      renderClips();
    }
    show(b.dataset.view);
  })
);

/* ---------- recorder status (sidebar) ---------- */
let lastClip;
function setMeta(rows) {
  const dl = $("#rec-meta");
  dl.replaceChildren();
  for (const [k, v] of rows) dl.append(el("dt", null, k), el("dd", null, v));
}
async function refreshStatus() {
  const rec = $("#rec");
  try {
    status = await invoke("get_status");
  } catch {
    rec.className = "rec err";
    $("#rec-state").textContent = "Recorder not running";
    setMeta([["", "Start Hindsight from the Start menu"]]);
    $("#save-clip").disabled = true;
    $("#toggle-rec").disabled = true;
    return;
  }
  // The recorder is a separate process: hotkey saves and tray pause/resume
  // show up here as status changes.
  if (lastClip !== undefined && status.last_clip && status.last_clip !== lastClip) loadClips(status.last_clip);
  lastClip = status.last_clip;
  if (settings) settings.enabled = status.enabled;

  $("#toggle-rec").disabled = false;
  $("#toggle-rec").textContent = status.enabled ? "Pause recording" : "Resume recording";
  $("#save-clip").disabled = !status.recording;
  rec.className = "rec" + (status.recording ? " on" : "") + (status.error ? " err" : "");
  if (status.error) {
    $("#rec-state").textContent = "Not recording";
    setMeta([]);
    $("#rec-meta").append(el("p", "rec-msg", status.error));
  } else if (!status.enabled) {
    $("#rec-state").textContent = "Paused";
    setMeta([]);
    $("#rec-meta").append(el("p", "rec-msg", "The hotkey does nothing until you resume."));
  } else if (!status.recording) {
    $("#rec-state").textContent = "Starting";
    setMeta([]);
  } else {
    $("#rec-state").textContent = "Recording";
    setMeta([
      ["Video", `${status.height}p · ${status.fps} fps`],
      ["Encoder", codecName(status.encoder)],
      ["Audio", status.audio_tracks.length ? status.audio_tracks.join(", ") : "Off"],
      ["Buffer", `${Math.floor(status.buffered_secs)} s · ${Math.round(status.buffer_mb)} MB`],
    ]);
  }
}
setInterval(() => !document.hidden && refreshStatus(), 1000);

$("#save-clip").addEventListener("click", async () => {
  const b = $("#save-clip");
  b.disabled = true;
  try {
    await invoke("save_clip");
  } catch (e) {
    toast(`Couldn't save clip: ${e}`, { error: true });
  }
  b.disabled = false;
});

$("#toggle-rec").addEventListener("click", async () => {
  try {
    await invoke("set_enabled", { enabled: !status?.enabled });
  } catch (e) {
    toast(String(e), { error: true });
  }
  refreshStatus();
});

/* ---------- library ---------- */
async function loadClips(highlight) {
  try {
    clips = await invoke("list_clips");
  } catch (e) {
    toast(`Couldn't read the clips folder: ${e}`, { error: true });
    clips = [];
  }
  renderClips(highlight);
}

function renderGames() {
  const counts = new Map();
  for (const c of clips) counts.set(c.game, (counts.get(c.game) || 0) + 1);
  if (gameFilter && !counts.has(gameFilter)) gameFilter = null;
  const box = $("#games");
  box.replaceChildren();
  if (counts.size < 2) return;
  for (const [game, n] of [...counts].sort((a, b) => b[1] - a[1])) {
    const b = el("button", "game" + (gameFilter === game ? " active" : ""));
    b.append(el("span", null, game), el("span", "count", n));
    b.title = game;
    b.onclick = () => {
      gameFilter = gameFilter === game ? null : game;
      renderClips();
      show("clips");
    };
    box.append(b);
  }
}

function renderClips(highlight) {
  renderGames();
  const shown = clips.filter((c) => !gameFilter || c.game === gameFilter);
  $("#nav-count").textContent = clips.length || "";
  $("#clips-title").textContent = gameFilter || "All clips";
  const total = shown.reduce((a, c) => a + c.size, 0);
  $("#clip-count").textContent = shown.length ? `${shown.length} clip${shown.length === 1 ? "" : "s"} · ${fmtSize(total)}` : "";
  $("#empty").hidden = clips.length > 0;

  const days = $("#days");
  days.replaceChildren();
  let grid = null;
  let lastDay = null;
  for (const c of shown) {
    const label = dayLabel(c.created);
    if (label !== lastDay) {
      const day = el("section", "day");
      day.append(el("h2", null, label));
      grid = el("div", "grid");
      day.append(grid);
      days.append(day);
      lastDay = label;
    }
    grid.append(card(c, c.path === highlight));
  }
}

function card(c, isNew) {
  const d = el("div", "clip" + (isNew ? " new" : ""));
  d.dataset.path = c.path;
  const th = el("div", "thumb");
  const img = el("img");
  img.loading = "lazy";
  img.alt = "";
  if (c.thumb) img.src = convertFileSrc(c.thumb);
  else img.style.visibility = "hidden";
  th.append(img, el("span", "dur", fmtTime(c.duration, false)));
  d.append(th, el("div", "name", gameFilter ? fmtClock(c.created) : c.game), el("div", "meta", `${gameFilter ? "" : fmtClock(c.created) + " · "}${fmtSize(c.size)}`));
  d.title = c.name;
  d.onclick = () => openPlayer(c);
  hoverScrub(th, c);
  return d;
}

/* Hovering a thumbnail scrubs through the clip using its scrub proxy. */
function hoverScrub(th, c) {
  let v = null;
  let bar = null;
  th.addEventListener("mouseenter", async () => {
    const p = await invoke("scrub_proxy", { path: c.path, generate: false });
    if (!p || !th.matches(":hover")) return;
    v = el("video");
    v.muted = true;
    v.preload = "auto";
    v.src = convertFileSrc(p);
    bar = el("div", "hoverbar");
    th.append(v, bar);
  });
  th.addEventListener("mousemove", (e) => {
    if (!v || !v.duration) return;
    const r = th.getBoundingClientRect();
    const f = Math.min(Math.max((e.clientX - r.left) / r.width, 0), 1);
    bar.style.width = `${f * 100}%`;
    seekLatest(v, f * (v.duration - 0.05));
  });
  th.addEventListener("mouseleave", () => {
    v?.remove();
    bar?.remove();
    v = bar = null;
  });
}

listen("thumb", (e) => {
  const [clip, thumb] = e.payload;
  const c = clips.find((x) => x.path === clip);
  if (c) c.thumb = thumb;
  const img = document.querySelector(`.clip[data-path="${CSS.escape(clip)}"] img`);
  if (img) {
    img.src = convertFileSrc(thumb);
    img.style.visibility = "";
  }
});

$("#open-folder").onclick = () => invoke("open_clips_folder");

/* ---------- player ---------- */
const video = $("#video");
const proxy = $("#scrub");
let cur = null;
let trim = { start: 0, end: 0 };
let busy = false;

function setPlayIcon() {
  $("#p-play").innerHTML = icon(video.paused ? "play" : "pause");
}

function openPlayer(c) {
  cur = c;
  $("#p-title").value = c.name;
  $("#p-meta").textContent = `${dayLabel(c.created)}, ${fmtClock(c.created)} · ${fmtSize(c.size)}`;
  $("#video-error").hidden = true;
  video.src = convertFileSrc(c.path);
  trim = { start: 0, end: c.duration || 0 };
  show("player");
  video.play().catch(() => {});
  drawTimeline();
  clearStrip();
  loadProxy(c.path);
}

function closePlayer() {
  if (!cur) return;
  video.pause();
  video.removeAttribute("src");
  video.load();
  unloadProxy();
  cur = null;
}

video.addEventListener("loadedmetadata", () => {
  if (cur && (!trim.end || Math.abs(trim.end - (cur.duration || 0)) < 0.01)) trim.end = video.duration;
  drawTimeline();
});
video.addEventListener("error", () => cur && video.getAttribute("src") && ($("#video-error").hidden = false));
video.addEventListener("play", setPlayIcon);
video.addEventListener("pause", setPlayIcon);

const duration = () => (isFinite(video.duration) && video.duration > 0 ? video.duration : cur?.duration || 0);

function drawTimeline() {
  const d = duration() || 1;
  const pct = (t) => `${(t / d) * 100}%`;
  $("#shade-l").style.width = pct(trim.start);
  $("#shade-r").style.width = pct(d - trim.end);
  $("#sel").style.left = pct(trim.start);
  $("#sel").style.width = pct(trim.end - trim.start);
  $("#h-start").style.left = pct(trim.start);
  $("#h-end").style.left = pct(trim.end);
  const now = scrubbing ? scrubT : video._want ?? video.currentTime ?? 0;
  $("#playhead").style.left = pct(now);
  $("#p-time").textContent = fmtTime(now);
  $("#p-dur").textContent = fmtTime(d);
  const full = trim.start < 0.05 && trim.end > d - 0.05;
  $("#p-range").textContent = full ? "Drag the red handles to trim" : `Selection ${fmtTime(trim.start)} – ${fmtTime(trim.end)} · ${fmtTime(trim.end - trim.start)}`;
  if (typeof updateExportHint === "function") updateExportHint();
}

(function tick() {
  if (cur && !video.paused) {
    // Loop inside the selection.
    if (video.currentTime >= trim.end - 0.02 || video.currentTime < trim.start - 0.25) video.currentTime = trim.start;
    drawTimeline();
  }
  requestAnimationFrame(tick);
})();

const timeAt = (clientX) => {
  const r = $("#track").getBoundingClientRect();
  return Math.min(Math.max((clientX - r.left) / r.width, 0), 1) * duration();
};

/* ---------- scrubbing ----------
 * While dragging, frames come from a 360p all-keyframe proxy (every seek is a
 * single-frame decode, so it keeps up with the mouse). On release the full
 * quality video seeks once and replaces it. Without a proxy, the main video
 * is seeked "latest wins": one seek in flight, always to the newest position. */
let proxyReady = false;
let scrubbing = false;
let scrubT = 0;
let resumeAfterScrub = false;

function loadProxy(path) {
  unloadProxy();
  invoke("scrub_proxy", { path, generate: true }).then((p) => p && cur && cur.path === path && setProxy(p));
}
function setProxy(p) {
  const src = convertFileSrc(p) + `?v=${Date.now()}`;
  proxy.src = src;
  proxy.addEventListener("loadeddata", () => (proxyReady = true), { once: true });
  buildStrip();
}
function unloadProxy() {
  proxyReady = false;
  proxy.classList.remove("on");
  proxy.removeAttribute("src");
  proxy.load();
}
listen("scrub-ready", (e) => {
  const [clip, p] = e.payload;
  if (cur && cur.path === clip) setProxy(p);
});

function seekLatest(v, t) {
  v._want = t;
  if (v._seeking) return;
  v._seeking = true;
  v.currentTime = t;
}
function trackSeeks(v) {
  v.addEventListener("seeked", () => {
    v._seeking = false;
    if (v._want != null && Math.abs(v._want - v.currentTime) > 0.001) seekLatest(v, v._want);
    else v._want = null;
  });
}
trackSeeks(video);
trackSeeks(proxy);
// Hover-scrub videos are created on the fly; track their seeks too.
new MutationObserver((ms) => ms.forEach((m) => m.addedNodes.forEach((n) => n.tagName === "VIDEO" && n.closest(".thumb") && trackSeeks(n)))).observe($("#days"), { childList: true, subtree: true });

function scrubTo(t) {
  scrubT = Math.min(Math.max(t, 0), duration());
  if (proxyReady) {
    proxy.classList.add("on");
    seekLatest(proxy, Math.min(scrubT, proxy.duration || scrubT));
  } else {
    seekLatest(video, scrubT);
  }
  drawTimeline();
}
function beginScrub() {
  scrubbing = true;
  resumeAfterScrub = !video.paused;
  video.pause();
}
function endScrub() {
  scrubbing = false;
  seekLatest(video, scrubT);
  // Swap back to full quality once the real frame is decoded.
  const done = () => {
    if (scrubbing) return;
    if (video._seeking) return video.addEventListener("seeked", done, { once: true });
    proxy.classList.remove("on");
    if (resumeAfterScrub) video.play().catch(() => {});
  };
  video.addEventListener("seeked", done, { once: true });
  drawTimeline();
}
function drag(onMove) {
  return (e) => {
    e.stopPropagation();
    beginScrub();
    onMove(e);
    const move = (ev) => onMove(ev);
    const up = () => {
      window.removeEventListener("pointermove", move);
      window.removeEventListener("pointerup", up);
      endScrub();
    };
    window.addEventListener("pointermove", move);
    window.addEventListener("pointerup", up);
  };
}
$("#h-start").addEventListener("pointerdown", drag((ev) => {
  trim.start = Math.min(timeAt(ev.clientX), trim.end - 0.2);
  scrubTo(trim.start);
}));
$("#h-end").addEventListener("pointerdown", drag((ev) => {
  trim.end = Math.max(timeAt(ev.clientX), trim.start + 0.2);
  scrubTo(trim.end);
}));
$("#track").addEventListener("pointerdown", drag((ev) => scrubTo(timeAt(ev.clientX))));

/* Filmstrip behind the timeline: one tiled image (32 frames) cut from the
 * scrub proxy by ffmpeg; as many tiles as fit the timeline are shown. */
const STRIP_TILES = 32;
let stripToken = 0;
let stripSrc = null;
function clearStrip() {
  stripToken++;
  stripSrc = null;
  $("#strip").replaceChildren();
}
function layoutStrip() {
  const box = $("#strip");
  box.replaceChildren();
  if (!stripSrc) return;
  const r = box.getBoundingClientRect();
  const tileW = (r.height * 16) / 9;
  const n = Math.max(1, Math.round(r.width / tileW));
  for (let i = 0; i < n; i++) {
    const t = el("i");
    const idx = n === 1 ? 0 : Math.round((i * (STRIP_TILES - 1)) / (n - 1));
    // Sprite is STRIP_TILES tiles wide; scale it so one tile fills this cell.
    t.style.backgroundImage = `url("${stripSrc}")`;
    t.style.backgroundSize = `${STRIP_TILES * 100}% 100%`;
    t.style.backgroundPosition = `${(idx / (STRIP_TILES - 1)) * 100}% 0`;
    box.append(t);
  }
}
async function buildStrip() {
  const token = ++stripToken;
  const path = cur?.path;
  if (!path) return;
  const p = await invoke("filmstrip", { path });
  if (!p || token !== stripToken) return;
  stripSrc = convertFileSrc(p) + `?v=${Date.now()}`;
  layoutStrip();
}
// Re-lay out the tiles when the timeline's width changes.
let stripTimer;
new ResizeObserver(() => {
  clearTimeout(stripTimer);
  stripTimer = setTimeout(layoutStrip, 100);
}).observe($("#track"));

/* ---------- player actions ---------- */
$("#p-play").onclick = () => (video.paused ? video.play() : video.pause());
$("#p-back").onclick = () => show("clips");
$("#p-external").onclick = $("#p-external2").onclick = () => cur && invoke("open_external", { path: cur.path });
$("#p-folder").onclick = () => cur && invoke("show_in_folder", { path: cur.path });

$("#p-delete").onclick = async () => {
  if (!cur || !confirm(`Move "${cur.name}" to the Recycle Bin?`)) return;
  const path = cur.path;
  show("clips");
  try {
    await invoke("delete_clip", { path });
    toast("Moved to the Recycle Bin");
  } catch (e) {
    toast(`Couldn't delete: ${e}`, { error: true });
  }
  loadClips();
};

async function commitRename() {
  if (!cur) return;
  const name = $("#p-title").value.trim();
  if (!name || name === cur.name) {
    $("#p-title").value = cur.name;
    return;
  }
  // The file can't be renamed while the video element holds it open.
  const t = video.currentTime;
  video.removeAttribute("src");
  video.load();
  unloadProxy();
  try {
    const path = await invoke("rename_clip", { path: cur.path, name });
    cur.path = path;
    cur.name = name;
    loadClips();
  } catch (e) {
    toast(`Couldn't rename: ${e}`, { error: true });
    $("#p-title").value = cur.name;
  }
  video.src = convertFileSrc(cur.path);
  video.currentTime = t;
  loadProxy(cur.path);
}
$("#p-title").addEventListener("keydown", (e) => {
  if (e.key === "Enter") e.target.blur();
  if (e.key === "Escape") {
    e.target.value = cur?.name ?? "";
    e.target.blur();
  }
  e.stopPropagation();
});
$("#p-title").addEventListener("blur", commitRename);

async function runJob(label, cmd, args) {
  if (busy || !cur) return;
  busy = true;
  document.querySelectorAll("#p-trim, #p-export").forEach((b) => (b.disabled = true));
  toast(`${label}…`, { progress: 0, sticky: true });
  try {
    const out = await invoke(cmd, args);
    toast(`${label} saved`);
    loadClips(out);
  } catch (e) {
    toast(`${label} failed: ${e}`, { error: true });
  }
  busy = false;
  document.querySelectorAll("#p-trim, #p-export").forEach((b) => (b.disabled = false));
}
listen("progress", (e) => {
  if (busy) toast(`${e.payload.job === "export" ? "Exporting" : "Trimming"}… ${Math.round(e.payload.pct * 100)}%`, { progress: e.payload.pct, sticky: true });
});

$("#p-trim").onclick = () => runJob("Selection", "trim_clip", { path: cur.path, start: trim.start, end: trim.end, exact: $("#p-exact").checked });
/* Export presets. Limits as of October 2026: Discord free 20 MB (raised from
 * 10 MB in August 2026), Nitro Basic 50 MB, Nitro 1 GB; WhatsApp 16 MB for
 * regular video; email 25 MB, of which encoding leaves about 18 MB for the
 * file; X 512 MB and 2:20 for free accounts. */
const PRESETS = {
  discord: { mb: 20, file: "Discord" },
  "nitro-basic": { mb: 50, file: "Nitro Basic" },
  nitro: { mb: 1000, file: "Nitro" },
  whatsapp: { mb: 16, file: "WhatsApp" },
  email: { mb: 18, file: "Email" },
  x: { mb: 500, file: "X", maxDur: 140 },
  hq: { mb: null, file: "Export" },
  custom: { mb: null, file: null },
};

function presetTarget() {
  const id = $("#p-preset").value;
  const p = PRESETS[id];
  if (id === "custom") {
    const mb = Math.max(1, Number($("#p-custom-mb").value) || 1);
    return { mb, file: `${mb} MB`, maxDur: null };
  }
  return p;
}

// Mirrors clips::export: the resolution / frame rate the bitrate can carry.
function exportPlan() {
  const p = presetTarget();
  const dur = Math.max(0.1, trim.end - trim.start);
  const srcH = video.videoHeight || 1080;
  const srcFps = Math.min(status?.fps || 60, 60);
  if (p.mb == null) return { text: `${Math.min(srcH, 2160)}p ${srcFps} fps, high quality` };
  const av1 = $("#p-codec").value === "av1";
  const total = (p.mb * 8000 * 0.95) / dur;
  const audio = total < 1500 ? 64 : total < 6000 ? 96 : 160;
  const vid = Math.max(100, total - audio);
  const minBpp = av1 ? 0.045 : 0.075;
  const ladder = [[2160, 60], [1440, 60], [1080, 60], [1080, 30], [720, 60], [720, 30], [540, 30], [480, 30], [360, 30]];
  let pick = [Math.min(360, srcH), Math.min(30, srcFps)];
  for (const [h0, f0] of ladder) {
    const h = Math.min(h0, srcH), f = Math.min(f0, srcFps);
    if ((vid * 1000) / ((h * 16) / 9 * h * f) >= minBpp) { pick = [h, f]; break; }
  }
  const warn = p.maxDur && dur > p.maxDur ? `Over ${fmtTime(p.maxDur, false)}: trim it first` : null;
  return { text: warn || `${pick[0]}p ${pick[1]} fps`, warn: !!warn, tiny: pick[0] < 540 };
}

function updateExportHint() {
  if (!cur) return;
  $("#p-custom-mb").hidden = $("#p-preset").value !== "custom";
  const plan = exportPlan();
  const hint = $("#p-export-hint");
  hint.textContent = plan.text;
  hint.className = "sub" + (plan.warn ? " warn" : "");
  hint.title = plan.tiny ? "The selection is long for this size limit. A shorter selection looks sharper." : "";
  $("#p-export").disabled = busy || !!plan.warn;
}
for (const id of ["#p-preset", "#p-codec", "#p-custom-mb"]) {
  $(id).addEventListener("input", () => {
    try {
      localStorage.setItem("hs-export", JSON.stringify({ preset: $("#p-preset").value, codec: $("#p-codec").value, mb: $("#p-custom-mb").value }));
    } catch {}
    updateExportHint();
  });
}
try {
  const saved = JSON.parse(localStorage.getItem("hs-export") || "null");
  if (saved) {
    $("#p-preset").value = saved.preset in PRESETS ? saved.preset : "discord";
    $("#p-codec").value = saved.codec === "av1" ? "av1" : "h264";
    $("#p-custom-mb").value = saved.mb || 100;
  }
} catch {}
video.addEventListener("loadedmetadata", updateExportHint);

$("#p-export").onclick = () => {
  const p = presetTarget();
  runJob("Export", "export_clip", {
    path: cur.path,
    start: trim.start,
    end: trim.end,
    targetMb: p.mb,
    codec: $("#p-codec").value,
    label: p.file,
  });
};

document.addEventListener("keydown", (e) => {
  if (view !== "player" || !cur || ["INPUT", "SELECT"].includes(e.target.tagName)) return;
  const d = duration();
  const at = video._want ?? video.currentTime;
  switch (e.key) {
    case " ":
      e.preventDefault();
      video.paused ? video.play() : video.pause();
      break;
    case "Escape":
      show("clips");
      break;
    case "i":
    case "I":
      trim.start = Math.min(at, trim.end - 0.2);
      drawTimeline();
      break;
    case "o":
    case "O":
      trim.end = Math.max(at, trim.start + 0.2);
      drawTimeline();
      break;
    case "ArrowLeft":
      seekLatest(video, Math.max(0, at - (e.shiftKey ? 5 : 1)));
      drawTimeline();
      break;
    case "ArrowRight":
      seekLatest(video, Math.min(d, at + (e.shiftKey ? 5 : 1)));
      drawTimeline();
      break;
  }
});

/* ---------- settings ---------- */
const form = $("#settings");
const FIELDS = ["monitor", "max_height", "fps", "quality", "codec", "buffer_secs", "save_sound", "save_overlay", "desktop_audio", "microphone", "start_with_windows", "desktop_mode", "output_device", "input_device", "separate_tracks"];
const NUMERIC = ["fps", "max_height", "buffer_secs"];
const QUALITY = {
  balanced: "Smaller files. Fine for most games.",
  high: "Hard to tell apart from the original.",
  max: "Highest bitrate. Large files.",
};

function fillSettings() {
  for (const name of FIELDS) {
    const field = form.elements[name];
    const v = settings[name];
    if (!field) continue;
    if (field.type === "checkbox") field.checked = !!v;
    else field.value = v == null ? "" : String(v);
  }
  $("#hotkey").textContent = settings.hotkey;
  $("#save-key").textContent = settings.hotkey;
  $("#empty-key").textContent = settings.hotkey;
  $("#empty-secs").textContent = settings.buffer_secs;
  $("#out-dir").textContent = settings.output_dir;
  $("#out-dir").title = settings.output_dir;
  $("#quality-desc").textContent = QUALITY[settings.quality] || "";
  memHint();
  fillAudio();
}

/* ---------- audio sources ---------- */
let outputDevices = [];
let inputDevices = [];

function fillDeviceSelect(sel, devices, current, label) {
  sel.replaceChildren(el("option", null, label));
  sel.firstChild.value = "";
  for (const d of devices) {
    const o = el("option", null, d.name || "Unnamed device");
    o.value = d.id;
    sel.append(o);
  }
  // A saved device that's unplugged right now still shows, so it isn't lost.
  if (current && !devices.some((d) => d.id === current)) {
    const o = el("option", null, "Saved device (not connected)");
    o.value = current;
    sel.append(o);
  }
  sel.value = current || "";
}

function fillAudio() {
  const desk = !!settings.desktop_audio;
  const apps = settings.desktop_mode === "apps";
  const show = {
    desktop: desk,
    "desktop-all": desk && !apps,
    "desktop-apps": desk && apps,
    mic: !!settings.microphone,
    multi: (desk ? (apps ? Math.max(1, settings.audio_apps.length) : 1) : 0) + (settings.microphone ? 1 : 0) > 1,
  };
  document.querySelectorAll("[data-show]").forEach((r) => (r.hidden = !show[r.dataset.show]));
  $("#mode-desc").textContent = apps ? "Just the apps below, e.g. the game without Discord or music." : "Everything you hear on the output device.";
  fillDeviceSelect($("#output-device"), outputDevices, settings.output_device, "Windows default");
  fillDeviceSelect($("#input-device"), inputDevices, settings.input_device, "Windows default");

  const list = $("#app-list");
  list.replaceChildren();
  if (!settings.audio_apps.length) list.append(el("span", "app-empty", "No apps yet. Until you add one, everything is recorded."));
  for (const a of settings.audio_apps) {
    const chip = el("span", "app-chip");
    chip.title = a.exe;
    const x = el("button", null, "✕");
    x.type = "button";
    x.title = `Stop recording ${a.name}`;
    x.onclick = () => pushSettings({ ...settings, audio_apps: settings.audio_apps.filter((b) => b.exe !== a.exe) });
    chip.append(el("span", null, a.name), x);
    list.append(chip);
  }
}

async function loadAudioDevices() {
  [outputDevices, inputDevices] = await Promise.all([
    invoke("audio_devices", { output: true }).catch(() => []),
    invoke("audio_devices", { output: false }).catch(() => []),
  ]);
  if (settings) fillAudio();
}

// The app picker lists whatever is playing audio right now; refreshed on open.
async function refreshAudioApps() {
  const sel = $("#add-app");
  const apps = await invoke("audio_apps").catch(() => []);
  sel.replaceChildren(el("option", null, apps.length ? "Add an app…" : "Nothing is playing audio right now"));
  sel.firstChild.value = "";
  for (const a of apps) {
    if (settings.audio_apps.some((b) => b.exe.toLowerCase() === a.exe.toLowerCase())) continue;
    const o = el("option", null, a.name === a.exe.replace(/\.exe$/i, "") ? a.name : `${a.name} (${a.exe})`);
    o.value = JSON.stringify(a);
    sel.append(o);
  }
}
$("#add-app").addEventListener("pointerdown", refreshAudioApps);
$("#add-app").addEventListener("focus", refreshAudioApps);
$("#add-app").addEventListener("change", (e) => {
  e.stopPropagation();
  const v = e.target.value;
  if (!v) return;
  const app = JSON.parse(v);
  e.target.value = "";
  pushSettings({ ...settings, audio_apps: [...settings.audio_apps, app] });
});

function memHint() {
  // Mirrors Settings::max_kbps: the most the replay buffer can hold in RAM.
  const mon = monitors.find((m) => m.id === settings.monitor) || monitors.find((m) => m.primary) || monitors[0];
  let w = 1920, h = 1080;
  const m = mon?.label.match(/(\d+)×(\d+)/);
  if (m) [w, h] = [Number(m[1]), Number(m[2])];
  if (settings.max_height && h > settings.max_height) {
    w = Math.round((w * settings.max_height) / h);
    h = settings.max_height;
  }
  const bpp = { balanced: 0.06, high: 0.1, max: 0.16 }[settings.quality];
  const kbps = Math.min(Math.max((w * h * settings.fps * bpp) / 1000, 8000), 300000);
  const mb = (kbps * settings.buffer_secs) / 8 / 1000;
  $("#mem-hint").textContent = `Kept in memory, at most ${mb < 1000 ? Math.round(mb) + " MB" : (mb / 1000).toFixed(1) + " GB"} with these settings.`;
}

let savedTimer;
async function pushSettings(next) {
  const prev = settings;
  settings = next;
  fillSettings();
  const s = $("#saved");
  try {
    await invoke("set_settings", { settings: next });
    s.textContent = "Saved";
    s.className = "sub ok";
  } catch (e) {
    settings = prev;
    fillSettings();
    s.textContent = `Couldn't apply: ${e}`;
    s.className = "sub error";
  }
  clearTimeout(savedTimer);
  savedTimer = setTimeout(() => {
    s.textContent = "Changes apply immediately";
    s.className = "sub";
  }, 2500);
  refreshStatus();
}

form.addEventListener("change", (e) => {
  const i = e.target;
  if (!i.name) return;
  const next = { ...settings };
  if (i.type === "checkbox") next[i.name] = i.checked;
  else if (NUMERIC.includes(i.name)) next[i.name] = Number(i.value);
  else if (["monitor", "output_device", "input_device"].includes(i.name)) next[i.name] = i.value || null;
  else if (i.id === "add-app") return;
  else next[i.name] = i.value;
  pushSettings(next);
});

$("#pick-dir").onclick = async () => {
  const dir = await invoke("pick_folder");
  if (dir) {
    await pushSettings({ ...settings, output_dir: dir });
    loadClips();
  }
};
$("#open-logs").onclick = () => invoke("open_logs");

/* Hotkey capture: keyboard combos or mouse side buttons. */
const MODS = ["Control", "Alt", "Shift", "Meta"];
function keyName(e) {
  const c = e.code;
  if (c.startsWith("Key")) return c.slice(3);
  if (c.startsWith("Digit")) return c.slice(5);
  if (/^Numpad\d$/.test(c)) return "Num" + c.slice(6);
  return c;
}
function combo(e, key) {
  const parts = [];
  if (e.ctrlKey) parts.push("Ctrl");
  if (e.altKey) parts.push("Alt");
  if (e.shiftKey) parts.push("Shift");
  if (e.metaKey) parts.push("Win");
  parts.push(key);
  return parts.join("+");
}
$("#hotkey").addEventListener("click", () => {
  const b = $("#hotkey");
  b.classList.add("listening");
  b.textContent = "Press a key…";
  const done = (value) => {
    window.removeEventListener("keydown", onKey, true);
    window.removeEventListener("mousedown", onMouse, true);
    b.classList.remove("listening");
    if (value && value !== settings.hotkey) pushSettings({ ...settings, hotkey: value });
    else b.textContent = settings.hotkey;
  };
  const onKey = (e) => {
    e.preventDefault();
    e.stopPropagation();
    if (e.key === "Escape") return done(null);
    if (MODS.includes(e.key)) return;
    done(combo(e, keyName(e)));
  };
  const onMouse = (e) => {
    if (e.button === 3 || e.button === 4) {
      e.preventDefault();
      e.stopPropagation();
      done(combo(e, e.button === 3 ? "Mouse4" : "Mouse5"));
    }
  };
  window.addEventListener("keydown", onKey, true);
  window.addEventListener("mousedown", onMouse, true);
});

/* ---------- boot ---------- */
(async function boot() {
  setPlayIcon();
  invoke("app_version").then((v) => ($("#version").textContent = v));
  try {
    settings = await invoke("get_settings");
  } catch {
    settings = null;
  }
  monitors = await invoke("monitors");
  const sel = $("#monitor");
  sel.replaceChildren(el("option", null, "Primary display"));
  sel.firstChild.value = "";
  for (const m of monitors) {
    const o = el("option", null, m.label);
    o.value = m.id;
    sel.append(o);
  }
  if (settings) fillSettings();
  refreshStatus();
  loadClips();
  loadAudioDevices();
})();

// Mouse back/forward buttons would otherwise navigate the webview.
window.addEventListener("mouseup", (e) => (e.button === 3 || e.button === 4) && e.preventDefault());
