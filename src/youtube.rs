use actix_web::{HttpRequest, HttpResponse};
use futures_util::TryStreamExt;
use tokio::process::Command;
use tracing::{info, warn};

use crate::utils::{http_client, parse_query};

pub async fn handle_stream(req: HttpRequest) -> actix_web::Result<HttpResponse> {
    let query_map = parse_query(req.uri().query().unwrap_or(""));
    let raw_id = query_map.get("id").cloned().unwrap_or_default();

    let (artist, title) = crate::lastfm::decode_track_id(&raw_id)
        .ok_or_else(|| actix_web::error::ErrorBadRequest("invalid track ID"))?;

    info!("yt stream: {} - {}", artist, title);

    let user = query_map.get("u").cloned().unwrap_or_default();
    crate::db::add_song(&user, &artist, &title);

    let search_query = format!("{} - {}", artist, title);
    let video_id = yt_search_one(&search_query).await.map_err(|e| {
        warn!("yt search error: {}", e);
        actix_web::error::ErrorInternalServerError("failed to find track on YouTube")
    })?;

    let url = yt_get_url(&video_id).await.map_err(|e| {
        warn!("yt-dlp error: {}", e);
        actix_web::error::ErrorInternalServerError("yt-dlp failed to get stream URL")
    })?;

    info!("yt stream URL obtained for {} - {}", artist, title);

    let artist_owned = artist.clone();
    let title_owned = title.clone();
    let vid = video_id.clone();
    let raw_query = req.uri().query().unwrap_or("").to_string();
    let user_owned = user.clone();
    tokio::spawn(async move {
        if let Err(e) = crate::download::download_and_scan(
            &vid,
            &artist_owned,
            &title_owned,
            &raw_query,
            &user_owned,
        )
        .await
        {
            warn!("background download failed: {}", e);
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

async fn yt_get_url(video_id: &str) -> anyhow::Result<String> {
    let output = Command::new("yt-dlp")
        .args(["-f", "bestaudio/best", "-g"])
        .arg(format!("https://youtu.be/{}", video_id))
        .output()
        .await?;

    let url = String::from_utf8_lossy(&output.stdout)
        .lines()
        .next()
        .unwrap_or("")
        .trim()
        .to_string();

    anyhow::ensure!(!url.is_empty(), "yt-dlp returned no URL");
    Ok(url)
}

async fn yt_search_one(query: &str) -> anyhow::Result<String> {
    let output = Command::new("yt-dlp")
        .args([&format!("ytsearch1:{}", query), "--get-id", "--no-playlist"])
        .output()
        .await?;

    let id = String::from_utf8_lossy(&output.stdout)
        .lines()
        .next()
        .unwrap_or("")
        .trim()
        .to_string();

    anyhow::ensure!(!id.is_empty(), "no YouTube result found for: {}", query);
    Ok(id)
}

pub async fn handle_cover_art(req: HttpRequest) -> actix_web::Result<HttpResponse> {
    let query_map = parse_query(req.uri().query().unwrap_or(""));
    let raw_id = query_map.get("id").cloned().unwrap_or_default();

    let image_url = if let Some(url) = crate::lastfm::get_cached_cover(&raw_id) {
        url
    } else if let Some((artist, title)) = crate::lastfm::decode_track_id(&raw_id) {
        let (_, image_url, _) = crate::lastfm::lookup(&artist, &title).await;
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
