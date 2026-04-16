use std::collections::HashMap;

use actix_web::{App, HttpRequest, HttpServer, web};
use nd_ytdl_proxy::{db, filters, playlist, proxy, search, utils, youtube};
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
        "/rest/createPlaylist"
        | "/rest/createPlaylist.view"
        | "/rest/updatePlaylist"
        | "/rest/updatePlaylist.view" => playlist::handle_playlist_update(req, payload).await,
        "/rest/getAlbum.view" | "/rest/getAlbum" if id.starts_with("yt_") => {
            youtube::handle_get_album(req).await
        }
        "/rest/getAlbum.view" | "/rest/getAlbum" => {
            handle_filtered(req, payload, filters::filter_get_album).await
        }
        "/rest/getAlbumList.view" | "/rest/getAlbumList" => {
            handle_filtered(req, payload, filters::filter_get_album_list).await
        }
        "/rest/getAlbumList2.view" | "/rest/getAlbumList2" => {
            handle_filtered(req, payload, filters::filter_get_album_list2).await
        }
        "/rest/getArtist.view" | "/rest/getArtist" if id.starts_with("yt_artist_") => {
            youtube::handle_get_artist(req).await
        }
        "/rest/getArtists.view" | "/rest/getArtists" => {
            handle_filtered(req, payload, filters::filter_get_artists).await
        }
        "/rest/getCoverArt" | "/rest/getCoverArt.view" if id.starts_with("yt_") => {
            youtube::handle_cover_art(req).await
        }
        "/rest/getGenres.view" | "/rest/getGenres" => handle_get_genres(req).await,
        "/rest/getPlaylists.view" | "/rest/getPlaylists" => {
            playlist::handle_get_playlists(req).await
        }
        "/rest/getPlaylist.view" | "/rest/getPlaylist" if id == "delete-queue" => {
            playlist::handle_get_delete_queue(req).await
        }
        "/rest/search3.view" => search::handle(req).await,
        "/rest/scrobble.view" | "/rest/scrobble" if id.starts_with("yt_") => {
            youtube::handle_scrobble(req).await
        }
        "/rest/stream" | "/rest/stream.view" if id.starts_with("yt_") => {
            youtube::handle_stream(req).await
        }
        "/rest/stream" | "/rest/stream.view" => handle_nd_stream(req, payload).await,
        "/rest/deletePlaylist.view" | "/rest/deletePlaylist" if id == "delete-queue" => {
            Ok(actix_web::HttpResponse::Ok().json(playlist::subsonic_ok()))
        }
        _ => proxy::forward(req, payload).await,
    }
}

async fn handle_get_genres(req: HttpRequest) -> actix_web::Result<actix_web::HttpResponse> {
    let query = req.uri().query().unwrap_or("");
    let query_map = parse_query(query);
    let user = query_map.get("u").cloned().unwrap_or_default();

    let url = format!("{}/rest/getGenres.view?{}", utils::upstream_url(), query);

    let mut data: Value = utils::http_client()
        .get(&url)
        .send()
        .await
        .map_err(actix_web::error::ErrorBadGateway)?
        .json()
        .await
        .map_err(actix_web::error::ErrorBadGateway)?;

    if !user.is_empty() && db::has_any(&user) {
        let allowed = db::get_artists(&user);
        let albums_url = format!(
            "{}/rest/getAlbumList2.view?{}&type=alphabeticalByName&size=500",
            utils::upstream_url(),
            query
        );

        let albums_data: Value = utils::http_client()
            .get(&albums_url)
            .send()
            .await
            .map_err(actix_web::error::ErrorBadGateway)?
            .json()
            .await
            .map_err(actix_web::error::ErrorBadGateway)?;

        let mut album_counts: HashMap<String, u64> = HashMap::new();
        let mut song_counts: HashMap<String, u64> = HashMap::new();

        if let Some(albums) = albums_data
            .get("subsonic-response")
            .and_then(|r| r.get("albumList2"))
            .and_then(|al| al.get("album"))
            .and_then(|a| a.as_array())
        {
            for album in albums {
                let artist = album["artist"].as_str().unwrap_or("");
                if !filters::artist_allowed(artist, &allowed) {
                    continue;
                }
                let genre = album["genre"].as_str().unwrap_or("");
                if genre.is_empty() || genre.eq_ignore_ascii_case("music") {
                    continue;
                }
                *album_counts.entry(genre.to_string()).or_default() += 1;
                *song_counts.entry(genre.to_string()).or_default() +=
                    album["songCount"].as_u64().unwrap_or(0);
            }
        }

        let genres: Vec<Value> = album_counts
            .iter()
            .map(|(name, &ac)| {
                serde_json::json!({
                    "value": name,
                    "albumCount": ac,
                    "songCount": song_counts.get(name).copied().unwrap_or(0),
                })
            })
            .collect();

        if let Some(genre_arr) = data
            .get_mut("subsonic-response")
            .and_then(|r| r.get_mut("genres"))
            .and_then(|g| g.get_mut("genre"))
        {
            *genre_arr = Value::Array(genres);
        }
    } else {
        if let Some(genres) = data
            .get_mut("subsonic-response")
            .and_then(|r| r.get_mut("genres"))
            .and_then(|g| g.get_mut("genre"))
            .and_then(|g| g.as_array_mut())
        {
            genres.retain(|g| {
                g["value"]
                    .as_str()
                    .map(|v| !v.eq_ignore_ascii_case("music"))
                    .unwrap_or(true)
            });
        }
    }

    Ok(actix_web::HttpResponse::Ok().json(data))
}

async fn handle_filtered(
    req: HttpRequest,
    payload: web::Payload,
    filter_fn: fn(&str, &mut Value),
) -> actix_web::Result<actix_web::HttpResponse> {
    let query_map = parse_query(req.uri().query().unwrap_or(""));
    let user = query_map.get("u").cloned().unwrap_or_default();

    if user.is_empty() || (!db::has_any(&user) && !db::has_trashed(&user)) {
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

async fn handle_nd_stream(
    req: HttpRequest,
    payload: web::Payload,
) -> actix_web::Result<actix_web::HttpResponse> {
    let query = req.uri().query().unwrap_or("");
    let query_map = parse_query(query);
    let user = query_map.get("u").cloned().unwrap_or_default();
    let id = query_map.get("id").cloned().unwrap_or_default();

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
