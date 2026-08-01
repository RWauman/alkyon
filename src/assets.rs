use axum::http::{header, StatusCode, Uri};
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

pub async fn serve(uri: Uri) -> Response {
    let path = match uri.path().trim_start_matches('/') {
        "" => "index.html",
        p => p,
    };

    let file = match path.strip_prefix("logo/") {
        Some(name) => Logo::get(name),
        None => Assets::get(path),
    };

    match file {
        Some(file) => {
            let mime = mime_guess::from_path(path).first_or_octet_stream();
            (
                [(header::CONTENT_TYPE, mime.as_ref())],
                file.data.into_owned(),
            )
                .into_response()
        }
        None => (StatusCode::NOT_FOUND, "not found").into_response(),
    }
}
