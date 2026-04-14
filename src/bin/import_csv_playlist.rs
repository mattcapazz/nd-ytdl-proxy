//! Import a playlist from csv file to Navidrome
//!
//! Reads a csv file and creates or updates a Navidrome playlist,
//! downloading songs from YouTube as needed.
//!
//! csv file: ./src/bin/playlist.csv
//!
//! Usage:
//!   import-csv-playlist <user> [playlist_name]
//!
//! CSV format (title,artist[,youtube_url] per line):
//!   Song Name 1,Artist1
//!   Song2,Artist 2
//!   "Song, with commas",Artist3
//!   "Obscure Song",Really Unknown Artist,https://youtu.be/video_id
//!
//! Rate limiting: 15 downloads per 5 minutes
//! Environment variables:
//!   UPSTREAM_URL (default: http://localhost:4533)
//!   ND_ADMIN_USER
//!   ND_ADMIN_PASS
//!   MUSIC_DIR
//!   DB_PATH - database path (default: data/library.db)

use nd_ytdl_proxy::{db, download, title, utils};
use serde_json::Value;
use std::time::{Duration, Instant};
use tracing::{error, info, warn};

struct Song {
    artist: String,
    title: String,
    yt_url: Option<String>,
}

async fn read_csv(path: &str) -> anyhow::Result<Vec<Song>> {
    let content = tokio::fs::read_to_string(path).await?;
    // filter comment lines before handing to the csv parser
    let filtered: String = content
        .lines()
        .filter(|l| !l.trim().is_empty() && !l.trim_start().starts_with('#'))
        .flat_map(|l| [l, "\n"])
        .collect();

    let mut rdr = csv::ReaderBuilder::new()
        .has_headers(false)
        .flexible(true)
        .from_reader(filtered.as_bytes());

    let mut songs = Vec::new();
    for result in rdr.records() {
        let record = result?;
        let raw_title = match record.get(0) {
            Some(v) => v.trim(),
            None => continue,
        };
        let artist = match record.get(1) {
            Some(v) => v.trim().to_string(),
            None => continue,
        };
        let yt_url = record
            .get(2)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        // strip YouTube-style tags and artist prefix duplications from the title
        let clean = title::strip_tags(raw_title);
        let clean = utils::strip_artist_prefix(&artist, &clean);
        if !artist.is_empty() && !clean.is_empty() {
            songs.push(Song {
                artist,
                title: clean,
                yt_url,
            });
        }
    }

    anyhow::ensure!(!songs.is_empty(), "no valid songs found in csv");
    info!("loaded {} songs from csv", songs.len());
    Ok(songs)
}

// finds existing playlist by name or creates one, returns (id, existing_song_ids)
async fn find_or_create_playlist(name: &str) -> anyhow::Result<String> {
    let url = format!(
        "{}/rest/getPlaylists.view?{}",
        nd_url(),
        utils::admin_auth_query()
    );
    let resp: Value = utils::http_client().get(&url).send().await?.json().await?;

    if let Some(list) = resp["subsonic-response"]["playlists"]["playlist"].as_array() {
        for pl in list {
            if pl["name"].as_str() == Some(name) {
                let id = pl["id"].as_str().unwrap_or("").to_string();
                if !id.is_empty() {
                    info!("found existing playlist '{}' ({})", name, id);
                    return Ok(id);
                }
            }
        }
    }

    let url = format!(
        "{}/rest/createPlaylist.view?{}&name={}",
        nd_url(),
        utils::admin_auth_query(),
        utils::url_encode_param(name)
    );
    let resp: Value = utils::http_client().get(&url).send().await?.json().await?;
    let id = resp["subsonic-response"]["playlist"]["id"]
        .as_str()
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow::anyhow!("failed to create playlist: {:?}", resp))?;
    info!("created playlist '{}' ({})", name, id);
    Ok(id)
}

