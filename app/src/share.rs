//! Share links. Nothing leaves this PC until the user presses Share: the
//! selection is encoded to fit, uploaded to clips.hindsighthq.app, and the link
//! and its delete token are kept here so the user can take it down again.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

const API: &str = "https://clips.hindsighthq.app/api/clips";
/// Under the server's 60 MB limit.
pub const MAX_MB: f64 = 57.0;

#[derive(Serialize, Deserialize, Clone)]
struct Share {
    id: String,
    url: String,
    delete_token: String,
    expires_at: u64,
}

fn store() -> PathBuf {
    hindsight_recorder::data_dir().join("shares.json")
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// Links by clip file name, without the ones that have expired.
fn load() -> HashMap<String, Share> {
    let all: HashMap<String, Share> = std::fs::read(store()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
    all.into_iter().filter(|(_, s)| s.expires_at > now_ms()).collect()
}

fn save(shares: &HashMap<String, Share>) -> Result<()> {
    std::fs::write(store(), serde_json::to_vec(shares)?)?;
    Ok(())
}

fn key(clip: &Path) -> String {
    clip.file_name().unwrap_or_default().to_string_lossy().into()
}

pub fn links() -> HashMap<String, String> {
    load().into_iter().map(|(k, v)| (k, v.url)).collect()
}

/// Percent-encodes everything but letters and digits, for a header value.
fn header_text(text: &str) -> String {
    text.bytes().map(|b| if b.is_ascii_alphanumeric() { (b as char).to_string() } else { format!("%{b:02X}") }).collect()
}

/// "Fortnite 2026-10-30 22-20-54 Double kill" reads better as "Fortnite Double kill".
fn share_title(name: &str, game: &str) -> String {
    let date = name.split(' ').position(|p| p.len() == 10 && p.as_bytes()[4] == b'-' && p.as_bytes()[7] == b'-');
    let Some(date) = date else { return name.to_string() };
    let label = name.split(' ').skip(date + 2).collect::<Vec<_>>().join(" ");
    if label.is_empty() {
        game.to_string()
    } else {
        format!("{game} {label}")
    }
}

fn client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder().timeout(Duration::from_secs(300)).build()?)
}

async fn refused(resp: reqwest::Response) -> anyhow::Error {
    let status = resp.status();
    let body: Value = resp.json().await.unwrap_or(Value::Null);
    anyhow::anyhow!("{}", body["error"].as_str().map(String::from).unwrap_or_else(|| format!("the server answered {status}")))
}

/// Uploads `file` (the encoded copy of `clip`) and remembers the link.
pub async fn upload(clip: &Path, file: &Path, name: &str, game: &str, duration: f64) -> Result<String> {
    let data = std::fs::read(file).context("reading the encoded clip")?;
    let (w, h) = crate::clips::video_size(file);
    let thumb = crate::clips::share_thumb(file);
    let http = client()?;
    let mut req = http
        .post(API)
        .header("content-type", "video/mp4")
        .header("x-title", header_text(&share_title(name, game)))
        .header("x-game", header_text(game))
        .header("x-duration", format!("{duration:.2}"))
        .header("x-width", w.to_string())
        .header("x-height", h.to_string());
    // Lets a build be tried end to end before sharing opens to everyone.
    if let Ok(k) = std::env::var("HINDSIGHT_SHARE_TEST_KEY") {
        req = req.header("x-hindsight-test", k);
    }
    let resp = req.body(data).send().await.context("uploading")?;
    if !resp.status().is_success() {
        return Err(refused(resp).await);
    }
    let body: Value = resp.json().await?;
    let share = Share {
        id: body["id"].as_str().context("no id in the reply")?.to_string(),
        url: body["url"].as_str().context("no link in the reply")?.to_string(),
        delete_token: body["delete_token"].as_str().context("no token in the reply")?.to_string(),
        expires_at: body["expires_at"].as_u64().unwrap_or(0),
    };
    // The link works without a thumbnail, so a failure here isn't fatal.
    match thumb {
        Ok(jpg) => {
            let sent = http.put(format!("{API}/{}/thumb", share.id)).bearer_auth(&share.delete_token).header("content-type", "image/jpeg").body(jpg).send().await;
            if let Err(e) = sent {
                log::warn!("share thumbnail: {e:#}");
            }
        }
        Err(e) => log::warn!("share thumbnail: {e:#}"),
    }
    let url = share.url.clone();
    let mut shares = load();
    shares.insert(key(clip), share);
    save(&shares)?;
    Ok(url)
}

pub async fn remove(clip: &Path) -> Result<()> {
    let mut shares = load();
    let Some(share) = shares.get(&key(clip)).cloned() else { bail!("this clip isn't shared") };
    let resp = client()?.delete(format!("{API}/{}", share.id)).bearer_auth(&share.delete_token).send().await.context("removing the link")?;
    // Already gone counts: it's what the user asked for.
    if !resp.status().is_success() && resp.status() != reqwest::StatusCode::NOT_FOUND {
        return Err(refused(resp).await);
    }
    shares.remove(&key(clip));
    save(&shares)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn titles_lose_the_timestamp() {
        assert_eq!(share_title("Fortnite 2026-10-30 22-20-54", "Fortnite"), "Fortnite");
        assert_eq!(share_title("Counter-Strike 2 2026-11-01 22-10-00 Double kill", "Counter-Strike 2"), "Counter-Strike 2 Double kill");
        assert_eq!(share_title("my best shot", "Clips"), "my best shot");
    }

    /// Against the real service: HINDSIGHT_SHARE_TEST_KEY and HINDSIGHT_SHARE_TEST_CLIP set, run with --ignored.
    #[test]
    #[ignore]
    fn uploads_and_removes_for_real() {
        let file = PathBuf::from(std::env::var("HINDSIGHT_SHARE_TEST_CLIP").unwrap());
        let clip = PathBuf::from("Share test 2026-11-01 22-10-00 Double kill.mp4");
        tauri::async_runtime::block_on(async {
            let url = upload(&clip, &file, "Share test 2026-11-01 22-10-00 Double kill", "Share test", 6.0).await.unwrap();
            assert!(url.starts_with("https://clips.hindsighthq.app/c/"));
            assert_eq!(links().get(&key(&clip)), Some(&url));
            let page = reqwest::get(&url).await.unwrap().text().await.unwrap();
            assert!(page.contains("Share test Double kill"), "title didn't arrive");
            remove(&clip).await.unwrap();
            assert_eq!(links().get(&key(&clip)), None);
            assert_eq!(reqwest::get(&url).await.unwrap().status(), 404);
        });
    }

    #[test]
    fn header_values_survive_any_name() {
        assert_eq!(header_text("a b/é"), "a%20b%2F%C3%A9");
    }
}
