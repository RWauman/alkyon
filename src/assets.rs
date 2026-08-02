use axum::http::{header, HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use rust_embed::RustEmbed;

/// The web UI, compiled into the binary. Distribution is the only difference
/// between the Tauri build and the Docker build — both serve these bytes.
#[derive(RustEmbed)]
#[folder = "src/ui/"]
struct Assets;

/// The brand assets live outside `src/ui/` because they are also the source for
/// the README and the installer icons.
#[derive(RustEmbed)]
#[folder = "logo/"]
struct Logo;

pub async fn serve(uri: Uri, headers: HeaderMap) -> Response {
    let path = match uri.path().trim_start_matches('/') {
        "" => "index.html",
        p => p,
    };

    let file = match path.strip_prefix("logo/") {
        Some(name) => Logo::get(name),
        None => Assets::get(path),
    };

    let Some(file) = file else {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    };

    // An ETag over the file's own hash, and `no-cache` to mean "ask me first".
    //
    // Without either, the browser has no validator and falls back to guessing
    // how long a response stays fresh — so upgrading the binary leaves it
    // running yesterday's `app.js` against today's API, with nothing on screen
    // to say why. This makes every load a conditional request that answers 304
    // in a couple of hundred bytes when nothing changed.
    let etag = format!("\"{}\"", hex(&file.metadata.sha256_hash()[..8]));
    if headers
        .get(header::IF_NONE_MATCH)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.split(',').any(|candidate| candidate.trim() == etag))
    {
        return (StatusCode::NOT_MODIFIED, [(header::ETAG, etag)]).into_response();
    }

    let mime = mime_guess::from_path(path).first_or_octet_stream();
    (
        [
            (header::CONTENT_TYPE, mime.as_ref().to_owned()),
            (header::ETAG, etag),
            (header::CACHE_CONTROL, "no-cache".to_owned()),
        ],
        file.data.into_owned(),
    )
        .into_response()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