// strips punctuation and collapses whitespace for fuzzy title comparison
fn normalize(s: &str) -> String {
    // remove apostrophes first so "what's" → "whats" (matches FTS5 indexing)
    let no_apos = s.replace('\'', "").replace('\u{2019}', "");
    no_apos
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { ' ' })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

// searches Navidrome for a song by artist+title, returns its internal song ID
async fn search_navidrome(artist: &str, track_title: &str) -> Option<String> {
    // build a list of progressively shorter title variants to try:
    // e.g. "Hard to Say I'm Sorry / Get Away - 2005 Remaster"
    //   -> "Hard to Say I'm Sorry / Get Away"
    //   -> "Hard to Say I'm Sorry"
    let mut variants: Vec<String> = vec![track_title.to_string()];
    for sep in [" - ", " / "] {
        if let Some(pos) = track_title.find(sep) {
            let shorter = track_title[..pos].trim().to_string();
            if !shorter.is_empty() && !variants.contains(&shorter) {
                variants.push(shorter);
            }
        }
    }

    for variant in &variants {
        // try the original query first (preserves contractions like I'm, Don't, I'll),
        // then fall back to a normalized query (strips ? ' & which break FTS5 tokenizer)
        let queries = {
            let original = format!("{} {}", artist, variant);
            let normalized = normalize(&original);
            if normalized == original.to_lowercase() {
                vec![original]
            } else {
                vec![original, normalized]
            }
        };

        for query in &queries {
            let url = format!(
                "{}/rest/search3.view?{}&query={}&songCount=10",
                nd_url(),
                utils::admin_auth_query(),
                utils::url_encode_param(query)
            );
            let resp: Value = match utils::http_client().get(&url).send().await {
                Ok(r) => match r.json().await {
                    Ok(v) => v,
                    Err(_) => continue,
                },
                Err(_) => continue,
            };

            info!("search3 '{}': {}", query, resp);

            let songs = match resp["subsonic-response"]["searchResult3"]["song"].as_array() {
                Some(s) => s,
                None => continue,
            };
            let want_artist = normalize(artist);
            let want_title = normalize(variant);

            for song in songs {
                let nd_artist = normalize(song["artist"].as_str().unwrap_or(""));
                let nd_title = normalize(song["title"].as_str().unwrap_or(""));
                let artist_match =
                    nd_artist.contains(&want_artist) || want_artist.contains(&nd_artist);
                let title_match = nd_title.contains(&want_title) || want_title.contains(&nd_title);
                if artist_match && title_match {
                    return song["id"].as_str().map(|s| s.to_string());
                }
            }
        }
    }
    None
}

async fn add_to_playlist(playlist_id: &str, song_id: &str) -> anyhow::Result<()> {
    let url = format!(
        "{}/rest/updatePlaylist.view?{}&playlistId={}&songIdToAdd={}",
        nd_url(),
        utils::admin_auth_query(),
        utils::url_encode_param(playlist_id),
        utils::url_encode_param(song_id)
    );
    let resp: Value = utils::http_client().get(&url).send().await?.json().await?;
    info!("updatePlaylist response: {}", resp);
    Ok(())
}

// changes playlist owner using Navidrome's native REST API with JWT auth
async fn change_playlist_owner(
    playlist_id: &str,
    playlist_name: &str,
    owner: &str,
) -> anyhow::Result<()> {
    let admin_user = std::env::var("ND_ADMIN_USER").unwrap_or_else(|_| "admin".to_string());
    let admin_pass = std::env::var("ND_ADMIN_PASS").unwrap_or_else(|_| "admin".to_string());

    let login_url = format!("{}/auth/login", nd_url());
    let resp: Value = utils::http_client()
        .post(&login_url)
        .json(&serde_json::json!({"username": admin_user, "password": admin_pass}))
        .send()
        .await?
        .json()
        .await?;

    let token = resp["token"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("no token in login response: {:?}", resp))?;

    let put_url = format!(
        "{}/api/playlist/{}",
        nd_url(),
        utils::url_encode_param(playlist_id)
    );
    let status = utils::http_client()
        .put(&put_url)
        .header("Authorization", format!("Bearer {}", token))
        .json(&serde_json::json!({"owner": owner, "name": playlist_name}))
        .send()
        .await?
        .status();

    if status.is_success() {
        info!("changed playlist owner to {}", owner);
    } else {
        warn!("owner change returned {}", status);
    }
    Ok(())
}

