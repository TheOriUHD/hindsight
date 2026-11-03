//! Automatic clips from the official event feeds games publish for tools like
//! this: Valve's Game State Integration (CS2, Dota 2) and Riot's Live Client
//! Data API (League). Nothing here touches a game's process or memory.
//!
//! GSI only starts once the user turns a game on, which writes the small
//! config file Valve documents into that game's folder. Turning it off removes
//! the file. Everything runs on loopback.

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use hindsight_recorder::{data_dir, Recorder, Settings};
use serde_json::Value;
use winreg::enums::HKEY_CURRENT_USER;
use winreg::RegKey;

const GSI_PORT: u16 = 27532;
const MAX_BODY: usize = 2 << 20;
/// Kills this close together become one clip.
const MERGE: Duration = Duration::from_secs(9);
/// Clip length around the moment: the build-up, and the seconds after it.
const BEFORE_SECS: f64 = 12.0;
const AFTER_SECS: f64 = 3.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hit {
    Kill,
    Death,
    Win,
}

/* ---------- turning hits into clips ---------- */

fn label(kills: u32, deaths: u32, win: bool) -> &'static str {
    match kills {
        0 if win => "Win",
        0 if deaths > 0 => "Death",
        1 => "Kill",
        2 => "Double kill",
        3 => "Triple kill",
        4 => "Quad kill",
        _ => "Multi kill",
    }
}

#[derive(Default)]
struct Burst {
    game: &'static str,
    kills: u32,
    deaths: u32,
    win: bool,
    first: i64,
    last: i64,
}

fn start_clipper(rec: Arc<Recorder>) -> Sender<(&'static str, Hit)> {
    let (tx, rx) = channel::<(&'static str, Hit)>();
    std::thread::Builder::new()
        .name("event-clips".into())
        .spawn(move || {
            let mut burst: Option<Burst> = None;
            loop {
                let wait = if burst.is_some() { MERGE } else { Duration::from_secs(3600) };
                match rx.recv_timeout(wait) {
                    Ok((game, hit)) => {
                        if hit == Hit::Death && !rec.settings().auto_deaths {
                            continue;
                        }
                        let now = rec.now();
                        // A different game means the last one is over.
                        if burst.as_ref().is_some_and(|b| b.game != game) {
                            flush(&rec, burst.take());
                        }
                        let b = burst.get_or_insert_with(|| Burst { game, first: now, ..Default::default() });
                        b.last = now;
                        match hit {
                            Hit::Kill => b.kills += 1,
                            Hit::Death => b.deaths += 1,
                            Hit::Win => b.win = true,
                        }
                    }
                    Err(RecvTimeoutError::Timeout) => flush(&rec, burst.take()),
                    Err(RecvTimeoutError::Disconnected) => return,
                }
            }
        })
        .expect("spawn event clip thread");
    tx
}

fn flush(rec: &Recorder, burst: Option<Burst>) {
    let Some(b) = burst else { return };
    let end = b.last + (AFTER_SECS * 1e7) as i64;
    let secs = BEFORE_SECS + AFTER_SECS + (b.last - b.first) as f64 / 1e7;
    let what = label(b.kills, b.deaths, b.win);
    match rec.save_event_clip(b.game, what, end, secs) {
        Ok(p) => log::info!("auto clip: {}", p.display()),
        Err(e) => log::warn!("auto clip: {e:#}"),
    }
}

/* ---------- Valve Game State Integration ---------- */

/// Compares each update with the last one. Game state only says "you have 7
/// kills", so the event is the change, and the first update just sets the
/// baseline: starting Hindsight mid-match must not clip anything.
#[derive(Default)]
struct Gsi {
    kills: Option<i64>,
    deaths: Option<i64>,
    won: bool,
}

impl Gsi {
    fn step(&mut self, kills: Option<i64>, deaths: Option<i64>, live: bool, won: bool) -> Vec<Hit> {
        let mut out = Vec::new();
        if let (Some(old), Some(new)) = (self.kills, kills) {
            if live {
                out.extend((old..new).map(|_| Hit::Kill));
            }
        }
        if let (Some(old), Some(new)) = (self.deaths, deaths) {
            if live {
                out.extend((old..new).map(|_| Hit::Death));
            }
        }
        if won && !self.won {
            out.push(Hit::Win);
        }
        self.won = won;
        (self.kills, self.deaths) = (kills, deaths);
        out
    }
}

fn cs2_hits(state: &mut Gsi, v: &Value) -> Vec<Hit> {
    // While spectating, `player` is whoever is on screen.
    let me = v["provider"]["steamid"].as_str();
    if me.is_none() || me != v["player"]["steamid"].as_str() {
        return state.step(None, None, false, false);
    }
    let stats = &v["player"]["match_stats"];
    let phase = v["map"]["phase"].as_str().unwrap_or("");
    let live = matches!(phase, "live" | "intermission" | "gameover");
    let (ct, t) = (v["map"]["team_ct"]["score"].as_i64(), v["map"]["team_t"]["score"].as_i64());
    let won = phase == "gameover"
        && match (v["player"]["team"].as_str(), ct, t) {
            (Some("CT"), Some(a), Some(b)) => a > b,
            (Some("T"), Some(a), Some(b)) => b > a,
            _ => false,
        };
    state.step(stats["kills"].as_i64(), stats["deaths"].as_i64(), live, won)
}

fn dota_hits(state: &mut Gsi, v: &Value) -> Vec<Hit> {
    let p = &v["player"];
    let game = v["map"]["game_state"].as_str().unwrap_or("");
    let live = matches!(game, "DOTA_GAMERULES_STATE_GAME_IN_PROGRESS" | "DOTA_GAMERULES_STATE_POST_GAME");
    let team = p["team_name"].as_str();
    let won = team.is_some() && team == v["map"]["win_team"].as_str();
    state.step(p["kills"].as_i64(), p["deaths"].as_i64(), live, won)
}

fn read_request(s: &mut TcpStream) -> Option<Vec<u8>> {
    s.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        if let Some(head) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            let headers = String::from_utf8_lossy(&buf[..head]).to_ascii_lowercase();
            let len: usize = headers.lines().find_map(|l| l.strip_prefix("content-length:")?.trim().parse().ok())?;
            if len > MAX_BODY {
                return None;
            }
            while buf.len() < head + 4 + len {
                let n = s.read(&mut chunk).ok()?;
                if n == 0 {
                    return None;
                }
                buf.extend_from_slice(&chunk[..n]);
            }
            return Some(buf[head + 4..head + 4 + len].to_vec());
        }
        if buf.len() > 64 * 1024 {
            return None;
        }
        let n = s.read(&mut chunk).ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}

