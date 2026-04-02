mod db;
mod download;
mod lastfm;
mod metadata;
mod proxy;
mod search;
mod title;
mod utils;
mod youtube;

use actix_web::{App, HttpRequest, HttpServer, web};
use serde_json::Value;
use tracing::info;
use utils::parse_query;

async fn handler(
    req: HttpRequest,
    payload: web::Payload,
) -> actix_web::Result<actix_web::HttpResponse> {
    info!("{} {}", req.method(), req.uri());

    let query_map = parse_query(req.uri().query().unwrap_or(""));
    let id = query_map.get("id").map(String::as_str).unwrap_or("");

    match req.uri().path() {
        "/rest/getAlbum.view" | "/rest/getAlbum" if id.starts_with("yt_") => {
            youtube::handle_get_album(req).await
        }
        "/rest/getAlbum.view" | "/rest/getAlbum" => {
            handle_filtered(req, payload, filter_get_album).await
        }
        "/rest/getAlbumList.view" | "/rest/getAlbumList" => {
            handle_filtered(req, payload, filter_get_album_list).await
        }
        "/rest/getAlbumList2.view" | "/rest/getAlbumList2" => {
            handle_filtered(req, payload, filter_get_album_list2).await
        }
        "/rest/getArtists.view" | "/rest/getArtists" => {
            handle_filtered(req, payload, filter_get_artists).await
        }
        "/rest/getCoverArt" | "/rest/getCoverArt.view" if id.starts_with("yt_") => {
            youtube::handle_cover_art(req).await
        }
        "/rest/getGenres.view" | "/rest/getGenres" => handle_get_genres(req).await,
        "/rest/search3.view" => search::handle(req).await,
        "/rest/stream" | "/rest/stream.view" if id.starts_with("yt_") => {
            youtube::handle_stream(req).await
        }
        "/rest/scrobble.view" | "/rest/scrobble" if id.starts_with("yt_") => {
            youtube::handle_scrobble(req).await
        }
        "/rest/stream" | "/rest/stream.view" => handle_nd_stream(req, payload).await,
        "/rest/updatePlaylist.view"
        | "/rest/updatePlaylist"
        | "/rest/createPlaylist.view"
        | "/rest/createPlaylist" => handle_playlist_update(req, payload).await,
        _ => proxy::forward(req, payload).await,
    }
}

async fn handle_get_genres(req: HttpRequest) -> actix_web::Result<actix_web::HttpResponse> {
    let query = req.uri().query().unwrap_or("");
    let url = format!("{}/rest/getGenres.view?{}", utils::upstream_url(), query);

    let mut data: Value = utils::http_client()
        .get(&url)
        .send()
        .await
        .map_err(actix_web::error::ErrorBadGateway)?
        .json()
        .await
        .map_err(actix_web::error::ErrorBadGateway)?;

    if let Some(genres) = data["subsonic-response"]["genres"]["genre"].as_array_mut() {
        genres.retain(|g| {
            g["value"]
                .as_str()
                .map(|v| !v.eq_ignore_ascii_case("music"))
                .unwrap_or(true)
        });
    }

    Ok(actix_web::HttpResponse::Ok().json(data))
}

// only deserialize + filter when the user has library entries, otherwise raw proxy
async fn handle_filtered(
    req: HttpRequest,
    payload: web::Payload,
    filter_fn: fn(&str, &mut Value),
) -> actix_web::Result<actix_web::HttpResponse> {
    let query_map = parse_query(req.uri().query().unwrap_or(""));
    let user = query_map.get("u").cloned().unwrap_or_default();

    if user.is_empty() || !db::has_any(&user) {
        return proxy::forward(req, payload).await;
    }

    let query = req.uri().query().unwrap_or("");
    let path = req.uri().path();
    let url = format!("{}{path}?{query}", utils::upstream_url());

    let mut data: Value = utils::http_client()
        .get(&url)
        .send()
        .await
        .map_err(actix_web::error::ErrorBadGateway)?
        .json()
        .await
        .map_err(actix_web::error::ErrorBadGateway)?;

    filter_fn(&user, &mut data);

    Ok(actix_web::HttpResponse::Ok().json(data))
}

