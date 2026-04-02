use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use actix_web::{HttpRequest, HttpResponse};
use serde_json::Value;
use tracing::{info, warn};

use crate::utils::{http_client, parse_query, upstream_url};

struct CacheEntry {
    results: Vec<Value>,
    fetched_at: Instant,
    fetching: bool,
}

static CACHE: LazyLock<Mutex<HashMap<String, CacheEntry>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

static SEARCH_GEN: AtomicU64 = AtomicU64::new(0);

const CACHE_TTL: Duration = Duration::from_secs(5 * 60);
const DEBOUNCE: Duration = Duration::from_millis(800);

fn cache_get(q: &str) -> (Vec<Value>, bool) {
    let mut map = CACHE.lock().unwrap();
    match map.get_mut(q) {
        Some(e) if e.fetched_at.elapsed() < CACHE_TTL => (e.results.clone(), false),
        Some(e) if e.fetching => (e.results.clone(), false),
        _ => {
            let stale = map.get(q).map(|e| e.results.clone()).unwrap_or_default();
            map.insert(
                q.to_string(),
                CacheEntry {
                    results: stale.clone(),
                    fetched_at: Instant::now() - CACHE_TTL,
                    fetching: true,
                },
            );
            (stale, true)
        }
    }
}

fn cache_set(q: &str, results: Vec<Value>) {
    CACHE.lock().unwrap().insert(
        q.to_string(),
        CacheEntry {
            results,
            fetched_at: Instant::now(),
            fetching: false,
        },
    );
}

fn cache_peek(q: &str) -> Vec<Value> {
    CACHE
        .lock()
        .unwrap()
        .get(q)
        .map(|e| e.results.clone())
        .unwrap_or_default()
}

fn cache_clear_fetching(q: &str) {
    if let Some(e) = CACHE.lock().unwrap().get_mut(q) {
        e.fetching = false;
    }
}

pub async fn handle(req: HttpRequest) -> actix_web::Result<HttpResponse> {
    let query_map = parse_query(req.uri().query().unwrap_or(""));
    let q = query_map.get("query").cloned().unwrap_or_default();
    let raw_query = req.uri().query().unwrap_or("").to_string();

    info!("search: {}", q);

    let search_gen = SEARCH_GEN.fetch_add(1, Ordering::SeqCst) + 1;

    let (nd_result, _) = tokio::join!(nd_search(&raw_query), tokio::time::sleep(DEBOUNCE));
    let nd_songs: Vec<Value> = match nd_result {
        Ok(v) => v,
        Err(e) => {
            warn!("navidrome search failed: {}", e);
            vec![]
        }
    };

    let still_latest = SEARCH_GEN.load(Ordering::SeqCst) == search_gen;
    let lfm_songs = if !still_latest {
        cache_peek(&q)
    } else {
        let (cached, should_fetch) = cache_get(&q);
        if should_fetch {
            if cached.is_empty() {
                lastfm_fetch_and_cache(&q).await.unwrap_or_default()
            } else {
                let q_bg = q.clone();
                tokio::spawn(async move {
                    lastfm_fetch_and_cache(&q_bg).await;
                });
                cached
            }
        } else {
            cached
        }
    };

    let nd_titles: HashSet<String> = nd_songs
        .iter()
        .filter_map(|s| s["title"].as_str().map(|t| t.to_lowercase()))
        .collect();

    let lfm_deduped: Vec<Value> = lfm_songs
        .into_iter()
        .filter(|s| {
            s["title"]
                .as_str()
                .map(|t| !nd_titles.contains(&t.to_lowercase()))
                .unwrap_or(true)
        })
        .collect();

    let mut songs = nd_songs;
    songs.extend(lfm_deduped);

    Ok(HttpResponse::Ok().json(serde_json::json!({
        "subsonic-response": {
            "status": "ok",
            "version": "1.16.1",
            "searchResult3": {
                "artist": [],
                "album": [],
                "song": songs
            }
        }
    })))
}

async fn nd_search(raw_query: &str) -> anyhow::Result<Vec<Value>> {
    let url = format!("{}/rest/search3.view?{}", upstream_url(), raw_query);
    let resp = http_client()
        .get(&url)
        .send()
        .await?
        .json::<Value>()
        .await?;
    Ok(resp["subsonic-response"]["searchResult3"]["song"]
        .as_array()
        .cloned()
        .unwrap_or_default())
}

async fn lastfm_fetch_and_cache(q: &str) -> Option<Vec<Value>> {
    info!("Last.fm search: {}", q);
    let tracks = crate::lastfm::search(q).await;
    if tracks.is_empty() {
        cache_clear_fetching(q);
        return None;
    }

    let mapped = map_lastfm_tracks(&tracks);
    cache_set(q, mapped.clone());
    info!("Last.fm cache populated for: {}", q);
    Some(mapped)
}

fn map_lastfm_tracks(tracks: &[crate::lastfm::TrackInfo]) -> Vec<Value> {
    tracks
        .iter()
        .map(|t| {
            let id = crate::lastfm::encode_track_id(&t.artist, &t.name);

            if let Some(ref url) = t.image_url {
                crate::lastfm::cache_cover(&id, url);
            }

            let genres_json: Vec<Value> = t
                .genres
                .iter()
                .map(|g| serde_json::json!({"name": g}))
                .collect();

            serde_json::json!({
                "id": &id,
                "title": &t.name,
                "artist": &t.artist,
                "artistId": format!("yt_artist_{}", t.artist),
                "album": t.album.as_deref().unwrap_or(""),
                "albumId": "yt_album_lastfm",
                "coverArt": &id,
                "parent": null,
                "duration": t.duration_sec,
                "bitRate": 128,
                "bitDepth": 16,
                "samplingRate": 44100,
                "channelCount": 2,
                "size": 1_000_000,
                "suffix": "mp3",
                "contentType": "audio/mpeg",
                "path": format!("{}/{}.mp3", t.artist, t.name),
                "genres": genres_json,
                "bpm": 0,
                "moods": [],
                "contributors": [],
                "replayGain": {},
                "explicitStatus": ""
            })
        })
        .collect()
}
