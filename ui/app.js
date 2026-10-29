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
let clips = [];
let gameFilter = null;

/* ---------- formatting ---------- */
const fmtTime = (s, tenths = true) => {
  if (!isFinite(s)) s = 0;
  const m = Math.floor(s / 60);
  const sec = s - m * 60;
  return tenths ? `${m}:${sec.toFixed(1).padStart(4, "0")}` : `${m}:${String(Math.floor(sec)).padStart(2, "0")}`;
};
const fmtSize = (b) => (b > 1e9 ? `${(b / 1e9).toFixed(2)} GB` : `${(b / 1e6).toFixed(1)} MB`);
const fmtDate = (unix) => {
  const d = new Date(unix * 1000);
  const today = new Date();
  const sameDay = d.toDateString() === today.toDateString();
  const time = d.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
  return sameDay ? `Today ${time}` : `${d.toLocaleDateString([], { month: "short", day: "numeric" })} ${time}`;
};
const codecName = (enc) => {
  if (!enc) return "";
  const [codec, vendor] = enc.split("_");
  const c = { av1: "AV1", hevc: "HEVC", h264: "H.264" }[codec] || codec;
  const v = { nvenc: "NVENC", amf: "AMF", mf: "MF", qsv: "QSV" }[vendor] || vendor;
  return `${c} · ${v}`;
};

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
  if (!sticky) toastTimer = setTimeout(() => (t.hidden = true), error ? 6000 : 2600);
}

/* ---------- tabs ---------- */
document.querySelectorAll(".tab").forEach((b) =>
  b.addEventListener("click", () => {
    document.querySelectorAll(".tab").forEach((x) => x.classList.toggle("active", x === b));
    document.querySelectorAll(".view").forEach((v) => v.classList.toggle("active", v.id === `view-${b.dataset.view}`));
  })
);

/* ---------- status ---------- */
let lastClip;
async function refreshStatus() {
  try {
    status = await invoke("get_status");
  } catch (e) {
    $("#status").className = "status err";
    $("#status-text").textContent = "Recorder not running — start Hindsight from the Start menu";
    $("#save-clip").disabled = true;
    return;
  }
  // The recorder runs in another process: new clips (hotkey) and pause/resume
  // (tray) show up as status changes.
  if (lastClip !== undefined && status.last_clip && status.last_clip !== lastClip) {
    toast("Clip saved");
    loadClips(status.last_clip);
  }
  lastClip = status.last_clip;
  if (settings && settings.enabled !== status.enabled) {
    settings.enabled = status.enabled;
  }
  const s = $("#status");
  const enabled = status.enabled;
  s.classList.toggle("rec", status.recording);
  s.classList.toggle("err", !!status.error);
  let text;
  if (status.error && status.error.includes("isn't running")) text = "Recorder not running — start Hindsight from the Start menu";
  else if (status.error) text = `Not recording — ${status.error}`;
  else if (!enabled) text = "Paused";
  else if (!status.recording) text = "Starting…";
  else {
    const tracks = status.audio_tracks.length ? ` · ${status.audio_tracks.length} audio` : " · no audio";
    text = `${status.width}×${status.height} · ${status.fps} fps · ${codecName(status.encoder)}${tracks} · ${Math.floor(status.buffered_secs)}s buffered (${status.buffer_mb.toFixed(0)} MB)`;
  }
  $("#status-text").textContent = text;
  s.title = status.error || text;
  $("#toggle-rec").textContent = enabled ? "Pause" : "Resume";
  $("#save-clip").disabled = !status.recording;
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
  const on = !settings.enabled;
  try {
    await invoke("set_enabled", { enabled: on });
    settings.enabled = on;
  } catch (e) {
    toast(String(e), { error: true });
  }
  refreshStatus();
});

/* ---------- clips ---------- */
async function loadClips(highlight) {
  try {
    clips = await invoke("list_clips");
  } catch (e) {
    toast(`Couldn't read clips folder: ${e}`, { error: true });
    clips = [];
  }
  renderClips(highlight);
}

function renderChips() {
  const games = [...new Set(clips.map((c) => c.game))];
  const box = $("#game-chips");
  box.replaceChildren();
  if (games.length < 2) return;
  if (gameFilter && !games.includes(gameFilter)) gameFilter = null;
  const mk = (label, value) => {
    const c = el("button", "chip" + (gameFilter === value ? " active" : ""), label);
    c.onclick = () => {
      gameFilter = value;
      renderClips();
    };
    box.append(c);
  };
  mk("All", null);
  games.forEach((g) => mk(g, g));
}

function renderClips(highlight) {
  renderChips();
  const grid = $("#grid");
  const shown = clips.filter((c) => !gameFilter || c.game === gameFilter);
  grid.replaceChildren(...shown.map((c) => card(c, c.path === highlight)));
  $("#empty").hidden = clips.length > 0;
  $("#clip-count").textContent = clips.length ? `${shown.length} clip${shown.length === 1 ? "" : "s"}` : "";
}

