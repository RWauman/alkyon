use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("unknown source `{0}`")]
    UnknownSource(String),

    #[error("source `{0}` already exists")]
    DuplicateSource(String),

    #[error("{0}")]
    BadRequest(String),

    #[error("{0}")]
    Unsupported(String),

    #[error("no credential for source `{0}` in the vault — register it again to store one")]
    MissingSecret(String),

    #[error("vault: {0}")]
    Vault(#[from] keyring::Error),

    #[error("terminal: {0}")]
    Terminal(String),

    #[error("duckdb: {0}")]
    Federated(String),

    /// Anything sqlx reports, which is now PostgreSQL *and* MySQL.
    ///
    /// Deliberately not named after an engine: sqlx's error carries no backend,
    /// so the old `postgres:` prefix started labelling MySQL failures — a wrong
    /// MySQL password came back as `postgres: Access denied for user 'root'`,
    /// which is worse than saying nothing.
    #[error("database: {0}")]
    Sqlx(#[from] sqlx::Error),

    #[error("sql server: {0}")]
    MsSql(#[from] tiberius::error::Error),

    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("json: {0}")]
    Json(#[from] serde_json::Error),

    #[error("websocket: {0}")]
    WebSocket(#[from] axum::Error),
}

impl Error {
    pub fn status(&self) -> StatusCode {
        match self {
            Error::UnknownSource(_) => StatusCode::NOT_FOUND,
            Error::DuplicateSource(_) => StatusCode::CONFLICT,
            Error::BadRequest(_) | Error::Unsupported(_) => StatusCode::BAD_REQUEST,
            Error::MissingSecret(_) => StatusCode::PRECONDITION_FAILED,
            Error::Sqlx(_) | Error::MsSql(_) => StatusCode::BAD_GATEWAY,
            // A federated failure is usually the user's SQL, not a broken server.
            Error::Federated(_) => StatusCode::BAD_REQUEST,
            Error::Io(_)
            | Error::Json(_)
            | Error::WebSocket(_)
            | Error::Vault(_)
            | Error::Terminal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        let status = self.status();
        if status.is_server_error() {
            tracing::error!(error = %self, "request failed");
        }
        (status, Json(json!({ "error": self.to_string() }))).into_response()
    }
}