fn gsi_server(rec: Arc<Recorder>, hits: Sender<(&'static str, Hit)>) {
    let listener = match TcpListener::bind(("127.0.0.1", GSI_PORT)) {
        Ok(l) => l,
        Err(e) => return log::warn!("game events: can't listen on port {GSI_PORT}: {e}"),
    };
    let token = gsi_token();
    let (mut cs2, mut dota) = (Gsi::default(), Gsi::default());
    for stream in listener.incoming().flatten() {
        let mut stream = stream;
        let body = read_request(&mut stream);
        // Valve's client only needs a 2xx to keep sending.
        let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        let Some(v) = body.and_then(|b| serde_json::from_slice::<Value>(&b).ok()) else { continue };
        if v["auth"]["token"].as_str() != Some(token.as_str()) {
            continue;
        }
        let s = rec.settings();
        let found = match v["provider"]["appid"].as_i64() {
            Some(730) if s.auto_cs2 => cs2_hits(&mut cs2, &v).into_iter().map(|h| ("Counter-Strike 2", h)).collect(),
            Some(570) if s.auto_dota2 => dota_hits(&mut dota, &v).into_iter().map(|h| ("Dota 2", h)).collect(),
            _ => Vec::new(),
        };
        for hit in found {
            let _ = hits.send(hit);
        }
    }
}

fn gsi_token() -> String {
    let path = data_dir().join("gsi_token");
    if let Some(t) = std::fs::read_to_string(&path).ok().filter(|t| t.len() == 32) {
        return t;
    }
    let part = || RandomState::new().build_hasher().finish();
    let token = format!("{:016x}{:016x}", part(), part());
    let _ = std::fs::create_dir_all(data_dir());
    let _ = std::fs::write(&path, &token);
    token
}

/// "path" lines of Steam's libraryfolders.vdf.
fn library_paths(vdf: &str) -> Vec<PathBuf> {
    vdf.lines()
        .filter_map(|line| {
            let mut parts = line.split('"').filter(|p| !p.trim().is_empty());
            (parts.next()? == "path").then(|| PathBuf::from(parts.next().unwrap_or("").replace("\\\\", "\\")))
        })
        .collect()
}

fn steam_libraries() -> Vec<PathBuf> {
    let Ok(key) = RegKey::predef(HKEY_CURRENT_USER).open_subkey(r"Software\Valve\Steam") else { return vec![] };
    let Ok(steam) = key.get_value::<String, _>("SteamPath") else { return vec![] };
    let steam = PathBuf::from(steam);
    let vdf = std::fs::read_to_string(steam.join("steamapps").join("libraryfolders.vdf")).unwrap_or_default();
    let mut all = library_paths(&vdf);
    if !all.contains(&steam) {
        all.push(steam);
    }
    all
}

struct GameConfig {
    name: &'static str,
    /// Below steamapps/common.
    dir: &'static str,
    cfg: &'static str,
    data: &'static str,
}

const CS2: GameConfig = GameConfig {
    name: "Counter-Strike 2",
    dir: "Counter-Strike Global Offensive",
    cfg: r"game\csgo\cfg",
    data: "\"provider\" \"1\"\n    \"map\" \"1\"\n    \"player_id\" \"1\"\n    \"player_state\" \"1\"\n    \"player_match_stats\" \"1\"",
};
const DOTA2: GameConfig = GameConfig {
    name: "Dota 2",
    dir: "dota 2 beta",
    cfg: r"game\dota\cfg\gamestate_integration",
    data: "\"provider\" \"1\"\n    \"map\" \"1\"\n    \"player\" \"1\"",
};

fn cfg_text(game: &GameConfig, token: &str) -> String {
    format!(
        "\"Hindsight\"\n{{\n  \"uri\" \"http://127.0.0.1:{GSI_PORT}/\"\n  \"timeout\" \"5.0\"\n  \"buffer\" \"0.1\"\n  \"throttle\" \"0.5\"\n  \"heartbeat\" \"30.0\"\n  \"auth\" {{ \"token\" \"{token}\" }}\n  \"data\"\n  {{\n    {}\n  }}\n}}\n",
        game.data
    )
}

fn cfg_path(library: &Path, game: &GameConfig) -> PathBuf {
    library.join("steamapps").join("common").join(game.dir).join(game.cfg).join("gamestate_integration_hindsight.cfg")
}

fn set_config(game: &GameConfig, on: bool) -> Result<()> {
    let installed: Vec<PathBuf> =
        steam_libraries().into_iter().filter(|l| l.join("steamapps").join("common").join(game.dir).is_dir()).collect();
    if on && installed.is_empty() {
        bail!("{} isn't installed through Steam on this PC", game.name);
    }
    for library in installed {
        let path = cfg_path(&library, game);
        if on {
            std::fs::create_dir_all(path.parent().context("config folder")?)?;
            std::fs::write(&path, cfg_text(game, &gsi_token())).with_context(|| format!("writing {}", path.display()))?;
            log::info!("game events: wrote {}", path.display());
        } else if path.exists() {
            std::fs::remove_file(&path)?;
        }
    }
    Ok(())
}

/* ---------- Riot Live Client Data API ---------- */

fn same_name(a: &str, b: &str) -> bool {
    let name = |s: &str| s.split('#').next().unwrap_or("").trim().to_lowercase();
    !name(a).is_empty() && name(a) == name(b)
}

/// Hits from new entries in the event list; `seen` is the last EventID handled.
fn lol_hits(events: &Value, me: &str, seen: &mut Option<i64>) -> Vec<Hit> {
    let list = events["Events"].as_array().map(Vec::as_slice).unwrap_or(&[]);
    let newest = list.iter().filter_map(|e| e["EventID"].as_i64()).max();
    // Joined mid-game, or a new game started: nothing older counts.
    let Some(mut last) = (*seen).filter(|s| newest.is_some_and(|n| n >= *s)) else {
        *seen = newest;
        return Vec::new();
    };
    let mut out = Vec::new();
    for e in list {
        let Some(id) = e["EventID"].as_i64().filter(|i| *i > last) else { continue };
        last = id;
        match e["EventName"].as_str() {
            Some("ChampionKill") => {
                if same_name(e["KillerName"].as_str().unwrap_or(""), me) {
                    out.push(Hit::Kill);
                }
                if same_name(e["VictimName"].as_str().unwrap_or(""), me) {
                    out.push(Hit::Death);
                }
            }
            Some("GameEnd") if e["Result"].as_str() == Some("Win") => out.push(Hit::Win),
            _ => {}
        }
    }
    *seen = Some(last);
    out
}

fn lol_watcher(rec: Arc<Recorder>, hits: Sender<(&'static str, Hit)>) {
    // League serves this API on loopback with Riot's own self-signed certificate.
    let client = reqwest::Client::builder().danger_accept_invalid_certs(true).timeout(Duration::from_secs(2)).build();
    let Ok(client) = client else { return };
    let get = |path: &'static str| {
        let client = client.clone();
        tauri::async_runtime::block_on(async move {
            let r = client.get(format!("https://127.0.0.1:2999/liveclientdata/{path}")).send().await.ok()?;
            r.error_for_status().ok()?.json::<Value>().await.ok()
        })
    };
    let mut seen = None;
    loop {
        if !rec.settings().auto_lol {
            std::thread::sleep(Duration::from_secs(5));
            continue;
        }
        let me = get("activeplayername");
        let events = get("eventdata");
        match (me.as_ref().and_then(Value::as_str), events) {
            (Some(me), Some(events)) => {
                for hit in lol_hits(&events, me, &mut seen) {
                    let _ = hits.send(("League of Legends", hit));
                }
                std::thread::sleep(Duration::from_secs(1));
            }
            // No game running.
            _ => {
                seen = None;
                std::thread::sleep(Duration::from_secs(5));
            }
        }
    }
}

/* ---------- wiring ---------- */

struct Running {
    hits: Sender<(&'static str, Hit)>,
    gsi: AtomicBool,
    lol: AtomicBool,
}

static RUNNING: Mutex<Option<Arc<Running>>> = Mutex::new(None);

/// Start whatever the settings turn on. Threads stay parked once started.
pub fn start(rec: &Arc<Recorder>) {
    let s = rec.settings();
    if !(s.auto_cs2 || s.auto_dota2 || s.auto_lol) {
        return;
    }
    let running = RUNNING
        .lock()
        .unwrap()
        .get_or_insert_with(|| Arc::new(Running { hits: start_clipper(rec.clone()), gsi: AtomicBool::new(false), lol: AtomicBool::new(false) }))
        .clone();
    if (s.auto_cs2 || s.auto_dota2) && !running.gsi.swap(true, Ordering::SeqCst) {
        let (r, h) = (rec.clone(), running.hits.clone());
        let _ = std::thread::Builder::new().name("gsi".into()).spawn(move || gsi_server(r, h));
    }
    if s.auto_lol && !running.lol.swap(true, Ordering::SeqCst) {
        let (r, h) = (rec.clone(), running.hits.clone());
        let _ = std::thread::Builder::new().name("league-events".into()).spawn(move || lol_watcher(r, h));
    }
}

/// Writes or removes game config files when a toggle changes. Runs before the
/// settings are saved, so a game that isn't installed leaves its toggle off.
pub fn prepare(old: &Settings, new: &Settings) -> Result<()> {
    if old.auto_cs2 != new.auto_cs2 {
        set_config(&CS2, new.auto_cs2)?;
    }
    if old.auto_dota2 != new.auto_dota2 {
        set_config(&DOTA2, new.auto_dota2)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn cs2(kills: i64, deaths: i64, phase: &str) -> Value {
        json!({
            "provider": {"appid": 730, "steamid": "1"},
            "map": {"phase": phase, "team_ct": {"score": 13}, "team_t": {"score": 7}},
            "player": {"steamid": "1", "team": "CT", "match_stats": {"kills": kills, "deaths": deaths}},
        })
    }

    #[test]
    fn first_update_is_only_a_baseline() {
        let mut g = Gsi::default();
        assert!(cs2_hits(&mut g, &cs2(7, 3, "live")).is_empty());
        assert_eq!(cs2_hits(&mut g, &cs2(8, 3, "live")), [Hit::Kill]);
        assert_eq!(cs2_hits(&mut g, &cs2(10, 4, "live")), [Hit::Kill, Hit::Kill, Hit::Death]);
    }

    #[test]
    fn warmup_and_new_matches_dont_clip() {
        let mut g = Gsi::default();
        cs2_hits(&mut g, &cs2(0, 0, "warmup"));
        assert!(cs2_hits(&mut g, &cs2(4, 0, "warmup")).is_empty());
        assert!(cs2_hits(&mut g, &cs2(0, 0, "live")).is_empty());
    }

    #[test]
    fn spectating_someone_else_never_counts() {
        let mut g = Gsi::default();
        cs2_hits(&mut g, &cs2(1, 0, "live"));
        let mut other = cs2(9, 0, "live");
        other["player"]["steamid"] = json!("2");
        assert!(cs2_hits(&mut g, &other).is_empty());
        // Back on yourself is a new baseline, not eight kills.
        assert!(cs2_hits(&mut g, &cs2(1, 1, "live")).is_empty());
    }

    #[test]
    fn a_won_match_fires_once() {
        let mut g = Gsi::default();
        cs2_hits(&mut g, &cs2(1, 0, "live"));
        assert_eq!(cs2_hits(&mut g, &cs2(1, 0, "gameover")), [Hit::Win]);
        assert!(cs2_hits(&mut g, &cs2(1, 0, "gameover")).is_empty());
        let mut lost = cs2(1, 0, "gameover");
        lost["player"]["team"] = json!("T");
        let mut g = Gsi::default();
        assert!(cs2_hits(&mut g, &lost).is_empty());
    }

    #[test]
    fn dota_kills_deaths_and_win() {
        let v = |k: i64, d: i64, win: &str| {
            json!({
                "map": {"game_state": "DOTA_GAMERULES_STATE_GAME_IN_PROGRESS", "win_team": win},
                "player": {"team_name": "radiant", "kills": k, "deaths": d},
            })
        };
        let mut g = Gsi::default();
        dota_hits(&mut g, &v(2, 1, "none"));
        assert_eq!(dota_hits(&mut g, &v(3, 2, "none")), [Hit::Kill, Hit::Death]);
        assert_eq!(dota_hits(&mut g, &v(3, 2, "radiant")), [Hit::Win]);
    }

    #[test]
    fn league_counts_only_new_events_of_mine() {
        let events = |list: Value| json!({ "Events": list });
        let mut seen = None;
        let first = events(json!([{"EventID": 0, "EventName": "GameStart"}, {"EventID": 1, "EventName": "ChampionKill", "KillerName": "Me"}]));
        assert!(lol_hits(&first, "Me#EUW", &mut seen).is_empty());
        let next = events(json!([
            {"EventID": 1, "EventName": "ChampionKill", "KillerName": "Me"},
            {"EventID": 2, "EventName": "ChampionKill", "KillerName": "me", "VictimName": "Them"},
            {"EventID": 3, "EventName": "ChampionKill", "KillerName": "Them", "VictimName": "Me"},
            {"EventID": 4, "EventName": "GameEnd", "Result": "Win"},
        ]));
        assert_eq!(lol_hits(&next, "Me#EUW", &mut seen), [Hit::Kill, Hit::Death, Hit::Win]);
        assert!(lol_hits(&next, "Me#EUW", &mut seen).is_empty());
        // A new game restarts the ids.
        let fresh = events(json!([{"EventID": 0, "EventName": "GameStart"}]));
        assert!(lol_hits(&fresh, "Me#EUW", &mut seen).is_empty());
        assert_eq!(seen, Some(0));
    }

    fn request_over_socket(raw: Vec<u8>) -> Option<Vec<u8>> {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let addr = listener.local_addr().unwrap();
        let client = std::thread::spawn(move || {
            let mut s = TcpStream::connect(addr).unwrap();
            // Valve sends the headers and the body in separate writes.
            let split = raw.len() / 2;
            s.write_all(&raw[..split]).unwrap();
            s.flush().unwrap();
            std::thread::sleep(Duration::from_millis(20));
            s.write_all(&raw[split..]).unwrap();
        });
        let (mut stream, _) = listener.accept().unwrap();
        let body = read_request(&mut stream);
        client.join().unwrap();
        body
    }

    #[test]
    fn reads_a_post_split_across_packets() {
        let raw = b"POST / HTTP/1.1\r\nContent-Type: application/json\r\nContent-Length: 11\r\n\r\n{\"auth\":{}}\n".to_vec();
        assert_eq!(request_over_socket(raw).unwrap(), b"{\"auth\":{}}");
    }

    #[test]
    fn refuses_an_oversized_post() {
        let raw = format!("POST / HTTP/1.1\r\nContent-Length: {}\r\n\r\n", MAX_BODY + 1).into_bytes();
        assert!(request_over_socket(raw).is_none());
    }

    #[test]
    fn labels() {
        assert_eq!(label(1, 0, false), "Kill");
        assert_eq!(label(2, 1, false), "Double kill");
        assert_eq!(label(5, 0, true), "Multi kill");
        assert_eq!(label(0, 0, true), "Win");
        assert_eq!(label(0, 1, false), "Death");
    }

    #[test]
    fn reads_steam_library_paths() {
        let vdf = "\"libraryfolders\"\n{\n\t\"0\"\n\t{\n\t\t\"path\"\t\t\"D:\\\\Games\\\\Steam\"\n\t\t\"label\"\t\t\"\"\n\t}\n}\n";
        assert_eq!(library_paths(vdf), [PathBuf::from(r"D:\Games\Steam")]);
    }

    #[test]
    fn config_carries_the_token_and_port() {
        let text = cfg_text(&CS2, "abc");
        assert!(text.contains("\"token\" \"abc\"") && text.contains("127.0.0.1:27532") && text.contains("player_match_stats"));
    }
}
