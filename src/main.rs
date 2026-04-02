mod db;
mod download;
mod lastfm;
mod metadata;
mod proxy;
mod search;
mod title;
mod utils;
mod youtube;

use std::collections::HashMap;

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
        // recount genres from only the albums belonging to this user's artists
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
                if !allowed.iter().any(|lib| lib.eq_ignore_ascii_case(artist)) {
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
    let should_remove = {
        let songs = data
            .get_mut("subsonic-response")
            .and_then(|r| r.get_mut("album"))
            .and_then(|a| a.get_mut("song"))
            .and_then(|s| s.as_array_mut());
        if let Some(song_list) = songs {
            song_list.retain(|s| {
                let artist = s["artist"].as_str().unwrap_or("");
                allowed.iter().any(|lib| lib.eq_ignore_ascii_case(artist))
            });
            song_list.is_empty()
        } else {
            false
        }
    };
    if should_remove {
        if let Some(album) = data
            .get_mut("subsonic-response")
            .and_then(|r| r.get_mut("album"))
            .and_then(|a| a.as_object_mut())
        {
            album.remove("song");
        }
    }
}

fn filter_get_artists(user: &str, data: &mut Value) {
    let allowed = db::get_artists(user);
    let indexes = data
        .get_mut("subsonic-response")
        .and_then(|r| r.get_mut("artists"))
        .and_then(|a| a.get_mut("index"))
        .and_then(|i| i.as_array_mut());
    if let Some(indexes) = indexes {
        for index in indexes.iter_mut() {
            if let Some(artists) = index.get_mut("artist").and_then(|a| a.as_array_mut()) {
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
    let should_remove = {
        let albums = data
            .get_mut("subsonic-response")
            .and_then(|r| r.get_mut("albumList"))
            .and_then(|al| al.get_mut("album"))
            .and_then(|a| a.as_array_mut());
        if let Some(albums) = albums {
            albums.retain(|a| {
                let artist = a["artist"].as_str().unwrap_or("");
                allowed.iter().any(|lib| lib.eq_ignore_ascii_case(artist))
            });
            albums.is_empty()
        } else {
            false
        }
    };
    if should_remove {
        if let Some(list) = data
            .get_mut("subsonic-response")
            .and_then(|r| r.get_mut("albumList"))
            .and_then(|al| al.as_object_mut())
        {
            list.remove("album");
        }
    }
}

fn filter_get_album_list2(user: &str, data: &mut Value) {
    let allowed = db::get_artists(user);
    let should_remove = {
        let albums = data
            .get_mut("subsonic-response")
            .and_then(|r| r.get_mut("albumList2"))
            .and_then(|al| al.get_mut("album"))
            .and_then(|a| a.as_array_mut());
        if let Some(albums) = albums {
            albums.retain(|a| {
                let artist = a["artist"].as_str().unwrap_or("");
                allowed.iter().any(|lib| lib.eq_ignore_ascii_case(artist))
            });
            albums.is_empty()
        } else {
            false
        }
    };
    if should_remove {
        if let Some(list) = data
            .get_mut("subsonic-response")
            .and_then(|r| r.get_mut("albumList2"))
            .and_then(|al| al.as_object_mut())
        {
            list.remove("album");
        }
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
