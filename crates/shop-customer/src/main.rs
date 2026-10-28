//! Customer storefront: sign up / sign in, browse parts, product popup, cart.
//!
//! Env vars:
//!   CUSTOMER_ADDR  listen address (default 127.0.0.1:8080; use 0.0.0.0:8080 for your LAN)
//!   SHOP_DB        database file (default: shop.db next to the .exe)
//!   NO_BROWSER     set to skip opening the browser on start

use axum::{
    extract::{FromRequestParts, Path, Query, State},
    http::{header, request::Parts, HeaderMap},
    response::{Html, IntoResponse},
    routing::{get, patch, post},
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};
use shop_core::{
    auth,
    db::{Cart, Product, Settings, User},
    web::{self, ApiError, ApiResult},
    Db,
};

const PAGE: &str = include_str!("../assets/customer.html");
const COOKIE: &str = "shop_session";
const COOKIE_MAX_AGE: i64 = 60 * 60 * 24 * 30;

#[tokio::main]
async fn main() {
    let path = shop_core::default_db_path();
    let db = Db::open(&path).unwrap_or_else(|e| {
        web::fatal(&format!("Could not open database {}: {e}", path.display()))
    });

    let app = Router::new()
        .route("/", get(|| async { Html(PAGE) }))
        .route("/static/base.css", get(web::base_css))
        .route("/static/logo.svg", get(web::logo_svg))
        .route("/static/jcs-logo.svg", get(web::jcs_logo_svg))
        .route("/images/{id}", get(web::image))
        .route("/api/settings", get(settings))
        .route("/api/auth/signup", post(signup))
        .route("/api/auth/login", post(login))
        .route("/api/auth/logout", post(logout))
        .route("/api/auth/me", get(me))
        .route("/api/featured", get(featured))
        .route("/api/categories", get(categories))
        .route("/api/products", get(products))
        .route("/api/products/{id}", get(product))
        .route("/api/cart", get(cart).post(add_to_cart).delete(clear_cart))
        .route(
            "/api/cart/{item_id}",
            patch(set_quantity).delete(remove_item),
        )
        .with_state(db.clone());

    let addr = std::env::var("CUSTOMER_ADDR").unwrap_or_else(|_| "127.0.0.1:8080".into());
    web::run(app, &addr, "JCS MotorParts — Customer Store", &db).await;
}

// ---------------------------------------------------------------------------
// Session handling
// ---------------------------------------------------------------------------

fn session_token(headers: &HeaderMap) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .find(|(k, _)| *k == COOKIE)
        .map(|(_, v)| v.to_string())
        .filter(|v| !v.is_empty())
}

fn session_cookie(token: &str) -> String {
    format!("{COOKIE}={token}; Path=/; HttpOnly; SameSite=Lax; Max-Age={COOKIE_MAX_AGE}")
}

fn cleared_cookie() -> String {
    format!("{COOKIE}=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0")
}

/// Extractor: the signed-in customer, or 401.
struct CurrentUser(User);

impl FromRequestParts<Db> for CurrentUser {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, db: &Db) -> Result<Self, Self::Rejection> {
        let token = session_token(&parts.headers).ok_or_else(ApiError::unauthorized)?;
        db.session_user(&token)?
            .map(CurrentUser)
            .ok_or_else(ApiError::unauthorized)
    }
}

async fn start_session(db: &Db, user: User) -> Result<impl IntoResponse, ApiError> {
    let token = auth::new_token();
    db.create_session(user.id, &token)?;
    Ok(([(header::SET_COOKIE, session_cookie(&token))], Json(user)))
}

// ---------------------------------------------------------------------------
// Auth
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct SignupBody {
    name: String,
    email: String,
    password: String,
}

#[derive(Deserialize)]
struct LoginBody {
    email: String,
    password: String,
}

fn valid_email(email: &str) -> bool {
    let Some((local, domain)) = email.split_once('@') else {
        return false;
    };
    !local.is_empty()
        && domain.contains('.')
        && !domain.starts_with('.')
        && !domain.ends_with('.')
        && !email.contains(char::is_whitespace)
        && email.len() <= 254
}

async fn signup(
    State(db): State<Db>,
    Json(body): Json<SignupBody>,
) -> Result<impl IntoResponse, ApiError> {
    let name = body.name.trim().to_string();
    let email = body.email.trim().to_lowercase();
    if name.is_empty() {
        return Err(ApiError::bad_request("Please enter your name"));
    }
    if name.chars().count() > 80 {
        return Err(ApiError::bad_request("Name is too long"));
    }
    if !valid_email(&email) {
        return Err(ApiError::bad_request("Please enter a valid email address"));
    }
    if body.password.chars().count() < 6 {
        return Err(ApiError::bad_request(
            "Password must be at least 6 characters",
        ));
    }
    if body.password.len() > 256 {
        return Err(ApiError::bad_request("Password is too long"));
    }
    let password = body.password;
    let hash = tokio::task::spawn_blocking(move || auth::hash_password(&password))
        .await
        .map_err(|e| ApiError::internal(e.to_string()))?
        .map_err(ApiError::internal)?;
    let user = db.create_user(&name, &email, &hash)?;
    start_session(&db, user).await
}

