use actix_files::NamedFile;
use actix_web::{HttpRequest, HttpResponse};
use futures_util::TryStreamExt;
use tokio::process::Command;
use tracing::{info, warn};

use deunicode::deunicode;
use serde_json::Value as JsonValue;

use crate::utils::{http_client, parse_query};

pub async fn handle_stream(req: HttpRequest) -> actix_web::Result<HttpResponse> {
    let query_map = parse_query(req.uri().query().unwrap_or(""));
    let raw_id = query_map.get("id").cloned().unwrap_or_default();

    let (artist, title) = crate::lastfm::decode_track_id(&raw_id)
        .ok_or_else(|| actix_web::error::ErrorBadRequest("invalid track ID"))?;

    info!("yt stream: {} - {}", artist, title);

    let user = query_map.get("u").cloned().unwrap_or_default();
    crate::db::add_song(&user, &artist, &title);

    // check if the file is already on disk and serve it directly, avoids YouTube CDN issues
    let stripped = crate::utils::strip_artist_prefix(&artist, &title);
    let safe_title = crate::utils::sanitize_filename(&stripped);
    let artist_dir = crate::utils::find_artist_dir(&crate::utils::music_dir(), &artist);
    let dest = format!("{}/{}.opus", artist_dir, safe_title);

    if std::path::Path::new(&dest).exists() {
        info!("yt stream (local): {} - {}", artist, title);
        let dest_bg = dest.clone();
        let artist_bg = artist.clone();
        tokio::spawn(async move {
            // fix artist tag if navidrome indexed it with wrong metadata
            let (_, _, cur_artist) = crate::metadata::read_tags(&dest_bg).await;
            let artist_ok = cur_artist
                .as_deref()
                .map(|a| a.eq_ignore_ascii_case(&artist_bg))
                .unwrap_or(false);
            if !artist_ok {
                warn!(
                    "fixing artist tag in {}: was {:?}, should be {}",
                    dest_bg, cur_artist, artist_bg
                );
                let _ = crate::metadata::write_tags(&dest_bg, "", &[], &artist_bg, "", "", None).await;
            }
            crate::download::trigger_scan().await;
        });
        let file = NamedFile::open_async(&dest).await?;
        return Ok(file.into_response(&req));
    }

    let search_query = format!("{} - {}", artist, title);
    // get both video ID and stream URL
    let (_video_id, url) = yt_search_and_url(&search_query).await.map_err(|e| {
        warn!("yt search+url error: {}", e);
        actix_web::error::ErrorInternalServerError("failed to find track on YouTube")
    })?;

    info!("yt stream URL obtained for {} - {}", artist, title);

    let artist_owned = artist.clone();
    let title_owned = title.clone();
    let raw_query = req.uri().query().unwrap_or("").to_string();
    let user_owned = user.clone();
    // spawn background task to find best match (uploader or Released on:) and download
    tokio::spawn(async move {
        match yt_search_one(&search_query).await {
            Ok((best_id, yt_meta)) => {
                if let Err(e) = crate::download::download_and_scan(
                    &best_id,
                    &artist_owned,
                    &title_owned,
                    &raw_query,
                    &user_owned,
                    yt_meta,
                )
                .await
                {
                    warn!("background download failed: {}", e);
                }
            }
            Err(e) => warn!("background search failed: {}", e),
        }
    });

    let mut req_builder = http_client().get(&url);
    if let Some(range) = req.headers().get("range") {
        if let Ok(v) = range.to_str() {
            req_builder = req_builder.header(reqwest::header::RANGE, v);
        }
    }

    let resp = req_builder
        .send()
        .await
        .map_err(actix_web::error::ErrorBadGateway)?;

    let status = actix_web::http::StatusCode::from_u16(resp.status().as_u16()).unwrap();

    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("audio/webm")
        .to_string();

    let mut builder = HttpResponse::build(status);
    builder.content_type(content_type.as_str());
    builder.append_header(("Accept-Ranges", "bytes"));

    if let Some(cl) = resp.headers().get(reqwest::header::CONTENT_LENGTH) {
        if let Ok(v) = cl.to_str() {
            builder.append_header(("Content-Length", v));
        }
    }
    if let Some(cr) = resp.headers().get(reqwest::header::CONTENT_RANGE) {
        if let Ok(v) = cr.to_str() {
            builder.append_header(("Content-Range", v));
        }
    }

    let stream = resp
        .bytes_stream()
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e));

    Ok(builder.streaming(stream))
}

