use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

use scraper::{Html, Selector};
use serde_json::Value;

use crate::utils::http_client;
use tracing::info;

static API_KEY: LazyLock<String> =
    LazyLock::new(|| std::env::var("LASTFM_API_KEY").unwrap_or_default());

static COVER_CACHE: LazyLock<Mutex<HashMap<String, String>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

static ALBUM_INFO_CACHE: LazyLock<Mutex<HashMap<String, Value>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

static RELEASE_DATE_CACHE: LazyLock<Mutex<HashMap<String, String>>> =
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

async fn album_get_info_cached(artist: &str, album: &str) -> Option<Value> {
    let key = format!("{}|{}", artist.to_lowercase(), album.to_lowercase());
    // check cache
    {
        let guard = ALBUM_INFO_CACHE.lock().unwrap();
        if let Some(v) = guard.get(&key) {
            return Some(v.clone());
        }
    }

    // not cached, fetch
    let res = api_call(&[
        ("method", "album.getInfo"),
        ("artist", artist),
        ("album", album),
    ])
    .await;
    if let Some(ad) = res.clone() {
        let mut guard = ALBUM_INFO_CACHE.lock().unwrap();
        guard.insert(key, ad.clone());
        return Some(ad);
    }
    None
}

fn extract_date_from_text(s: &str) -> Option<String> {
    let months = [
        "january",
        "february",
        "march",
        "april",
        "may",
        "june",
        "july",
        "august",
        "september",
        "october",
        "november",
        "december",
        "jan",
        "feb",
        "mar",
        "apr",
        "may",
        "jun",
        "jul",
        "aug",
        "sep",
        "oct",
        "nov",
        "dec",
    ];

    let toks: Vec<&str> = s.split_whitespace().collect();
    for (i, t) in toks.iter().enumerate() {
        let clean = t
            .trim_matches(|c: char| !c.is_alphanumeric())
            .to_lowercase();
        if let Some(pos) = months.iter().position(|&m| m == clean.as_str()) {
            let month_num = (pos % 12) + 1;
            let day = if i >= 1 {
                toks[i - 1]
                    .trim_matches(|c: char| !c.is_ascii_digit())
                    .to_string()
            } else {
                String::new()
            };
            let year = if i + 1 < toks.len() {
                toks[i + 1]
                    .trim_matches(|c: char| !c.is_ascii_digit())
                    .to_string()
            } else {
                String::new()
            };
            if !day.is_empty()
                && day.chars().all(|c| c.is_ascii_digit())
                && year.len() == 4
                && year.chars().all(|c| c.is_ascii_digit())
            {
                let d: u32 = day.parse().unwrap_or(1);
                return Some(format!("{}-{:02}-{:02}", year, month_num, d));
            }
            if year.len() == 4 && year.chars().all(|c| c.is_ascii_digit()) {
                return Some(format!("{}-{:02}", year, month_num));
            }
        }
    }
    None
}

pub struct TrackInfo {
    pub name: String,
    pub artist: String,
    pub album: Option<String>,
    pub image_url: Option<String>,
    pub genres: Vec<String>,
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
            async move {
                let (album, image_url, genres, duration, _track_number) =
                    get_track_info(&artist, &name).await;
                TrackInfo {
                    name,
                    artist,
                    album,
                    image_url,
                    genres,
                    duration_sec: duration.unwrap_or(0),
                }
            }
        })
        .collect();

    futures_util::future::join_all(futures).await
}

