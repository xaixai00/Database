//! Admin app: manage the catalog, the featured home screen, store settings,
//! and see registered customers. Only reachable from this computer.
//!
//! Env vars:
//!   ADMIN_ADDR     listen address (default 127.0.0.1:8081)
//!   STORE_URL      link shown for "Open store" (default http://127.0.0.1:8080/)
//!   SHOP_DB        database file (default: shop.db next to the .exe)
//!   NO_BROWSER     set to skip opening the browser on start

use axum::{
    extract::{DefaultBodyLimit, Path, Query, Request, State},
    http::{header, StatusCode},
    middleware::{self, Next},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use base64::Engine;
use serde::Deserialize;
use serde_json::{json, Value};
use shop_core::{
    db::{CustomerSummary, Product, ProductInput, Settings},
    web::{self, ApiError, ApiResult},
    Db,
};

const PAGE: &str = include_str!("../assets/admin.html");
const MAX_IMAGE_BYTES: usize = 6 * 1024 * 1024;

#[tokio::main]
async fn main() {
    let path = shop_core::default_db_path();
    let db = Db::open(&path).unwrap_or_else(|e| {
        web::fatal(&format!("Could not open database {}: {e}", path.display()))
    });

    let app = Router::new()
        .route("/", get(|| async { Html(PAGE) }))
        .route("/static/base.css", get(web::base_css))
        .route("/images/{id}", get(web::image))
        .route(
            "/api/images",
            post(upload_image).layer(DefaultBodyLimit::max(MAX_IMAGE_BYTES * 2)),
        )
        .route("/api/info", get(info))
        .route("/api/settings", get(get_settings).put(save_settings))
        .route("/api/categories", get(categories))
        .route("/api/products", get(list_products).post(create_product))
        .route(
            "/api/products/{id}",
            get(get_product).put(update_product).delete(delete_product),
        )
        .route("/api/featured", get(get_featured).put(set_featured))
        .route("/api/customers", get(customers))
        .route("/api/demo", post(seed_demo))
        .layer(middleware::from_fn(local_only))
        .with_state(db.clone());

    let addr = std::env::var("ADMIN_ADDR").unwrap_or_else(|_| "127.0.0.1:8081".into());
    web::run(app, &addr, "Shop — Admin", &db).await;
}

/// Reject requests whose Host isn't this machine (blocks DNS-rebinding tricks).
async fn local_only(req: Request, next: Next) -> Response {
    let host = req
        .headers()
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("");
    let name = if host.starts_with('[') {
        host.split(']')
            .next()
            .map(|h| format!("{h}]"))
            .unwrap_or_default()
    } else {
        host.split(':').next().unwrap_or("").to_string()
    };
    if matches!(name.as_str(), "localhost" | "127.0.0.1" | "[::1]") {
        next.run(req).await
    } else {
        (
            StatusCode::FORBIDDEN,
            "The admin app only accepts connections from this computer.",
        )
            .into_response()
    }
}

async fn info(State(db): State<Db>) -> Json<Value> {
    let store_url = std::env::var("STORE_URL").unwrap_or_else(|_| {
        let addr = std::env::var("CUSTOMER_ADDR").unwrap_or_else(|_| "127.0.0.1:8080".into());
        addr.parse()
            .map(|a| web::browser_url(&a))
            .unwrap_or_else(|_| "http://127.0.0.1:8080/".into())
    });
    Json(json!({
        "db_path": db.path().display().to_string(),
        "store_url": store_url,
        "version": env!("CARGO_PKG_VERSION"),
    }))
}

// ---------------------------------------------------------------------------
// Images
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct ImageUpload {
    /// `data:image/png;base64,....`
    data_url: String,
}

async fn upload_image(State(db): State<Db>, Json(body): Json<ImageUpload>) -> ApiResult<Value> {
    let bad = || ApiError::bad_request("That file doesn't look like a supported image");
    let rest = body.data_url.strip_prefix("data:").ok_or_else(bad)?;
    let (meta, b64) = rest.split_once(',').ok_or_else(bad)?;
    let mime = meta
        .strip_suffix(";base64")
        .ok_or_else(bad)?
        .to_ascii_lowercase();
    if !matches!(
        mime.as_str(),
        "image/png" | "image/jpeg" | "image/webp" | "image/gif"
    ) {
        return Err(ApiError::bad_request("Use a PNG, JPG, WEBP or GIF image"));
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(b64.trim())
        .map_err(|_| bad())?;
    if bytes.is_empty() {
        return Err(bad());
    }
    if bytes.len() > MAX_IMAGE_BYTES {
        return Err(ApiError::bad_request("Image is too large (6 MB max)"));
    }
    let id = db.insert_image(&mime, &bytes)?;
    Ok(Json(json!({ "id": id })))
}

// ---------------------------------------------------------------------------
// Products
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct ProductQuery {
    #[serde(default)]
    q: String,
    #[serde(default)]
    category: String,
}

async fn list_products(
    State(db): State<Db>,
    Query(q): Query<ProductQuery>,
) -> ApiResult<Vec<Product>> {
    Ok(Json(db.list_products(&q.q, &q.category)?))
}

async fn categories(State(db): State<Db>) -> ApiResult<Vec<String>> {
    Ok(Json(db.categories()?))
}

async fn get_product(State(db): State<Db>, Path(id): Path<i64>) -> ApiResult<Product> {
    Ok(Json(db.get_product(id)?))
}

async fn create_product(
    State(db): State<Db>,
    Json(input): Json<ProductInput>,
) -> ApiResult<Product> {
    Ok(Json(db.create_product(input)?))
}

async fn update_product(
    State(db): State<Db>,
    Path(id): Path<i64>,
    Json(input): Json<ProductInput>,
) -> ApiResult<Product> {
    Ok(Json(db.update_product(id, input)?))
}

async fn delete_product(State(db): State<Db>, Path(id): Path<i64>) -> ApiResult<Value> {
    db.delete_product(id)?;
    Ok(Json(json!({ "ok": true })))
}

// ---------------------------------------------------------------------------
// Featured / settings / customers
// ---------------------------------------------------------------------------

async fn get_featured(State(db): State<Db>) -> ApiResult<Vec<Product>> {
    Ok(Json(db.list_featured()?))
}

#[derive(Deserialize)]
struct FeaturedBody {
    product_ids: Vec<i64>,
}

async fn set_featured(
    State(db): State<Db>,
    Json(body): Json<FeaturedBody>,
) -> ApiResult<Vec<Product>> {
    Ok(Json(db.set_featured(&body.product_ids)?))
}

async fn get_settings(State(db): State<Db>) -> ApiResult<Settings> {
    Ok(Json(db.get_settings()?))
}

async fn save_settings(State(db): State<Db>, Json(s): Json<Settings>) -> ApiResult<Settings> {
    Ok(Json(db.save_settings(s)?))
}

async fn customers(State(db): State<Db>) -> ApiResult<Vec<CustomerSummary>> {
    Ok(Json(db.list_customers()?))
}

async fn seed_demo(State(db): State<Db>) -> ApiResult<Value> {
    let n = db.seed_demo()?;
    Ok(Json(json!({ "added": n })))
}