// single yt-dlp call: search for a video and return both ID and stream URL
async fn yt_search_and_url(query: &str) -> anyhow::Result<(String, String)> {
    let output = Command::new("yt-dlp")
        .args([
            &format!("ytsearch1:{}", query),
            "-f",
            "bestaudio/best",
            "--print",
            "id",
            "--print",
            "urls",
            "--no-playlist",
        ])
        .output()
        .await?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut lines = stdout.lines().filter(|l| !l.trim().is_empty());

    let id = lines.next().unwrap_or("").trim().to_string();
    let url = lines.next().unwrap_or("").trim().to_string();

    anyhow::ensure!(!id.is_empty(), "no YouTube result found for: {}", query);
    anyhow::ensure!(!url.is_empty(), "yt-dlp returned no URL for: {}", query);
    Ok((id, url))
}

async fn yt_search_one(query: &str) -> anyhow::Result<(String, Option<JsonValue>)> {
    // request top 5 search results as JSON and prefer one where uploader matches the artist
    let search_arg = format!("ytsearch5:{}", query);
    let output = Command::new("yt-dlp")
        .args([&search_arg, "--dump-json", "--no-playlist"])
        .output()
        .await?;

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    if stdout.trim().is_empty() {
        anyhow::bail!("no YouTube result found for: {}", query);
    }

    // parse newline-delimited JSON objects
    let mut candidates: Vec<JsonValue> = Vec::new();
    for line in stdout.lines() {
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(val) = serde_json::from_str::<JsonValue>(line) {
            candidates.push(val);
        }
    }

    // normalize artist from query (expect 'Artist - Title')
    let artist_part = query.split('-').next().unwrap_or(query).trim();
    let norm_artist = deunicode(artist_part).to_lowercase();

    // prefer candidate where uploader matches artist name
    for c in &candidates {
        let uploader = c["uploader"].as_str().unwrap_or("");
        let norm_uploader = deunicode(uploader).to_lowercase();
        if norm_uploader == norm_artist
            || norm_uploader.contains(&norm_artist)
            || norm_artist.contains(&norm_uploader)
        {
            if let Some(id) = c["id"].as_str() {
                return Ok((id.to_string(), Some(c.clone())));
            }
        }
    }

    // prefer candidate whose description contains 'Released on:'
    for c in &candidates {
        if let Some(desc) = c["description"].as_str() {
            if desc.to_lowercase().contains("released on:") {
                if let Some(id) = c["id"].as_str() {
                    return Ok((id.to_string(), Some(c.clone())));
                }
            }
        }
    }

    // fallback to first candidate id
    if let Some(first) = candidates.first() {
        if let Some(id) = first["id"].as_str() {
            return Ok((id.to_string(), Some(first.clone())));
        }
    }

    anyhow::bail!("no YouTube result found for: {}", query);
}

pub async fn handle_cover_art(req: HttpRequest) -> actix_web::Result<HttpResponse> {
    let query_map = parse_query(req.uri().query().unwrap_or(""));
    let raw_id = query_map.get("id").cloned().unwrap_or_default();

    let image_url = if let Some(url) = crate::lastfm::get_cached_cover(&raw_id) {
        url
    } else if let Some((artist, title)) = crate::lastfm::decode_track_id(&raw_id) {
        let (_, image_url, _, _) = crate::lastfm::lookup(&artist, &title).await;
        match image_url {
            Some(url) => {
                crate::lastfm::cache_cover(&raw_id, &url);
                url
            }
            None => return Ok(HttpResponse::NotFound().finish()),
        }
    } else {
        return Ok(HttpResponse::NotFound().finish());
    };

    let resp = http_client()
        .get(&image_url)
        .send()
        .await
        .map_err(actix_web::error::ErrorBadGateway)?;

    let status = actix_web::http::StatusCode::from_u16(resp.status().as_u16()).unwrap();

    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("image/jpeg")
        .to_string();

    let stream = resp
        .bytes_stream()
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e));

    Ok(HttpResponse::build(status)
        .content_type(content_type.as_str())
        .streaming(stream))
}

