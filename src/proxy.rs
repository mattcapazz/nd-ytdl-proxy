use actix_web::{HttpRequest, HttpResponse, web};
use awc::Client;
use futures_util::StreamExt;
use tracing::info;

use crate::utils::upstream_url;

pub async fn forward(req: HttpRequest, payload: web::Payload) -> actix_web::Result<HttpResponse> {
    let client = Client::new();

    let path = req.uri().path();
    let query = req.uri().query().unwrap_or("");

    let base = upstream_url();
    let url = if query.is_empty() {
        format!("{}{}", base, path)
    } else {
        format!("{}{}?{}", base, path, query)
    };

    let mut upstream = client.request(req.method().clone(), url.as_str());

    for (name, value) in req.headers().iter() {
        let n = name.as_str();
        if n.eq_ignore_ascii_case("connection")
            || n.eq_ignore_ascii_case("content-length")
            || n.eq_ignore_ascii_case("accept-encoding")
        {
            continue;
        }
        upstream = upstream.insert_header((name.clone(), value.clone()));
    }

    upstream = upstream.insert_header(("Accept-Encoding", "identity"));

    if let Some(peer) = req.peer_addr() {
        upstream = upstream.insert_header(("X-Forwarded-For", peer.ip().to_string()));
    }

    upstream = upstream.insert_header(("X-Forwarded-Proto", req.connection_info().scheme()));

    info!("forwarding {} -> {}", req.method(), url);

    let resp = upstream
        .send_stream(
            payload.map(|c| c.map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))),
        )
        .await
        .map_err(actix_web::error::ErrorBadGateway)?;

    let mut client_resp = HttpResponse::build(resp.status());

    for (name, value) in resp.headers().iter() {
        if name.as_str().eq_ignore_ascii_case("transfer-encoding") {
            continue;
        }
        client_resp.append_header((name.clone(), value.clone()));
    }

    Ok(client_resp.streaming(resp))
}