function card(c, isNew) {
  const d = el("div", "card" + (isNew ? " new" : ""));
  d.dataset.path = c.path;
  const th = el("div", "thumb");
  const img = el("img");
  img.loading = "lazy";
  img.alt = "";
  if (c.thumb) img.src = convertFileSrc(c.thumb);
  else img.style.visibility = "hidden";
  th.append(img, el("span", "dur", fmtTime(c.duration, false)));
  const meta = el("div", "meta");
  meta.append(el("div", "game", c.game), el("div", "sub", `${fmtDate(c.created)} · ${fmtSize(c.size)}`));
  d.append(th, meta);
  d.onclick = () => openPlayer(c);
  return d;
}

listen("thumb", (e) => {
  const [clip, thumb] = e.payload;
  const c = clips.find((x) => x.path === clip);
  if (c) c.thumb = thumb;
  const img = document.querySelector(`.card[data-path="${CSS.escape(clip)}"] img`);
  if (img) {
    img.src = convertFileSrc(thumb);
    img.style.visibility = "";
  }
});

$("#open-folder").onclick = () => invoke("open_clips_folder");

/* ---------- player ---------- */
const video = $("#video");
let cur = null;
let trim = { start: 0, end: 0 };
let busy = false;

function openPlayer(c) {
  cur = c;
  $("#p-title").value = c.name;
  $("#video-error").hidden = true;
  video.src = convertFileSrc(c.path);
  trim = { start: 0, end: c.duration || 0 };
  $("#player").hidden = false;
  video.play().catch(() => {});
  drawTimeline();
}

function closePlayer() {
  video.pause();
  video.removeAttribute("src");
  video.load();
  $("#player").hidden = true;
  cur = null;
}

video.addEventListener("loadedmetadata", () => {
  if (cur && (!trim.end || Math.abs(trim.end - (cur.duration || 0)) < 0.01)) trim.end = video.duration;
  drawTimeline();
});
video.addEventListener("error", () => {
  if (cur) $("#video-error").hidden = false;
});
video.addEventListener("play", () => ($("#p-play").textContent = "❚❚"));
video.addEventListener("pause", () => ($("#p-play").textContent = "▶"));

const duration = () => (isFinite(video.duration) && video.duration > 0 ? video.duration : cur?.duration || 0);

function drawTimeline() {
  const d = duration() || 1;
  const pct = (t) => `${(t / d) * 100}%`;
  $("#sel").style.left = pct(trim.start);
  $("#sel").style.width = `${((trim.end - trim.start) / d) * 100}%`;
  $("#h-start").style.left = pct(trim.start);
  $("#h-end").style.left = pct(trim.end);
  $("#playhead").style.left = pct(video.currentTime || 0);
  $("#p-time").textContent = fmtTime(video.currentTime || 0);
  $("#p-dur").textContent = fmtTime(d);
  const full = trim.start < 0.05 && trim.end > d - 0.05;
  $("#p-range").textContent = full ? `Full clip · ${fmtTime(d)}` : `Selection ${fmtTime(trim.start)} – ${fmtTime(trim.end)} (${fmtTime(trim.end - trim.start)})`;
}

(function tick() {
  if (cur && !video.paused) {
    // Loop inside the trim selection.
    if (video.currentTime >= trim.end - 0.02 || video.currentTime < trim.start - 0.25) video.currentTime = trim.start;
    drawTimeline();
  }
  requestAnimationFrame(tick);
})();
video.addEventListener("seeked", drawTimeline);

const timeAt = (clientX) => {
  const r = $("#track").getBoundingClientRect();
  return Math.min(Math.max((clientX - r.left) / r.width, 0), 1) * duration();
};

function dragHandle(which) {
  return (e) => {
    e.stopPropagation();
    const move = (ev) => {
      const t = timeAt(ev.clientX);
      if (which === "start") trim.start = Math.min(t, trim.end - 0.2);
      else trim.end = Math.max(t, trim.start + 0.2);
      video.currentTime = which === "start" ? trim.start : trim.end;
      drawTimeline();
    };
    const up = () => {
      window.removeEventListener("pointermove", move);
      window.removeEventListener("pointerup", up);
    };
    window.addEventListener("pointermove", move);
    window.addEventListener("pointerup", up);
  };
}
$("#h-start").addEventListener("pointerdown", dragHandle("start"));
$("#h-end").addEventListener("pointerdown", dragHandle("end"));
$("#track").addEventListener("pointerdown", (e) => {
  video.currentTime = timeAt(e.clientX);
  drawTimeline();
  const move = (ev) => {
    video.currentTime = timeAt(ev.clientX);
    drawTimeline();
  };
  const up = () => {
    window.removeEventListener("pointermove", move);
    window.removeEventListener("pointerup", up);
  };
  window.addEventListener("pointermove", move);
  window.addEventListener("pointerup", up);
});