// triggers a full library rescan and waits until it finishes
async fn full_scan_and_wait() {
    let scan_url = format!(
        "{}/rest/startScan.view?{}&full=true",
        nd_url(),
        utils::admin_auth_query()
    );
    let status_url = format!(
        "{}/rest/getScanStatus.view?{}",
        nd_url(),
        utils::admin_auth_query()
    );

    let _ = utils::http_client().get(&scan_url).send().await;

    // wait for scanning to start (up to 5s)
    for _ in 0..10u32 {
        tokio::time::sleep(Duration::from_millis(500)).await;
        if let Ok(resp) = utils::http_client().get(&status_url).send().await {
            if let Ok(v) = resp.json::<Value>().await {
                if v["subsonic-response"]["scanStatus"]["scanning"].as_bool() == Some(true) {
                    break;
                }
            }
        }
    }

    // wait for scan to finish (up to 2 minutes)
    for _ in 0..60u32 {
        tokio::time::sleep(Duration::from_secs(2)).await;
        if let Ok(resp) = utils::http_client().get(&status_url).send().await {
            if let Ok(v) = resp.json::<Value>().await {
                if v["subsonic-response"]["scanStatus"]["scanning"].as_bool() == Some(false) {
                    // give Navidrome a moment to commit index writes before querying
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    return;
                }
            }
        }
    }
}

// extracts the YouTube video ID from a watch or short URL
fn yt_id_from_url(url: &str) -> Option<String> {
    // https://youtu.be/VIDEO_ID or https://www.youtube.com/watch?v=VIDEO_ID
    if let Some(rest) = url
        .strip_prefix("https://youtu.be/")
        .or_else(|| url.strip_prefix("http://youtu.be/"))
    {
        let id = rest.split(&['?', '&', '#'][..]).next().unwrap_or("").trim();
        if !id.is_empty() {
            return Some(id.to_string());
        }
    }
    if let Some(pos) = url.find("v=") {
        let after = &url[pos + 2..];
        let id = after.split(&['&', '#'][..]).next().unwrap_or("").trim();
        if !id.is_empty() {
            return Some(id.to_string());
        }
    }
    None
}

// searches up to 5 YouTube results and returns the first video ID whose duration
// is within 10 seconds of the expected Last.fm duration (if known)
async fn yt_video_id(
    artist: &str,
    track_title: &str,
    expected_sec: Option<i64>,
) -> anyhow::Result<String> {
    let query = format!("{} - {}", artist, track_title);
    let count = if expected_sec.is_some() { "5" } else { "1" };
    let output = tokio::process::Command::new("yt-dlp")
        .args([
            &format!("ytsearch{}:{}", count, query),
            "--print",
            "%(id)s %(duration)s",
            "--no-playlist",
        ])
        .output()
        .await?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let lines: Vec<&str> = stdout
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    anyhow::ensure!(!lines.is_empty(), "no YouTube result for: {}", query);

    if let Some(expected) = expected_sec {
        let tolerance = 10i64;
        for line in &lines {
            let parts: Vec<&str> = line.splitn(2, ' ').collect();
            if parts.len() != 2 {
                continue;
            }
            let id = parts[0];
            // yt-dlp can print floats like "435.0"
            if let Ok(dur) = parts[1].trim().parse::<f64>() {
                let dur_sec = dur.round() as i64;
                if (dur_sec - expected).abs() <= tolerance {
                    info!(
                        "yt {}: duration {}s matches expected {}s",
                        id, dur_sec, expected
                    );
                    return Ok(id.to_string());
                }
                warn!(
                    "yt {}: duration {}s rejected (expected {}s +/-{}s)",
                    id, dur_sec, expected, tolerance
                );
            }
        }
        // if no result matched the duration, return an error so we skip the download
        anyhow::bail!(
            "no YouTube result within {}s of expected {}s for: {}",
            tolerance,
            expected,
            query
        );
    }

    // no duration filter, return first result's id
    let id = lines[0].split_whitespace().next().unwrap_or("").to_string();
    anyhow::ensure!(!id.is_empty(), "no YouTube result for: {}", query);
    Ok(id)
}