// album, image_url, genres, duration_seconds, release_date, track_number
async fn get_track_info(
    artist: &str,
    track: &str,
) -> (
    Option<String>,
    Option<String>,
    Vec<String>,
    Option<i64>,
    Option<u32>,
) {
    let data = match api_call(&[
        ("method", "track.getInfo"),
        ("artist", artist),
        ("track", track),
    ])
    .await
    {
        Some(v) => v,
        None => return (None, None, vec![], None, None),
    };

    let t = &data["track"];

    let mut album = t["album"]["title"].as_str().map(str::to_string);

    // track position, prefer album.getInfo tracklist rank (always an integer in the API response)
    let mut track_number: Option<u32> = t["album"]["@attr"]["position"]
        .as_u64()
        .or_else(|| {
            t["album"]["@attr"]["position"]
                .as_str()
                .and_then(|s| s.trim().parse::<u64>().ok())
        })
        .map(|n| n as u32)
        .filter(|&n| n > 0);

    // pick the largest available album image
    let mut image_url = t["album"]["image"].as_array().and_then(|imgs| {
        // prefer gif if available (search from largest to smallest)
        for img in imgs.iter().rev() {
            if let Some(url) = img["#text"].as_str() {
                if !url.is_empty() && url.to_lowercase().contains(".gif") {
                    return Some(url.to_string());
                }
            }
        }
        // fallback to largest non-empty
        imgs.iter().rev().find_map(|img| {
            let url = img["#text"].as_str()?;
            if url.is_empty() {
                None
            } else {
                Some(url.to_string())
            }
        })
    });

    // if we didn't find an image from the track info, try album.getInfo which may have original/gif
    if image_url.is_none() || track_number.is_none() {
        if let Some(alb) = &album {
            if let Some(ad) = album_get_info_cached(artist, alb).await {
                if let Some(imgs) = ad["album"]["image"].as_array() {
                    if image_url.is_none() {
                        for img in imgs.iter().rev() {
                            if let Some(url) = img["#text"].as_str() {
                                if !url.is_empty() && url.to_lowercase().contains(".gif") {
                                    image_url = Some(url.to_string());
                                    break;
                                }
                            }
                        }
                        if image_url.is_none() {
                            for img in imgs.iter().rev() {
                                if let Some(url) = img["#text"].as_str() {
                                    if !url.is_empty() {
                                        image_url = Some(url.to_string());
                                        break;
                                    }
                                }
                            }
                        }
                    }
                }
                // get rank from the album tracklist
                if track_number.is_none() {
                    if let Some(tracks_arr) = ad["album"]["tracks"]["track"].as_array() {
                        if let Some(tr) = tracks_arr.iter().find(|tr| {
                            tr["name"]
                                .as_str()
                                .map(|n| n.eq_ignore_ascii_case(track))
                                .unwrap_or(false)
                        }) {
                            track_number = tr["@attr"]["rank"]
                                .as_u64()
                                .or_else(|| {
                                    tr["@attr"]["rank"]
                                        .as_str()
                                        .and_then(|s| s.trim().parse::<u64>().ok())
                                })
                                .map(|n| n as u32)
                                .filter(|&n| n > 0);
                        }
                    }
                }
            }
        }
    }

    let mut genres: Vec<String> = t["toptags"]["tag"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|tag| tag["name"].as_str().map(str::to_string))
        .filter(|g| !g.eq_ignore_ascii_case("music"))
        .take(3)
        .collect();

    // fallback to artist top tags if track has none
    if genres.is_empty() {
        if let Some(tag_data) =
            api_call(&[("method", "artist.getTopTags"), ("artist", artist)]).await
        {
            genres = tag_data["toptags"]["tag"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|tag| tag["name"].as_str().map(str::to_string))
                .filter(|g| !g.eq_ignore_ascii_case("music"))
                .take(3)
                .collect();
        }
    }

    let duration = t["duration"]
        .as_str()
        .and_then(|d| d.parse::<i64>().ok())
        .map(|ms| ms / 1000);

    // if we still don't have an album title, try searching the artist's top albums
    if album.is_none() {
        if let Some(albums_data) = api_call(&[
            ("method", "artist.getTopAlbums"),
            ("artist", artist),
            ("limit", "50"),
        ])
        .await
        {
            if let Some(arr) = albums_data["topalbums"]["album"].as_array() {
                for a in arr {
                    if let Some(a_title) = a["name"].as_str() {
                        if let Some(ad) = album_get_info_cached(artist, a_title).await {
                            // check if this album contains the track
                            if let Some(tracks_arr) = ad["album"]["tracks"]["track"].as_array() {
                                let found = tracks_arr.iter().any(|tr| {
                                    tr["name"]
                                        .as_str()
                                        .map(|n| n.eq_ignore_ascii_case(track))
                                        .unwrap_or(false)
                                });
                                if found {
                                    if album.is_none() {
                                        album = Some(a_title.to_string());
                                    }
                                    // extract rank from the album tracklist (integer in API response)
                                    if track_number.is_none() {
                                        if let Some(tr) = tracks_arr.iter().find(|tr| {
                                            tr["name"]
                                                .as_str()
                                                .map(|n| n.eq_ignore_ascii_case(track))
                                                .unwrap_or(false)
                                        }) {
                                            track_number = tr["@attr"]["rank"]
                                                .as_u64()
                                                .or_else(|| {
                                                    tr["@attr"]["rank"]
                                                        .as_str()
                                                        .and_then(|s| s.trim().parse::<u64>().ok())
                                                })
                                                .map(|n| n as u32)
                                                .filter(|&n| n > 0);
                                        }
                                    }
                                    break;
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    (album, image_url, genres, duration, track_number)
}

pub async fn lookup(
    artist: &str,
    title: &str,
) -> (Option<String>, Option<String>, Vec<String>, Option<u32>) {
    let (album, image_url, genres, _, track_number) = get_track_info(artist, title).await;
    (album, image_url, genres, track_number)
}

// returns the track duration in seconds from Last.fm, or None if unavailable
pub async fn track_duration_sec(artist: &str, title: &str) -> Option<i64> {
    let (_, _, _, duration, _) = get_track_info(artist, title).await;
    duration.filter(|&d| d > 0)
}

pub async fn album_published(artist: &str, album: &str, track: Option<&str>) -> Option<String> {
    let key = format!("{}|{}", artist.to_lowercase(), album.to_lowercase());
    // check local cache first
    {
        let guard = RELEASE_DATE_CACHE.lock().unwrap();
        if let Some(d) = guard.get(&key) {
            return Some(d.clone());
        }
    }

    if let Some(scraped) = album_page_published_scrape(artist, album, track).await {
        let mut guard = RELEASE_DATE_CACHE.lock().unwrap();
        guard.insert(key, scraped.clone());
        return Some(scraped);
    }
    None
}

// return a preferred album image URL (prefer gif, then largest available)
pub async fn album_image(artist: &str, album: &str, track: Option<&str>) -> Option<String> {
    if album.is_empty() {
        return None;
    }
    let key = format!("{}|{}", artist.to_lowercase(), album.to_lowercase());
    // check cache
    {
        let guard = COVER_CACHE.lock().unwrap();
        if let Some(url) = guard.get(&key) {
            return Some(url.clone());
        }
    }

    // try the track page for an image first when the track name differs from the album
    if let Some(t) = track {
        if !t.eq_ignore_ascii_case(album) {
            let track_url = format!(
                "https://www.last.fm/music/{}/{}",
                url_encode(artist),
                url_encode(t)
            );
            if let Some(img) = scrape_image_at_url(&track_url).await {
                let mut guard = COVER_CACHE.lock().unwrap();
                guard.insert(key.clone(), img.clone());
                return Some(img);
            }
        }
    }

    if let Some(ad) = album_get_info_cached(artist, album).await {
        if let Some(imgs) = ad["album"]["image"].as_array() {
            // prefer gif
            for img in imgs.iter().rev() {
                if let Some(url) = img["#text"].as_str() {
                    if !url.is_empty() && url.to_lowercase().contains(".gif") {
                        let mut guard = COVER_CACHE.lock().unwrap();
                        guard.insert(key.clone(), url.to_string());
                        return Some(url.to_string());
                    }
                }
            }
            // fallback to largest non-empty
            for img in imgs.iter().rev() {
                if let Some(url) = img["#text"].as_str() {
                    if !url.is_empty() {
                        let mut guard = COVER_CACHE.lock().unwrap();
                        guard.insert(key.clone(), url.to_string());
                        return Some(url.to_string());
                    }
                }
            }
        }
    }
    // fallback: try scraping the album page for an og:image or background-image
    if let Some(scraped) = album_page_image_scrape(artist, album, None).await {
        let mut guard = COVER_CACHE.lock().unwrap();
        guard.insert(key.clone(), scraped.clone());
        return Some(scraped);
    }

    None
}

// return top genre tags for an album via album.getInfo
pub async fn album_genres(artist: &str, album: &str) -> Vec<String> {
    if album.is_empty() {
        return vec![];
    }
    if let Some(ad) = album_get_info_cached(artist, album).await {
        let tags: Vec<String> = ad["album"]["tags"]["tag"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|t| t["name"].as_str().map(str::to_string))
            .filter(|g| !g.eq_ignore_ascii_case("music"))
            .take(3)
            .collect();
        if !tags.is_empty() {
            return tags;
        }
    }
    vec![]
}

async fn scrape_published_at_url(url: &str) -> Option<String> {
    info!("lastfm scrape: GET {}", url);

    let text = tokio::time::timeout(
        std::time::Duration::from_millis(5000),
        http_client().get(url).send(),
    )
    .await
    .ok()?
    .ok()?
    .text()
    .await
    .ok()?;

    let doc = Html::parse_document(&text);
    let sel = Selector::parse(".catalogue-metadata-description").ok()?;
    let nodes: Vec<_> = doc.select(&sel).collect();
    // prefer the second node if present, then try others
    if nodes.len() > 1 {
        let txt = nodes[1]
            .text()
            .collect::<Vec<_>>()
            .join(" ")
            .trim()
            .to_string();
        if let Some(date) = extract_date_from_text(&txt) {
            return Some(date);
        }
        if !txt.is_empty() {
            return Some(txt);
        }
    }

    for node in &nodes {
        let txt = node.text().collect::<Vec<_>>().join(" ").trim().to_string();
        if let Some(date) = extract_date_from_text(&txt) {
            return Some(date);
        }
    }
    None
}

async fn album_page_published_scrape(
    artist: &str,
    album: &str,
    track: Option<&str>,
) -> Option<String> {
    // if a track name is provided and differs from the album, try it as an album URL first
    if let Some(t) = track {
        if !t.eq_ignore_ascii_case(album) {
            let track_url = format!(
                "https://www.last.fm/music/{}/{}",
                url_encode(artist),
                url_encode(t)
            );
            if let Some(date) = scrape_published_at_url(&track_url).await {
                return Some(date);
            }
        }
    }

    let url = format!(
        "https://www.last.fm/music/{}/{}",
        url_encode(artist),
        url_encode(album)
    );
    scrape_published_at_url(&url).await
}

async fn scrape_image_at_url(url: &str) -> Option<String> {
    info!("lastfm scrape-image: GET {}", url);

    let text = tokio::time::timeout(
        std::time::Duration::from_millis(5000),
        http_client().get(url).send(),
    )
    .await
    .ok()?
    .ok()?
    .text()
    .await
    .ok()?;

    let doc = Html::parse_document(&text);
    if let Ok(sel) = Selector::parse("meta[property=\"og:image\"]") {
        if let Some(el) = doc.select(&sel).next() {
            if let Some(content) = el.value().attr("content") {
                if !content.is_empty() {
                    return Some(content.to_string());
                }
            }
        }
    }

    // fallback: look for background-image url(...) in the page
    if let Some(idx) = text.find("background-image") {
        if let Some(start) = text[idx..].find("url(") {
            let s = &text[idx + start + 4..];
            if let Some(end) = s.find(')') {
                let candidate = s[..end].trim().trim_matches('"').trim_matches('\'');
                if !candidate.is_empty() {
                    return Some(candidate.to_string());
                }
            }
        }
    }

    None
}

async fn album_page_image_scrape(artist: &str, album: &str, track: Option<&str>) -> Option<String> {
    if let Some(t) = track {
        if !t.eq_ignore_ascii_case(album) {
            let track_url = format!(
                "https://www.last.fm/music/{}/{}",
                url_encode(artist),
                url_encode(t)
            );
            if let Some(img) = scrape_image_at_url(&track_url).await {
                return Some(img);
            }
        }
    }

    let url = format!(
        "https://www.last.fm/music/{}/{}",
        url_encode(artist),
        url_encode(album)
    );
    scrape_image_at_url(&url).await
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
