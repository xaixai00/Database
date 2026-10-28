//! Web helpers shared by both executables.

use crate::db::{Db, DbError};
use axum::{
    extract::{Path, State},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    Json, Router,
};
use std::net::SocketAddr;

/// JSON error response: `{"error": "..."}`.
#[derive(Debug)]
pub struct ApiError(pub StatusCode, pub String);

impl ApiError {
    pub fn bad_request(msg: impl Into<String>) -> Self {
        ApiError(StatusCode::BAD_REQUEST, msg.into())
    }
    pub fn unauthorized() -> Self {
        ApiError(StatusCode::UNAUTHORIZED, "Please sign in first".into())
    }
    pub fn internal(msg: impl Into<String>) -> Self {
        ApiError(StatusCode::INTERNAL_SERVER_ERROR, msg.into())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(serde_json::json!({ "error": self.1 }))).into_response()
    }
}

impl From<DbError> for ApiError {
    fn from(e: DbError) -> Self {
        match e {
            DbError::NotFound => ApiError(StatusCode::NOT_FOUND, "Not found".into()),
            DbError::Invalid(m) => ApiError(StatusCode::BAD_REQUEST, m),
            DbError::Conflict(m) => ApiError(StatusCode::CONFLICT, m),
            DbError::Sql(e) => {
                eprintln!("[db] {e}");
                ApiError::internal("Something went wrong with the database")
            }
        }
    }
}

pub type ApiResult<T> = Result<Json<T>, ApiError>;

/// `GET /images/{id}` – images are immutable (a new upload gets a new id).
pub async fn image(State(db): State<Db>, Path(id): Path<i64>) -> Result<Response, ApiError> {
    let (mime, data) = db.get_image(id)?;
    Ok((
        [
            (header::CONTENT_TYPE, mime),
            (
                header::CACHE_CONTROL,
                "public, max-age=31536000, immutable".to_string(),
            ),
        ],
        data,
    )
        .into_response())
}

/// `GET /static/base.css`
pub async fn base_css() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
        crate::BASE_CSS,
    )
}

/// `GET /static/logo.svg`
pub async fn logo_svg() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "image/svg+xml"),
            (header::CACHE_CONTROL, "public, max-age=86400"),
        ],
        crate::LOGO_SVG,
    )
}

/// Print an error and keep the console window open on Windows before exiting.
pub fn fatal(msg: &str) -> ! {
    eprintln!("\nERROR: {msg}\n");
    if cfg!(windows) {
        eprintln!("Press Enter to close this window...");
        let mut s = String::new();
        let _ = std::io::stdin().read_line(&mut s);
    }
    std::process::exit(1);
}

/// URL a browser on this machine should open for a listen address.
pub fn browser_url(addr: &SocketAddr) -> String {
    let host = if addr.ip().is_unspecified() || addr.ip().is_loopback() {
        "127.0.0.1".to_string()
    } else {
        addr.ip().to_string()
    };
    format!("http://{host}:{}/", addr.port())
}

/// Bind, print a banner, open the browser (unless `NO_BROWSER` is set) and serve
/// until Ctrl+C.
pub async fn run(app: Router, addr: &str, title: &str, db: &Db) {
    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) => fatal(&format!(
            "Could not start on {addr}: {e}\nIs another copy of this app already running?"
        )),
    };
    let local = listener
        .local_addr()
        .unwrap_or_else(|_| "127.0.0.1:0".parse().unwrap());
    let url = browser_url(&local);

    println!("==============================================");
    println!("  {title}");
    println!("==============================================");
    println!("  Open:      {url}");
    println!("  Listening: {local}");
    println!("  Database:  {}", db.path().display());
    println!("  Press Ctrl+C (or close this window) to stop.");
    println!();

    if std::env::var_os("NO_BROWSER").is_none() {
        if let Err(e) = open::that(&url) {
            eprintln!("Couldn't open a browser automatically ({e}). Open {url} yourself.");
        }
    }

    let shutdown = async {
        let _ = tokio::signal::ctrl_c().await;
        println!("Shutting down...");
    };
    if let Err(e) = axum::serve(listener, app)
        .with_graceful_shutdown(shutdown)
        .await
    {
        fatal(&format!("Server error: {e}"));
    }
}