fn nd_url() -> &'static str {
    static URL: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
        std::env::var("UPSTREAM_URL").unwrap_or_else(|_| "http://localhost:4533".to_string())
    });
    &URL
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    // set MUSIC_DIR default for local use before utils statics are initialized
    if std::env::var("MUSIC_DIR").is_err() {
        // SAFETY: called at program start before any threads are spawned
        unsafe { std::env::set_var("MUSIC_DIR", "./music") };
    }

    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("usage: import-csv-playlist <user> [playlist_name]");
        eprintln!("example: import-csv-playlist karine \"My Playlist\"");
        std::process::exit(1);
    }

    let user = args[1].clone();
    let playlist_name = args
        .get(2)
        .cloned()
        .unwrap_or_else(|| "Imported Playlist".to_string());

    info!("starting import: user={}, playlist={}", user, playlist_name);

    let songs = match read_csv("src/bin/playlist.csv").await {
        Ok(s) => s,
        Err(e) => {
            error!("failed to read csv: {}", e);
            std::process::exit(1);
        }
    };

    let playlist_id = match find_or_create_playlist(&playlist_name).await {
        Ok(v) => v,
        Err(e) => {
            error!("failed to set up playlist: {}", e);
            std::process::exit(1);
        }
    };

    if let Err(e) = change_playlist_owner(&playlist_id, &playlist_name, &user).await {
        warn!("failed to change playlist owner: {}", e);
    }

    // full scan upfront so all files already on disk are indexed before we start searching
    info!("running initial full scan to index existing files...");
    full_scan_and_wait().await;
    info!("initial scan complete");

    let total = songs.len();
    let mut download_times: Vec<Instant> = Vec::new();
    let max_per_window = 15usize;
    let window = Duration::from_secs(300);
    let mut added_count = 0usize;
    let mut failed: Vec<(String, String, String)> = Vec::new(); // (artist, title, reason)

    for (idx, song) in songs.iter().enumerate() {
        info!(
            "processing {}/{}: {} - {}",
            idx + 1,
            total,
            song.artist,
            song.title
        );

        // check navidrome first, if the song is already indexed, add it directly
        if let Some(nd_id) = search_navidrome(&song.artist, &song.title).await {
            match add_to_playlist(&playlist_id, &nd_id).await {
                Ok(_) => {
                    info!("added to playlist: {} - {}", song.artist, song.title);
                    db::add_song(&user, &song.artist, &song.title);
                    added_count += 1;
                }
                Err(e) => {
                    warn!(
                        "failed adding {} - {} to playlist: {}",
                        song.artist, song.title, e
                    );
                    failed.push((
                        song.artist.clone(),
                        song.title.clone(),
                        format!("playlist update failed: {}", e),
                    ));
                }
            }
            continue;
        }

        // song not in navidrome yet, apply rate limit then download
        let now = Instant::now();
        download_times.retain(|t| now.duration_since(*t) < window);
        if download_times.len() >= max_per_window {
            let oldest = *download_times.iter().min().unwrap();
            let wait = window - now.duration_since(oldest);
            info!("rate limit: waiting {} seconds", wait.as_secs());
            tokio::time::sleep(wait).await;
            let now = Instant::now();
            download_times.retain(|t| now.duration_since(*t) < window);
        }
        download_times.push(Instant::now());

        // if a direct YouTube URL was provided in the CSV, use it without searching
        let yt_id = if let Some(ref url) = song.yt_url {
            match yt_id_from_url(url) {
                Some(id) => {
                    info!(
                        "using provided url for {} - {}: {}",
                        song.artist, song.title, url
                    );
                    id
                }
                None => {
                    warn!(
                        "invalid youtube url for {} - {}: {}",
                        song.artist, song.title, url
                    );
                    failed.push((
                        song.artist.clone(),
                        song.title.clone(),
                        format!("invalid youtube url: {}", url),
                    ));
                    continue;
                }
            }
        } else {
            let lfm_duration =
                nd_ytdl_proxy::lastfm::track_duration_sec(&song.artist, &song.title).await;
            if let Some(d) = lfm_duration {
                info!(
                    "lastfm duration for {} - {}: {}s",
                    song.artist, song.title, d
                );
            }

            match yt_video_id(&song.artist, &song.title, lfm_duration).await {
                Ok(id) => id,
                Err(e) => {
                    warn!(
                        "yt search failed for {} - {}: {}",
                        song.artist, song.title, e
                    );
                    failed.push((
                        song.artist.clone(),
                        song.title.clone(),
                        format!("youtube search failed: {}", e),
                    ));
                    continue;
                }
            }
        };

        // download_song handles metadata, last.fm tagging, and triggers a scan
        if let Err(e) =
            download::download_song(&yt_id, &song.artist, &song.title, &user, None).await
        {
            warn!(
                "download failed for {} - {}: {}",
                song.artist, song.title, e
            );
            failed.push((
                song.artist.clone(),
                song.title.clone(),
                format!("download failed: {}", e),
            ));
            continue;
        }

        // wait for navidrome to finish indexing before searching for the song ID
        // use a full scan so Navidrome re-indexes files with unchanged mtime
        full_scan_and_wait().await;

        match search_navidrome(&song.artist, &song.title).await {
            Some(nd_id) => match add_to_playlist(&playlist_id, &nd_id).await {
                Ok(_) => {
                    info!("added to playlist: {} - {}", song.artist, song.title);
                    db::add_song(&user, &song.artist, &song.title);
                    added_count += 1;
                }
                Err(e) => {
                    warn!(
                        "failed adding {} - {} to playlist: {}",
                        song.artist, song.title, e
                    );
                    failed.push((
                        song.artist.clone(),
                        song.title.clone(),
                        format!("playlist update failed: {}", e),
                    ));
                }
            },
            None => {
                warn!(
                    "song not indexed after download: {} - {}",
                    song.artist, song.title
                );
                failed.push((
                    song.artist.clone(),
                    song.title.clone(),
                    "not indexed after download".to_string(),
                ));
            }
        }
    }

    download::trigger_scan().await;
    info!(
        "import complete: {}/{} songs in '{}'",
        added_count, total, playlist_name
    );
    if !failed.is_empty() {
        info!("failed to import {} song(s):", failed.len());
        for (artist, title, reason) in &failed {
            info!("  [{reason}] {artist} - {title}");
        }

        let entries: Vec<Value> = failed
            .iter()
            .map(|(artist, title, reason)| {
                serde_json::json!({
                    "artist": artist,
                    "title": title,
                    "reason": reason,
                })
            })
            .collect();
        let json = serde_json::to_string_pretty(&entries).unwrap_or_default();
        let out_path = format!("import-failed-{}.json", playlist_name.replace(' ', "_"));
        match tokio::fs::write(&out_path, &json).await {
            Ok(_) => info!("failed songs written to {}", out_path),
            Err(e) => warn!("could not write failed songs file: {}", e),
        }
    }
}
