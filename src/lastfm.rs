use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

use serde_json::Value;

use crate::utils::http_client;

static API_KEY: LazyLock<String> =
    LazyLock::new(|| std::env::var("LASTFM_API_KEY").unwrap_or_default());

static COVER_CACHE: LazyLock<Mutex<HashMap<String, String>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

async fn api_call(params: &[(&str, &str)]) -> Option<Value> {
    let key = API_KEY.as_str();
    if key.is_empty() {
        return None;
    }

    let mut url = format!(
        "https://ws.audioscrobbler.com/2.0/?api_key={}&format=json",
        url_encode(key)
    );
    for (k, v) in params {
        url.push('&');
        url.push_str(k);
        url.push('=');
        url.push_str(&url_encode(v));
    }

    tokio::time::timeout(
        std::time::Duration::from_millis(3000),
        http_client().get(&url).send(),
    )
    .await
    .ok()?
    .ok()?
    .json()
    .await
    .ok()
}

pub struct TrackInfo {
    pub name: String,
    pub artist: String,
    pub album: Option<String>,
    pub image_url: Option<String>,
    pub genres: Vec<String>,
    pub listeners: i64,
    pub duration_sec: i64,
}

pub async fn search(query: &str) -> Vec<TrackInfo> {
    let data = match api_call(&[
        ("method", "track.search"),
        ("track", query),
        ("limit", "15"),
    ])
    .await
    {
        Some(v) => v,
        None => return vec![],
    };

    let tracks = match data["results"]["trackmatches"]["track"].as_array() {
        Some(arr) => arr.clone(),
        None => return vec![],
    };

    let futures: Vec<_> = tracks
        .into_iter()
        .take(10)
        .map(|t| {
            let name = t["name"].as_str().unwrap_or("").to_string();
            let artist = t["artist"].as_str().unwrap_or("").to_string();
            let listeners: i64 = t["listeners"].as_str().unwrap_or("0").parse().unwrap_or(0);
            async move {
                let (album, image_url, genres, duration) = get_track_info(&artist, &name).await;
                TrackInfo {
                    name,
                    artist,
                    album,
                    image_url,
                    genres,
                    listeners,
                    duration_sec: duration.unwrap_or(0),
                }
            }
        })
        .collect();

    futures_util::future::join_all(futures).await
}

// returns (album, image_url, genres, duration_seconds)
async fn get_track_info(
    artist: &str,
    track: &str,
) -> (Option<String>, Option<String>, Vec<String>, Option<i64>) {
    let data = match api_call(&[
        ("method", "track.getInfo"),
        ("artist", artist),
        ("track", track),
    ])
    .await
    {
        Some(v) => v,
        None => return (None, None, vec![], None),
    };

    let t = &data["track"];

    let album = t["album"]["title"].as_str().map(str::to_string);

    // pick the largest available album image
    let image_url = t["album"]["image"].as_array().and_then(|imgs| {
        imgs.iter().rev().find_map(|img| {
            let url = img["#text"].as_str()?;
            if url.is_empty() {
                None
            } else {
                Some(url.to_string())
            }
        })
    });

    let genres: Vec<String> = t["toptags"]["tag"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|tag| tag["name"].as_str().map(str::to_string))
        .filter(|g| !g.eq_ignore_ascii_case("music"))
        .take(3)
        .collect();

    let duration = t["duration"]
        .as_str()
        .and_then(|d| d.parse::<i64>().ok())
        .map(|ms| ms / 1000);

    (album, image_url, genres, duration)
}

pub async fn lookup(artist: &str, title: &str) -> (Option<String>, Option<String>, Vec<String>) {
    let (album, image_url, genres, _) = get_track_info(artist, title).await;
    (album, image_url, genres)
}

pub async fn top_tracks(artist: &str, limit: usize) -> Vec<(String, String)> {
    let limit_str = limit.to_string();
    let data = match api_call(&[
        ("method", "artist.getTopTracks"),
        ("artist", artist),
        ("limit", &limit_str),
    ])
    .await
    {
        Some(v) => v,
        None => return vec![],
    };

    data["toptracks"]["track"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|t| {
            let name = t["name"].as_str()?.to_string();
            let artist = t["artist"]["name"].as_str()?.to_string();
            Some((name, artist))
        })
        .collect()
}

// encode artist + title into a stable subsonic-compatible ID
pub fn encode_track_id(artist: &str, title: &str) -> String {
    let data = format!("{}\0{}", artist, title);
    let hex: String = data.bytes().map(|b| format!("{:02x}", b)).collect();
    format!("yt_{}", hex)
}

// decode an encoded track ID back into (artist, title)
pub fn decode_track_id(raw_id: &str) -> Option<(String, String)> {
    let hex = raw_id.strip_prefix("yt_")?;
    if hex.len() < 4 || hex.len() % 2 != 0 {
        return None;
    }
    let bytes: Result<Vec<u8>, _> = (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16))
        .collect();
    let bytes = bytes.ok()?;
    let s = String::from_utf8(bytes).ok()?;
    let mut parts = s.splitn(2, '\0');
    let artist = parts.next()?;
    let title = parts.next()?;
    if artist.is_empty() || title.is_empty() {
        return None;
    }
    Some((artist.to_string(), title.to_string()))
}

pub fn cache_cover(id: &str, url: &str) {
    COVER_CACHE
        .lock()
        .unwrap()
        .insert(id.to_string(), url.to_string());
}

pub fn get_cached_cover(id: &str) -> Option<String> {
    COVER_CACHE.lock().unwrap().get(id).cloned()
}
