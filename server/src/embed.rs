//! Serve the React UI embedded in the binary (rust-embed), with SPA fallback.
use axum::{
    http::{header, StatusCode, Uri},
    response::{IntoResponse, Response},
};
use rust_embed::RustEmbed;

#[derive(RustEmbed)]
#[folder = "ui/dist"]
struct Assets;

pub async fn static_handler(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };

    if let Some(content) = Assets::get(path) {
        let mime = mime_guess::from_path(path).first_or_octet_stream();
        return ([(header::CONTENT_TYPE, mime.as_ref())], content.data.into_owned()).into_response();
    }
    // Unknown path that isn't an asset -> SPA fallback to index.html.
    match Assets::get("index.html") {
        Some(c) => ([(header::CONTENT_TYPE, "text/html")], c.data.into_owned()).into_response(),
        None => (StatusCode::NOT_FOUND, "UI not built (run `npm run build` in server/ui)").into_response(),
    }
}
