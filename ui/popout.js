// Tray popout: recorder status, save/pause and the settings people change most.
// Runs in its own short-lived process; closes when it loses focus.
const { invoke } = window.__TAURI__.core;
const $ = (s) => document.querySelector(s);

let settings = null;
let status = null;

const codecName = (enc) => {
  if (!enc) return "";
  const [codec, vendor] = enc.split("_");
  const c = { av1: "AV1", hevc: "HEVC", h264: "H.264" }[codec] || codec;
  const v = { nvenc: "NVENC", amf: "AMF", mf: "MF", qsv: "QSV" }[vendor] || vendor;
  return `${c} (${v})`;
};

async function refresh() {
  try {
    status = await invoke("get_status");
  } catch {
    $("#po-state-text").textContent = "Not running";
    $("#po-meta").textContent = "Start Hindsight from the Start menu.";
    document.body.classList.add("err");
    $("#po-save").disabled = $("#po-toggle").disabled = true;
    return;
  }
  document.body.classList.toggle("on", status.recording);
  document.body.classList.toggle("err", !!status.error);
  $("#po-save").disabled = !status.recording;
  $("#po-toggle").disabled = false;
  $("#po-toggle").textContent = status.enabled ? "Pause" : "Resume";
  if (status.error) {
    $("#po-state-text").textContent = "Not recording";
    $("#po-meta").textContent = status.error;
  } else if (!status.enabled) {
    $("#po-state-text").textContent = "Paused";
    $("#po-meta").textContent = "The hotkey does nothing until you resume.";
  } else if (!status.recording) {
    $("#po-state-text").textContent = "Starting";
    $("#po-meta").textContent = " ";
  } else {
    $("#po-state-text").textContent = "Recording";
    $("#po-meta").textContent = `${status.height}p · ${status.fps} fps · ${codecName(status.encoder)} · ${Math.floor(status.buffered_secs)} s buffered`;
  }
}

function fill() {
  const form = $("#po-form");
  for (const name of ["buffer_secs", "fps", "quality", "desktop_audio", "microphone", "save_overlay"]) {
    const f = form.elements[name];
    if (!f) continue;
    if (f.type === "checkbox") f.checked = !!settings[name];
    else f.value = String(settings[name]);
  }
  $("#po-key").textContent = settings.hotkey;
}

$("#po-form").addEventListener("change", async (e) => {
  const i = e.target;
  const next = { ...settings };
  if (i.type === "checkbox") next[i.name] = i.checked;
  else if (i.name === "fps" || i.name === "buffer_secs") next[i.name] = Number(i.value);
  else next[i.name] = i.value;
  const prev = settings;
  settings = next;
  try {
    await invoke("set_settings", { settings: next });
  } catch {
    settings = prev;
    fill();
  }
  refresh();
});

$("#po-save").onclick = async () => {
  const b = $("#po-save");
  b.disabled = true;
  try {
    await invoke("save_clip");
    b.firstChild.textContent = "Saved";
    setTimeout(() => (b.firstChild.textContent = "Save clip"), 1500);
  } catch (e) {
    $("#po-meta").textContent = String(e);
  }
  b.disabled = false;
};
$("#po-toggle").onclick = async () => {
  await invoke("set_enabled", { enabled: !status?.enabled }).catch(() => {});
  refresh();
};
$("#po-open").onclick = () => invoke("open_full_app");
$("#po-folder").onclick = async () => {
  await invoke("open_clips_folder");
  invoke("close_popout");
};
document.addEventListener("keydown", (e) => e.key === "Escape" && invoke("close_popout"));

(async function boot() {
  try {
    settings = await invoke("get_settings");
    fill();
  } catch {}
  await refresh();
  // Show only once the content is there, so the window never flashes blank.
  requestAnimationFrame(() => invoke("popout_ready"));
  setInterval(refresh, 1000);
})();