pub async fn handle_get_artist(req: HttpRequest) -> actix_web::Result<HttpResponse> {
    let query_map = parse_query(req.uri().query().unwrap_or(""));
    let raw_id = query_map.get("id").cloned().unwrap_or_default();
    let artist_name = raw_id
        .strip_prefix("yt_artist_")
        .unwrap_or(&raw_id)
        .to_string();
    let query = req.uri().query().unwrap_or("");

    // build list of names to try: individual parts first, then combined, then display form
    let parts = crate::utils::split_artists(&artist_name);
    let mut candidates: Vec<String> = Vec::new();
    if parts.len() > 1 {
        candidates.extend(parts);
    }
    candidates.push(artist_name.clone());
    let display = crate::utils::artist_display_name(&artist_name);
    if !display.eq_ignore_ascii_case(&artist_name) {
        candidates.push(display);
    }

    // try each candidate name against Navidrome
    for name in &candidates {
        let search_url = format!(
            "{}/rest/search3.view?{}&query={}&artistCount=10&albumCount=0&songCount=0",
            crate::utils::upstream_url(),
            query,
            crate::utils::url_encode_param(name)
        );

        let resp = match http_client().get(&search_url).send().await {
            Ok(r) => r,
            Err(_) => continue,
        };
        let data: serde_json::Value = match resp.json().await {
            Ok(d) => d,
            Err(_) => continue,
        };
        let arr = match data["subsonic-response"]["searchResult3"]["artist"].as_array() {
            Some(a) => a,
            None => continue,
        };

        if let Some(found) = arr.iter().find(|a| {
            a["name"]
                .as_str()
                .map(|n| n.eq_ignore_ascii_case(name))
                .unwrap_or(false)
        }) {
            if let Some(real_id) = found["id"].as_str() {
                let artist_url = format!(
                    "{}/rest/getArtist.view?{}&id={}",
                    crate::utils::upstream_url(),
                    query,
                    real_id
                );
                if let Ok(real_resp) = http_client().get(&artist_url).send().await {
                    if let Ok(real_data) = real_resp.json::<serde_json::Value>().await {
                        if real_data["subsonic-response"]["status"].as_str() == Some("ok") {
                            return Ok(HttpResponse::Ok().json(real_data));
                        }
                    }
                }
            }
        }
    }

    // fallback: return a minimal stub so the client doesn't error
    Ok(HttpResponse::Ok().json(serde_json::json!({
        "subsonic-response": {
            "status": "ok",
            "version": "1.16.1",
            "artist": {
                "id": raw_id,
                "name": artist_name,
                "albumCount": 0,
                "album": []
            }
        }
    })))
}

pub async fn handle_get_album(_req: HttpRequest) -> actix_web::Result<HttpResponse> {
    Ok(HttpResponse::Ok().json(serde_json::json!({
        "subsonic-response": {
            "status": "ok",
            "version": "1.16.1",
            "album": {
                "id": "yt_album_lastfm",
                "name": "Last.fm",
                "artist": "Last.fm",
                "artistId": "yt_artist_lastfm",
                "coverArt": "yt_lastfm_logo",
                "songCount": 0,
                "duration": 0,
                "playCount": 0,
                "created": "2024-01-01T00:00:00Z",
                "genres": [],
                "artists": [],
                "displayArtist": "Last.fm",
                "song": []
            }
        }
    })))
}

pub async fn handle_scrobble(_req: HttpRequest) -> actix_web::Result<HttpResponse> {
    Ok(HttpResponse::Ok().json(serde_json::json!({
        "subsonic-response": {
            "status": "ok",
            "version": "1.16.1"
        }
    })))
}
