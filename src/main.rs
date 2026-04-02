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
        "/rest/search3.view" => search::handle(req).await,
        "/rest/stream" | "/rest/stream.view" if id.starts_with("yt_") => {
            youtube::handle_stream(req).await
        }
        "/rest/getCoverArt" | "/rest/getCoverArt.view" if id.starts_with("yt_") => {
            youtube::handle_cover_art(req).await
        }
        "/rest/getAlbum.view" | "/rest/getAlbum" if id.starts_with("yt_") => {
            youtube::handle_get_album(req).await
        }
        "/rest/scrobble.view" | "/rest/scrobble" if id.starts_with("yt_") => {
            youtube::handle_scrobble(req).await
        }
        "/rest/getGenres.view" | "/rest/getGenres" => handle_get_genres(req).await,
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