async fn login(
    State(db): State<Db>,
    Json(body): Json<LoginBody>,
) -> Result<impl IntoResponse, ApiError> {
    let wrong = || {
        ApiError(
            axum::http::StatusCode::UNAUTHORIZED,
            "Incorrect email or password".into(),
        )
    };
    let Some((user, hash)) = db.user_credentials(&body.email.to_lowercase())? else {
        return Err(wrong());
    };
    let password = body.password;
    let ok = tokio::task::spawn_blocking(move || auth::verify_password(&password, &hash))
        .await
        .map_err(|e| ApiError::internal(e.to_string()))?;
    if !ok {
        return Err(wrong());
    }
    start_session(&db, user).await
}

async fn logout(State(db): State<Db>, headers: HeaderMap) -> Result<impl IntoResponse, ApiError> {
    if let Some(token) = session_token(&headers) {
        db.delete_session(&token)?;
    }
    Ok((
        [(header::SET_COOKIE, cleared_cookie())],
        Json(json!({ "ok": true })),
    ))
}

async fn me(CurrentUser(user): CurrentUser) -> ApiResult<User> {
    Ok(Json(user))
}

// ---------------------------------------------------------------------------
// Catalog
// ---------------------------------------------------------------------------

async fn settings(State(db): State<Db>) -> ApiResult<Settings> {
    Ok(Json(db.get_settings()?))
}

async fn featured(State(db): State<Db>, _u: CurrentUser) -> ApiResult<Vec<Product>> {
    Ok(Json(db.list_featured()?))
}

async fn categories(State(db): State<Db>, _u: CurrentUser) -> ApiResult<Vec<String>> {
    Ok(Json(db.categories()?))
}

#[derive(Deserialize)]
struct ProductQuery {
    #[serde(default)]
    q: String,
    #[serde(default)]
    category: String,
}

async fn products(
    State(db): State<Db>,
    _u: CurrentUser,
    Query(q): Query<ProductQuery>,
) -> ApiResult<Vec<Product>> {
    Ok(Json(db.list_products(&q.q, &q.category)?))
}

async fn product(State(db): State<Db>, _u: CurrentUser, Path(id): Path<i64>) -> ApiResult<Product> {
    Ok(Json(db.get_product(id)?))
}

// ---------------------------------------------------------------------------
// Cart
// ---------------------------------------------------------------------------

async fn cart(State(db): State<Db>, CurrentUser(user): CurrentUser) -> ApiResult<Cart> {
    Ok(Json(db.cart(user.id)?))
}

#[derive(Deserialize)]
struct AddBody {
    product_id: i64,
    #[serde(default)]
    variation_id: Option<i64>,
    quantity: i64,
}

async fn add_to_cart(
    State(db): State<Db>,
    CurrentUser(user): CurrentUser,
    Json(body): Json<AddBody>,
) -> ApiResult<Cart> {
    db.add_to_cart(user.id, body.product_id, body.variation_id, body.quantity)?;
    Ok(Json(db.cart(user.id)?))
}

#[derive(Deserialize)]
struct QtyBody {
    quantity: i64,
}

async fn set_quantity(
    State(db): State<Db>,
    CurrentUser(user): CurrentUser,
    Path(item_id): Path<i64>,
    Json(body): Json<QtyBody>,
) -> ApiResult<Cart> {
    db.set_cart_quantity(user.id, item_id, body.quantity)?;
    Ok(Json(db.cart(user.id)?))
}

async fn remove_item(
    State(db): State<Db>,
    CurrentUser(user): CurrentUser,
    Path(item_id): Path<i64>,
) -> ApiResult<Cart> {
    db.set_cart_quantity(user.id, item_id, 0)?;
    Ok(Json(db.cart(user.id)?))
}

async fn clear_cart(State(db): State<Db>, CurrentUser(user): CurrentUser) -> ApiResult<Value> {
    db.clear_cart(user.id)?;
    Ok(Json(json!({ "ok": true })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn email_validation() {
        assert!(valid_email("ana@example.com"));
        assert!(!valid_email("ana@example"));
        assert!(!valid_email("@example.com"));
        assert!(!valid_email("ana example@x.com"));
    }

    #[test]
    fn cookie_parsing() {
        let mut h = HeaderMap::new();
        h.insert(
            header::COOKIE,
            "a=1; shop_session=abc; b=2".parse().unwrap(),
        );
        assert_eq!(session_token(&h).as_deref(), Some("abc"));
    }
}
