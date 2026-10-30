//! "Report a problem": gathers what's needed to diagnose an issue remotely.
//! Nothing here runs unless the user opens the dialog, they can read exactly
//! what is sent, and it's only sent when they press Send.

use std::io::{Read, Seek, SeekFrom};
use std::os::windows::process::CommandExt;
use std::path::Path;
use std::process::Command;

use anyhow::{bail, Result};
use serde_json::{json, Value};
use winreg::enums::{HKEY_LOCAL_MACHINE, KEY_READ};
use winreg::RegKey;

use crate::ipc::{self, Request};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const REPORT_URL: &str = "https://api.hindsighthq.app/report";
const LOG_TAIL: u64 = 256 * 1024;

/// Programs known to interact with capture, overlays or the cursor. Only these
/// are looked for; the full process list is never collected.
const RELEVANT_APPS: &[&str] = &[
    "obs64.exe", "discord.exe", "medal.exe", "outplayed.exe", "overwolf.exe", "nvcontainer.exe", "nvidia app.exe", "nvsphelper64.exe",
    "rtss.exe", "msiafterburner.exe", "steam.exe", "gamebar.exe", "wallpaper64.exe", "shadowplay.exe", "radeonsoftware.exe", "javaw.exe",
    "lossless scaling.exe", "displayfusion.exe", "rewasd.exe",
];

/// Strip the user's home folder and name from paths, and clip names (they come
/// from window titles, which can be private) from log lines.
fn redact(text: &str) -> String {
    let home = dirs_home();
    let user = home.rsplit(['\\', '/']).next().unwrap_or("").to_string();
    let mut out = String::with_capacity(text.len());
    for line in text.lines() {
        let line = match line.find(":\\").filter(|_| line.contains(".mp4")) {
            Some(i) => format!("{}<clip path>", line.get(..i.saturating_sub(1)).unwrap_or("")),
            None => line.to_string(),
        };
        let mut line = line.replace(&home, "%USERPROFILE%");
        if !user.is_empty() {
            line = line.replace(&format!("\\Users\\{user}"), "\\Users\\<user>").replace(&format!("/Users/{user}"), "/Users/<user>");
        }
        out.push_str(&line);
        out.push('\n');
    }
    out
}

fn dirs_home() -> String {
    std::env::var("USERPROFILE").unwrap_or_default()
}

fn tail(path: &Path) -> Option<String> {
    let mut f = std::fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    f.seek(SeekFrom::Start(len.saturating_sub(LOG_TAIL))).ok()?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf).ok()?;
    Some(redact(&String::from_utf8_lossy(&buf).replace('\0', "")))
}

fn windows_info() -> Value {
    let hklm = RegKey::predef(HKEY_LOCAL_MACHINE);
    let cv = hklm.open_subkey_with_flags(r"SOFTWARE\Microsoft\Windows NT\CurrentVersion", KEY_READ);
    let get = |k: &str| cv.as_ref().ok().and_then(|c| c.get_value::<String, _>(k).ok()).unwrap_or_default();
    let ubr: u32 = cv.as_ref().ok().and_then(|c| c.get_value("UBR").ok()).unwrap_or(0);
    let build = get("CurrentBuildNumber");
    // Windows 11 still calls itself "Windows 10" here; the build tells them apart.
    let product = get("ProductName");
    let product = if build.parse::<u32>().unwrap_or(0) >= 22000 { product.replace("Windows 10", "Windows 11") } else { product };
    json!(format!("{product} {} (build {build}.{ubr})", get("DisplayVersion")))
}