$("#p-play").onclick = () => (video.paused ? video.play() : video.pause());
$("#p-close").onclick = closePlayer;
$("#player").addEventListener("pointerdown", (e) => e.target.id === "player" && closePlayer());
$("#p-external").onclick = $("#p-external2").onclick = () => cur && invoke("open_external", { path: cur.path });
$("#p-folder").onclick = () => cur && invoke("show_in_folder", { path: cur.path });

$("#p-delete").onclick = async () => {
  if (!cur || !confirm(`Move "${cur.name}" to the Recycle Bin?`)) return;
  const path = cur.path;
  closePlayer();
  try {
    await invoke("delete_clip", { path });
    toast("Moved to Recycle Bin");
  } catch (e) {
    toast(`Delete failed: ${e}`, { error: true });
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
  try {
    const path = await invoke("rename_clip", { path: cur.path, name });
    cur.path = path;
    cur.name = name;
    toast("Renamed");
    loadClips();
  } catch (e) {
    toast(`Rename failed: ${e}`, { error: true });
    $("#p-title").value = cur.name;
  }
  video.src = convertFileSrc(cur.path);
  video.currentTime = t;
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
    toast(`${label} done`);
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

$("#p-trim").onclick = () =>
  runJob("Trim", "trim_clip", { path: cur.path, start: trim.start, end: trim.end, exact: $("#p-exact").checked });
$("#p-export").onclick = () =>
  runJob("Export", "export_clip", { path: cur.path, start: trim.start, end: trim.end, targetMb: Number($("#p-size").value) });

document.addEventListener("keydown", (e) => {
  if (!cur || e.target.tagName === "INPUT" || e.target.tagName === "SELECT") return;
  const d = duration();
  switch (e.key) {
    case " ":
      e.preventDefault();
      video.paused ? video.play() : video.pause();
      break;
    case "Escape":
      closePlayer();
      break;
    case "i":
    case "I":
      trim.start = Math.min(video.currentTime, trim.end - 0.2);
      drawTimeline();
      break;
    case "o":
    case "O":
      trim.end = Math.max(video.currentTime, trim.start + 0.2);
      drawTimeline();
      break;
    case "ArrowLeft":
      video.currentTime = Math.max(0, video.currentTime - (e.shiftKey ? 5 : 1));
      break;
    case "ArrowRight":
      video.currentTime = Math.min(d, video.currentTime + (e.shiftKey ? 5 : 1));
      break;
  }
});

/* ---------- settings ---------- */
const form = $("#settings");

function fillSettings() {
  for (const input of form.elements) {
    if (!input.name || !(input.name in settings)) continue;
    const v = settings[input.name];
    if (input.type === "checkbox") input.checked = !!v;
    else input.value = v == null ? "" : String(v);
  }
  $("#hotkey").textContent = settings.hotkey;
  $("#save-key").textContent = settings.hotkey;
  $("#empty-key").textContent = settings.hotkey;
  $("#empty-secs").textContent = settings.buffer_secs;
  $("#out-dir").textContent = settings.output_dir;
  $("#out-dir").title = settings.output_dir;
  memHint();
}

function memHint() {
  // Mirrors Settings::max_kbps: the peak the encoder may use.
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
  $("#mem-hint").textContent = `Replay buffer lives in RAM: up to ~${mb < 1000 ? Math.round(mb) + " MB" : (mb / 1000).toFixed(1) + " GB"} at peak bitrate (${w}×${h} @ ${settings.fps} fps, ${(kbps / 1000).toFixed(0)} Mbps cap). Typical gameplay uses much less.`;
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
    s.className = "saved show";
  } catch (e) {
    settings = prev;
    fillSettings();
    s.textContent = `Couldn't apply: ${e}`;
    s.className = "saved show error";
  }
  clearTimeout(savedTimer);
  savedTimer = setTimeout(() => s.classList.remove("show"), 2500);
  refreshStatus();
}

form.addEventListener("change", (e) => {
  const i = e.target;
  if (!i.name) return;
  const next = { ...settings };
  if (i.type === "checkbox") next[i.name] = i.checked;
  else if (["fps", "max_height", "buffer_secs"].includes(i.name)) next[i.name] = Number(i.value);
  else if (i.name === "monitor") next.monitor = i.value || null;
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
  b.textContent = "Press a key… (Esc cancels)";
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
let monitors = [];
(async function boot() {
  settings = await invoke("get_settings");
  monitors = await invoke("monitors");
  const sel = $("#monitor");
  sel.replaceChildren(el("option", null, "Primary display"));
  sel.firstChild.value = "";
  for (const m of monitors) {
    const o = el("option", null, m.label);
    o.value = m.id;
    sel.append(o);
  }
  fillSettings();
  refreshStatus();
  loadClips();
})();

// Mouse back/forward buttons would otherwise navigate the webview.
window.addEventListener("mouseup", (e) => (e.button === 3 || e.button === 4) && e.preventDefault());