fn filter_get_album(user: &str, data: &mut Value) {
    let allowed = db::get_artists(user);
    if let Some(song_list) = data["subsonic-response"]["album"]["song"].as_array_mut() {
        song_list.retain(|s| {
            let artist = s["artist"].as_str().unwrap_or("");
            allowed.iter().any(|lib| lib.eq_ignore_ascii_case(artist))
        });
    }
}

fn filter_get_artists(user: &str, data: &mut Value) {
    let allowed = db::get_artists(user);
    if let Some(indexes) = data["subsonic-response"]["artists"]["index"].as_array_mut() {
        for index in indexes.iter_mut() {
            if let Some(artists) = index["artist"].as_array_mut() {
                artists.retain(|a| {
                    a["name"]
                        .as_str()
                        .map(|n| allowed.iter().any(|lib| lib.eq_ignore_ascii_case(n)))
                        .unwrap_or(false)
                });
            }
        }
        indexes.retain(|idx| {
            idx["artist"]
                .as_array()
                .map(|a| !a.is_empty())
                .unwrap_or(false)
        });
    }
}

fn filter_get_album_list(user: &str, data: &mut Value) {
    let allowed = db::get_artists(user);
    if let Some(albums) = data["subsonic-response"]["albumList"]["album"].as_array_mut() {
        albums.retain(|a| {
            let artist = a["artist"].as_str().unwrap_or("");
            allowed.iter().any(|lib| lib.eq_ignore_ascii_case(artist))
        });
    }
}

fn filter_get_album_list2(user: &str, data: &mut Value) {
    let allowed = db::get_artists(user);
    if let Some(albums) = data["subsonic-response"]["albumList2"]["album"].as_array_mut() {
        albums.retain(|a| {
            let artist = a["artist"].as_str().unwrap_or("");
            allowed.iter().any(|lib| lib.eq_ignore_ascii_case(artist))
        });
    }
}

async fn handle_nd_stream(
    req: HttpRequest,
    payload: web::Payload,
) -> actix_web::Result<actix_web::HttpResponse> {
    let query = req.uri().query().unwrap_or("");
    let query_map = parse_query(query);
    let user = query_map.get("u").cloned().unwrap_or_default();
    let id = query_map.get("id").cloned().unwrap_or_default();

    // look up the song from navidrome and record it in the user's library
    if !user.is_empty() && !id.is_empty() {
        let q = query.to_string();
        tokio::spawn(async move {
            let url = format!(
                "{}/rest/getSong.view?{}&id={}",
                utils::upstream_url(),
                q,
                id
            );
            if let Ok(resp) = utils::http_client().get(&url).send().await {
                if let Ok(data) = resp.json::<Value>().await {
                    let song = &data["subsonic-response"]["song"];
                    let artist = song["artist"].as_str().unwrap_or("");
                    let title = song["title"].as_str().unwrap_or("");
                    db::add_song(&user, artist, title);
                }
            }
        });
    }

    proxy::forward(req, payload).await
}

async fn handle_playlist_update(
    req: HttpRequest,
    mut payload: web::Payload,
) -> actix_web::Result<actix_web::HttpResponse> {
    let query = req.uri().query().unwrap_or("").to_owned();
    let path = req.uri().path();

    // collect any POST body (some clients send params as form data)
    let mut body_bytes = web::BytesMut::new();
    while let Some(chunk) = futures_util::StreamExt::next(&mut payload).await {
        body_bytes.extend_from_slice(&chunk.map_err(actix_web::error::ErrorBadRequest)?);
    }

    // merge body form params into the query string so navidrome always sees them
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

    Ok(actix_web::HttpResponse::Ok().json(resp))
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    tracing_subscriber::fmt::init();

    let addr = ("0.0.0.0", 4532);
    info!("proxy running on http://{}:{}", addr.0, addr.1);

    HttpServer::new(|| App::new().default_service(web::route().to(handler)))
        .bind(addr)?
        .run()
        .await
}