fn cpu() -> String {
    RegKey::predef(HKEY_LOCAL_MACHINE)
        .open_subkey_with_flags(r"HARDWARE\DESCRIPTION\System\CentralProcessor\0", KEY_READ)
        .and_then(|k| k.get_value::<String, _>("ProcessorNameString"))
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

fn ram_gb() -> f64 {
    use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    let mut m = MEMORYSTATUSEX { dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32, ..Default::default() };
    unsafe {
        let _ = GlobalMemoryStatusEx(&mut m);
    }
    (m.ullTotalPhys as f64 / (1u64 << 30) as f64 * 10.0).round() / 10.0
}

/// Display adapters with driver versions, from the driver's registry entries.
fn gpus() -> Vec<String> {
    let class = r"SYSTEM\CurrentControlSet\Control\Class\{4d36e968-e325-11ce-bfc1-08002be10318}";
    let Ok(key) = RegKey::predef(HKEY_LOCAL_MACHINE).open_subkey_with_flags(class, KEY_READ) else { return vec![] };
    let mut found: Vec<String> = key
        .enum_keys()
        .flatten()
        .filter_map(|sub| {
            let k = key.open_subkey_with_flags(&sub, KEY_READ).ok()?;
            let name: String = k.get_value("DriverDesc").ok()?;
            let ver: String = k.get_value("DriverVersion").unwrap_or_default();
            let date: String = k.get_value("DriverDate").unwrap_or_default();
            Some(format!("{name} (driver {ver}, {date})"))
        })
        .collect();
    found.sort();
    found.dedup();
    found
}

fn relevant_apps() -> Vec<String> {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Diagnostics::ToolHelp::*;
    let mut found = Vec::new();
    unsafe {
        let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else { return found };
        let mut e = PROCESSENTRY32W { dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
        if Process32FirstW(snap, &mut e).is_ok() {
            loop {
                let n = e.szExeFile.iter().position(|&c| c == 0).unwrap_or(e.szExeFile.len());
                let exe = String::from_utf16_lossy(&e.szExeFile[..n]).to_lowercase();
                if RELEVANT_APPS.contains(&exe.as_str()) && !found.contains(&exe) {
                    found.push(exe);
                }
                if Process32NextW(snap, &mut e).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snap);
    }
    found.sort();
    found
}

/// Recent Windows crash records that name Hindsight.
fn crashes() -> String {
    let q = "*[System[Provider[@Name='Application Error' or @Name='Windows Error Reporting']]]";
    let out = Command::new("wevtutil")
        .args(["qe", "Application", &format!("/q:{q}"), "/c:40", "/rd:true", "/f:text"])
        .creation_flags(CREATE_NO_WINDOW)
        .output();
    let text = out.map(|o| String::from_utf8_lossy(&o.stdout).to_string()).unwrap_or_default();
    let blocks: Vec<&str> = text.split("Event[").filter(|b| b.to_lowercase().contains("hindsight")).take(5).collect();
    redact(&blocks.join("\n"))
}

/// Everything except what the user types.
pub fn collect(version: &str) -> Value {
    let data = hindsight_recorder::data_dir();
    let status: Value = ipc::call(&Request::Status).unwrap_or(json!({ "error": "recorder not running" }));
    let settings: Value = ipc::call(&Request::Settings).unwrap_or(Value::Null);
    let mut status = status;
    let mut settings = settings;
    // Paths are the user's own business.
    status["last_clip"] = Value::Null;
    settings["output_dir"] = json!("<clips folder>");

    let gpus = gpus();
    let names = |d: Result<Vec<hindsight_recorder::AudioDevice>>| -> Vec<String> { d.unwrap_or_default().into_iter().map(|d| d.name).collect() };
    let mut logs = serde_json::Map::new();
    for f in ["hindsight.log", "hindsight.old.log", "hindsight-ui.log"] {
        if let Some(t) = tail(&data.join(f)) {
            logs.insert(f.to_string(), json!(t));
        }
    }
    json!({
        "app_version": version,
        "system": {
            "os": windows_info(),
            "cpu": cpu(),
            "ram_gb": ram_gb(),
            "gpu": gpus.join("; "),
            "monitors": hindsight_recorder::list_monitors().into_iter().map(|m| m.label).collect::<Vec<_>>(),
            "audio_outputs": names(hindsight_recorder::list_devices(true)),
            "audio_inputs": names(hindsight_recorder::list_devices(false)),
        },
        "recorder": status,
        "settings": settings,
        "apps_running": relevant_apps(),
        "crashes": crashes(),
        "logs": logs,
    })
}

/// Sends the report; returns its reference number.
pub async fn send(mut report: Value, description: String, contact: String) -> Result<String> {
    report["description"] = json!(description);
    report["contact"] = json!(contact);
    let resp = reqwest::Client::new().post(REPORT_URL).json(&report).send().await?;
    let status = resp.status();
    let body: Value = resp.json().await.unwrap_or(Value::Null);
    if !status.is_success() {
        bail!("{}", body["error"].as_str().unwrap_or("the server rejected the report"));
    }
    Ok(body["id"].as_str().unwrap_or("").to_string())
}
