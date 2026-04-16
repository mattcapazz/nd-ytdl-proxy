use actix_web::{HttpRequest, HttpResponse, web};
use serde_json::Value;
use tracing::info;

use crate::{db, download, lastfm, utils};

pub fn subsonic_ok() -> Value {
    serde_json::json!({
        "subsonic-response": {
            "status": "ok",
            "version": "1.16.1"
        }
    })
}

fn parse_query_values(q: &str, key: &str) -> Vec<String> {
    q.split('&')
        .filter_map(|pair| {
            let mut parts = pair.splitn(2, '=');
            let k = parts.next()?;
            if k == key {
                Some(utils::url_decode(parts.next().unwrap_or("")))
            } else {
                None
            }
        })
        .collect()
}

pub async fn handle_playlist_update(
    req: HttpRequest,
    mut payload: web::Payload,
) -> actix_web::Result<HttpResponse> {
    let query = req.uri().query().unwrap_or("").to_owned();
    let path = req.uri().path();

    let mut body_bytes = web::BytesMut::new();
    while let Some(chunk) = futures_util::StreamExt::next(&mut payload).await {
        body_bytes.extend_from_slice(&chunk.map_err(actix_web::error::ErrorBadRequest)?);
    }

    let merged_query = if !body_bytes.is_empty() {
        let body_str = String::from_utf8_lossy(&body_bytes);
        info!("playlist {} body: {}", path, body_str);
        if query.is_empty() {
            body_str.to_string()
        } else {
            format!("{}&{}", query, body_str)
        }
    } else {
        query
    };

    let query_map = utils::parse_query(&merged_query);
    let playlist_id = query_map.get("playlistId").cloned().unwrap_or_default();
    if playlist_id == "delete-queue" {
        let user = query_map.get("u").cloned().unwrap_or_default();
        return handle_delete_queue_update(&user, &merged_query).await;
    }

    let url = format!("{}{path}?{merged_query}", utils::upstream_url());
    info!("forwarding playlist {} -> {}", req.method(), url);

    let resp: Value = utils::http_client()
        .get(&url)
        .send()
        .await
        .map_err(actix_web::error::ErrorBadGateway)?
        .json()
        .await
        .map_err(actix_web::error::ErrorBadGateway)?;

    info!("playlist response: {}", resp);

    Ok(HttpResponse::Ok().json(resp))
}

pub async fn handle_get_playlists(req: HttpRequest) -> actix_web::Result<HttpResponse> {
    let query = req.uri().query().unwrap_or("");
    let query_map = utils::parse_query(query);
    let user = query_map.get("u").cloned().unwrap_or_default();

    let url = format!("{}/rest/getPlaylists.view?{}", utils::upstream_url(), query);

    let mut data: Value = utils::http_client()
        .get(&url)
        .send()
        .await
        .map_err(actix_web::error::ErrorBadGateway)?
        .json()
        .await
        .map_err(actix_web::error::ErrorBadGateway)?;

    let delete_queue = serde_json::json!({
        "id": "delete-queue",
        "name": "Delete Queue",
        "comment": "",
        "songCount": 0,
        "duration": 0,
        "public": false,
        "owner": user,
        "created": "2024-01-01T00:00:00.000Z",
        "changed": "2024-01-01T00:00:00.000Z"
    });

    if let Some(playlists) = data
        .get_mut("subsonic-response")
        .and_then(|r| r.get_mut("playlists"))
        .and_then(|p| p.get_mut("playlist"))
        .and_then(|p| p.as_array_mut())
    {
        playlists.push(delete_queue);
    } else if let Some(resp) = data.get_mut("subsonic-response") {
        resp["playlists"] = serde_json::json!({
            "playlist": [delete_queue]
        });
    }

    Ok(HttpResponse::Ok().json(data))
}

pub async fn handle_get_delete_queue(req: HttpRequest) -> actix_web::Result<HttpResponse> {
    let query_map = utils::parse_query(req.uri().query().unwrap_or(""));
    let user = query_map.get("u").cloned().unwrap_or_default();

    let data = serde_json::json!({
        "subsonic-response": {
            "status": "ok",
            "version": "1.16.1",
            "playlist": {
                "id": "delete-queue",
                "name": "Delete Queue",
                "comment": "",
                "songCount": 0,
                "duration": 0,
                "public": false,
                "owner": user,
                "created": "2024-01-01T00:00:00.000Z",
                "changed": "2024-01-01T00:00:00.000Z",
                "entry": []
            }
        }
    });

    Ok(HttpResponse::Ok().json(data))
}

async fn handle_delete_queue_update(
    user: &str,
    merged_query: &str,
) -> actix_web::Result<HttpResponse> {
    let song_ids = parse_query_values(merged_query, "songIdToAdd");

    if song_ids.is_empty() {
        return Ok(HttpResponse::Ok().json(subsonic_ok()));
    }

    let auth = utils::admin_auth_query();
    let mut needs_scan = false;
    let mut nd_ids_to_remove: Vec<String> = Vec::new();

    for song_id in &song_ids {
        let (artist, title, album_id, is_nd) = if song_id.starts_with("yt_") {
            match lastfm::decode_track_id(song_id) {
                Some((a, t)) => (a, t, String::new(), false),
                None => continue,
            }
        } else {
            let url = format!(
                "{}/rest/getSong.view?{}&id={}",
                utils::upstream_url(),
                auth,
                song_id
            );
            let data: Value = utils::http_client()
                .get(&url)
                .send()
                .await
                .map_err(actix_web::error::ErrorBadGateway)?
                .json()
                .await
                .map_err(actix_web::error::ErrorBadGateway)?;

            let song = &data["subsonic-response"]["song"];
            let a = song["artist"].as_str().unwrap_or("").to_string();
            let mut t = song["title"].as_str().unwrap_or("").to_string();
            let al = song["albumId"].as_str().unwrap_or("").to_string();
            if a.is_empty() || t.is_empty() {
                continue;
            }
            if let Some(stripped) = t.strip_prefix(&a).and_then(|s| s.strip_prefix(" - ")) {
                t = stripped.to_string();
            }
            (a, t, al, true)
        };

        info!(
            "delete queue: trashing '{}' - '{}' for user '{}'",
            artist, title, user
        );
        db::trash_song(user, &artist, &title, &album_id);

        if !db::song_owned_by_others(user, &artist, &title) {
            info!(
                "delete queue: no other owners, deleting '{}' - '{}'",
                artist, title
            );
            download::delete_song_file(&artist, &title);
            if is_nd {
                nd_ids_to_remove.push(song_id.clone());
            }
            needs_scan = true;
        }
    }

    db::navidrome_delete_songs(&nd_ids_to_remove);

    if needs_scan {
        download::trigger_scan().await;
    }

    Ok(HttpResponse::Ok().json(subsonic_ok()))
}
