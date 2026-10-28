//! The local SQLite database shared by both executables.
//!
//! Both apps open the same `shop.db` file. WAL mode plus a busy timeout lets
//! the admin and customer processes read and write it at the same time.

use rusqlite::{params, Connection, OptionalExtension, Row};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS images (
    id          INTEGER PRIMARY KEY,
    mime        TEXT    NOT NULL,
    data        BLOB    NOT NULL,
    created_at  TEXT    NOT NULL DEFAULT (datetime('now'))
);

CREATE TABLE IF NOT EXISTS products (
    id            INTEGER PRIMARY KEY,
    name          TEXT    NOT NULL,
    category      TEXT    NOT NULL DEFAULT '',
    description   TEXT    NOT NULL DEFAULT '',
    price_cents   INTEGER NOT NULL CHECK (price_cents >= 0),
    features      TEXT    NOT NULL DEFAULT '[]',   -- JSON array of strings
    available_for TEXT    NOT NULL DEFAULT '[]',   -- JSON array of strings
    image_id      INTEGER REFERENCES images(id) ON DELETE SET NULL,
    created_at    TEXT    NOT NULL DEFAULT (datetime('now')),
    updated_at    TEXT    NOT NULL DEFAULT (datetime('now'))
);

CREATE TABLE IF NOT EXISTS variations (
    id          INTEGER PRIMARY KEY,
    product_id  INTEGER NOT NULL REFERENCES products(id) ON DELETE CASCADE,
    name        TEXT    NOT NULL,
    price_cents INTEGER CHECK (price_cents IS NULL OR price_cents >= 0), -- NULL = use product price
    position    INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS idx_variations_product ON variations(product_id);

CREATE TABLE IF NOT EXISTS featured (
    product_id  INTEGER PRIMARY KEY REFERENCES products(id) ON DELETE CASCADE,
    position    INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS users (
    id            INTEGER PRIMARY KEY,
    name          TEXT NOT NULL,
    email         TEXT NOT NULL UNIQUE COLLATE NOCASE,
    password_hash TEXT NOT NULL,
    created_at    TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE TABLE IF NOT EXISTS sessions (
    token       TEXT    PRIMARY KEY,
    user_id     INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    expires_at  INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS cart_items (
    id            INTEGER PRIMARY KEY,
    user_id       INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    product_id    INTEGER NOT NULL REFERENCES products(id) ON DELETE CASCADE,
    variation_id  INTEGER NOT NULL DEFAULT 0,           -- 0 = product has no variations
    quantity      INTEGER NOT NULL CHECK (quantity > 0),
    added_at      TEXT    NOT NULL DEFAULT (datetime('now')),
    UNIQUE (user_id, product_id, variation_id)
);

CREATE TABLE IF NOT EXISTS settings (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

PRAGMA user_version = 1;
"#;

pub const MAX_CART_QTY: i64 = 999;
const SESSION_TTL_SECS: i64 = 60 * 60 * 24 * 30;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum DbError {
    NotFound,
    Invalid(String),
    Conflict(String),
    Sql(rusqlite::Error),
}

impl fmt::Display for DbError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DbError::NotFound => write!(f, "Not found"),
            DbError::Invalid(m) | DbError::Conflict(m) => write!(f, "{m}"),
            DbError::Sql(e) => write!(f, "Database error: {e}"),
        }
    }
}

impl std::error::Error for DbError {}

impl From<rusqlite::Error> for DbError {
    fn from(e: rusqlite::Error) -> Self {
        DbError::Sql(e)
    }
}

pub type DbResult<T> = Result<T, DbError>;

fn invalid<T>(msg: &str) -> DbResult<T> {
    Err(DbError::Invalid(msg.to_string()))
}

// ---------------------------------------------------------------------------
// Models
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Variation {
    #[serde(default)]
    pub id: Option<i64>,
    pub name: String,
    /// Overrides the product price when set.
    #[serde(default)]
    pub price_cents: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Product {
    pub id: i64,
    pub name: String,
    pub category: String,
    pub description: String,
    pub price_cents: i64,
    pub features: Vec<String>,
    pub available_for: Vec<String>,
    pub image_id: Option<i64>,
    pub variations: Vec<Variation>,
    pub featured: bool,
    pub created_at: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ProductInput {
    pub name: String,
    #[serde(default)]
    pub category: String,
    #[serde(default)]
    pub description: String,
    pub price_cents: i64,
    #[serde(default)]
    pub features: Vec<String>,
    #[serde(default)]
    pub available_for: Vec<String>,
    #[serde(default)]
    pub image_id: Option<i64>,
    #[serde(default)]
    pub variations: Vec<Variation>,
}

fn clean_list(items: Vec<String>) -> Vec<String> {
    let mut seen = HashSet::new();
    items
        .into_iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty() && seen.insert(s.to_lowercase()))
        .collect()
}

impl ProductInput {
    fn clean(mut self) -> DbResult<Self> {
        self.name = self.name.trim().to_string();
        self.category = self.category.trim().to_string();
        self.description = self.description.trim().to_string();
        if self.name.is_empty() {
            return invalid("Product name is required");
        }
        if self.name.chars().count() > 200 {
            return invalid("Product name is too long (200 characters max)");
        }
        if self.price_cents < 0 {
            return invalid("Price can't be negative");
        }
        self.features = clean_list(self.features);
        self.available_for = clean_list(self.available_for);
        for v in &mut self.variations {
            v.name = v.name.trim().to_string();
            if v.name.is_empty() {
                return invalid("Every variation needs a name");
            }
            if matches!(v.price_cents, Some(p) if p < 0) {
                return invalid("Variation prices can't be negative");
            }
        }
        let mut names = HashSet::new();
        if !self
            .variations
            .iter()
            .all(|v| names.insert(v.name.to_lowercase()))
        {
            return invalid("Variation names must be unique");
        }
        Ok(self)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    pub shop_name: String,
    pub tagline: String,
    pub currency: String,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            shop_name: "MotoParts Shop".into(),
            tagline: "Genuine and aftermarket motorcycle parts, ready to ride.".into(),
            currency: "₱".into(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct User {
    pub id: i64,
    pub name: String,
    pub email: String,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct CustomerSummary {
    pub id: i64,
    pub name: String,
    pub email: String,
    pub created_at: String,
    pub cart_quantity: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct CartLine {
    pub id: i64,
    pub product_id: i64,
    pub product_name: String,
    pub image_id: Option<i64>,
    pub variation_id: Option<i64>,
    pub variation_name: Option<String>,
    pub unit_price_cents: i64,
    pub quantity: i64,
    pub line_total_cents: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Cart {
    pub items: Vec<CartLine>,
    pub item_count: i64,
    pub total_cents: i64,
}

// ---------------------------------------------------------------------------
// Database handle
// ---------------------------------------------------------------------------

/// Where `shop.db` lives: `$SHOP_DB` if set, otherwise next to the executable,
/// so both .exe files in the same folder share one database.
pub fn default_db_path() -> PathBuf {
    if let Some(p) = std::env::var_os("SHOP_DB") {
        return PathBuf::from(p);
    }
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join("shop.db")))
        .unwrap_or_else(|| PathBuf::from("shop.db"))
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[derive(Clone)]
pub struct Db {
    conn: Arc<Mutex<Connection>>,
    path: PathBuf,
}

const PRODUCT_COLS: &str = "p.id, p.name, p.category, p.description, p.price_cents, \
    p.features, p.available_for, p.image_id, p.created_at, \
    EXISTS (SELECT 1 FROM featured f WHERE f.product_id = p.id)";

fn map_product(r: &Row) -> rusqlite::Result<Product> {
    let features: String = r.get(5)?;
    let available_for: String = r.get(6)?;
    Ok(Product {
        id: r.get(0)?,
        name: r.get(1)?,
        category: r.get(2)?,
        description: r.get(3)?,
        price_cents: r.get(4)?,
        features: serde_json::from_str(&features).unwrap_or_default(),
        available_for: serde_json::from_str(&available_for).unwrap_or_default(),
        image_id: r.get(7)?,
        created_at: r.get(8)?,
        featured: r.get::<_, i64>(9)? != 0,
        variations: Vec::new(),
    })
}

fn variations_for(conn: &Connection, product_id: i64) -> rusqlite::Result<Vec<Variation>> {
    let mut st = conn.prepare_cached(
        "SELECT id, name, price_cents FROM variations WHERE product_id = ?1 ORDER BY position, id",
    )?;
    let rows = st.query_map([product_id], |r| {
        Ok(Variation {
            id: Some(r.get(0)?),
            name: r.get(1)?,
            price_cents: r.get(2)?,
        })
    })?;
    rows.collect()
}

fn with_variations(
    conn: &Connection,
    mut products: Vec<Product>,
) -> rusqlite::Result<Vec<Product>> {
    for p in &mut products {
        p.variations = variations_for(conn, p.id)?;
    }
    Ok(products)
}

fn get_product_on(conn: &Connection, id: i64) -> DbResult<Product> {
    let sql = format!("SELECT {PRODUCT_COLS} FROM products p WHERE p.id = ?1");
    let mut p = conn
        .query_row(&sql, [id], map_product)
        .optional()?
        .ok_or(DbError::NotFound)?;
    p.variations = variations_for(conn, id)?;
    Ok(p)
}

fn image_exists(conn: &Connection, id: i64) -> rusqlite::Result<bool> {
    conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM images WHERE id = ?1)",
        [id],
        |r| r.get::<_, i64>(0),
    )
    .map(|n| n != 0)
}

fn map_user(r: &Row) -> rusqlite::Result<User> {
    Ok(User {
        id: r.get(0)?,
        name: r.get(1)?,
        email: r.get(2)?,
        created_at: r.get(3)?,
    })
}

impl Db {
    pub fn open(path: &Path) -> DbResult<Self> {
        if let Some(dir) = path.parent() {
            if !dir.as_os_str().is_empty() {
                let _ = std::fs::create_dir_all(dir);
            }
        }
        let conn = Connection::open(path)?;
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.query_row("PRAGMA journal_mode = WAL", [], |_| Ok(()))?;
        conn.execute_batch("PRAGMA foreign_keys = ON;")?;
        conn.execute_batch(SCHEMA)?;
        Ok(Db {
            conn: Arc::new(Mutex::new(conn)),
            path: path.to_path_buf(),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn lock(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    // ----- images ---------------------------------------------------------

    pub fn insert_image(&self, mime: &str, data: &[u8]) -> DbResult<i64> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO images (mime, data) VALUES (?1, ?2)",
            params![mime, data],
        )?;
        Ok(conn.last_insert_rowid())
    }

    pub fn get_image(&self, id: i64) -> DbResult<(String, Vec<u8>)> {
        self.lock()
            .query_row("SELECT mime, data FROM images WHERE id = ?1", [id], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .optional()?
            .ok_or(DbError::NotFound)
    }

    // ----- products -------------------------------------------------------

    /// `query` matches name, description, category and "available for";
    /// `category` filters exactly. Empty strings mean "no filter".
    pub fn list_products(&self, query: &str, category: &str) -> DbResult<Vec<Product>> {
        let conn = self.lock();
        let q = query.trim();
        let like = format!("%{q}%");
        let sql = format!(
            "SELECT {PRODUCT_COLS} FROM products p
             WHERE (?1 = '' OR p.name LIKE ?2 OR p.description LIKE ?2
                    OR p.category LIKE ?2 OR p.available_for LIKE ?2)
               AND (?3 = '' OR p.category = ?3)
             ORDER BY p.id DESC"
        );
        let mut st = conn.prepare(&sql)?;
        let products = st
            .query_map(params![q, like, category.trim()], map_product)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(with_variations(&conn, products)?)
    }

    pub fn categories(&self) -> DbResult<Vec<String>> {
        let conn = self.lock();
        let mut st = conn.prepare(
            "SELECT DISTINCT category FROM products WHERE category <> '' ORDER BY category COLLATE NOCASE",
        )?;
        let rows = st.query_map([], |r| r.get(0))?;
        Ok(rows.collect::<rusqlite::Result<Vec<String>>>()?)
    }

    pub fn get_product(&self, id: i64) -> DbResult<Product> {
        get_product_on(&self.lock(), id)
    }

    pub fn create_product(&self, input: ProductInput) -> DbResult<Product> {
        let input = input.clean()?;
        let mut conn = self.lock();
        if let Some(img) = input.image_id {
            if !image_exists(&conn, img)? {
                return invalid("Uploaded image was not found, please upload it again");
            }
        }
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT INTO products (name, category, description, price_cents, features, available_for, image_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                input.name,
                input.category,
                input.description,
                input.price_cents,
                serde_json::to_string(&input.features).unwrap_or_else(|_| "[]".into()),
                serde_json::to_string(&input.available_for).unwrap_or_else(|_| "[]".into()),
                input.image_id,
            ],
        )?;
        let id = tx.last_insert_rowid();
        for (pos, v) in input.variations.iter().enumerate() {
            tx.execute(
                "INSERT INTO variations (product_id, name, price_cents, position) VALUES (?1, ?2, ?3, ?4)",
                params![id, v.name, v.price_cents, pos as i64],
            )?;
        }
        tx.commit()?;
        get_product_on(&conn, id)
    }

    pub fn update_product(&self, id: i64, input: ProductInput) -> DbResult<Product> {
        let input = input.clean()?;
        let mut conn = self.lock();
        let old_image: Option<i64> = conn
            .query_row("SELECT image_id FROM products WHERE id = ?1", [id], |r| {
                r.get(0)
            })
            .optional()?
            .ok_or(DbError::NotFound)?;
        if let Some(img) = input.image_id {
            if Some(img) != old_image && !image_exists(&conn, img)? {
                return invalid("Uploaded image was not found, please upload it again");
            }
        }

        let tx = conn.transaction()?;
        tx.execute(
            "UPDATE products SET name = ?1, category = ?2, description = ?3, price_cents = ?4,
                 features = ?5, available_for = ?6, image_id = ?7, updated_at = datetime('now')
             WHERE id = ?8",
            params![
                input.name,
                input.category,
                input.description,
                input.price_cents,
                serde_json::to_string(&input.features).unwrap_or_else(|_| "[]".into()),
                serde_json::to_string(&input.available_for).unwrap_or_else(|_| "[]".into()),
                input.image_id,
                id,
            ],
        )?;

        // Sync variations in place so existing cart lines keep pointing at them.
        let existing: HashSet<i64> = {
            let mut st = tx.prepare("SELECT id FROM variations WHERE product_id = ?1")?;
            let ids = st.query_map([id], |r| r.get(0))?;
            ids.collect::<rusqlite::Result<HashSet<i64>>>()?
        };
        let kept: HashSet<i64> = input
            .variations
            .iter()
            .filter_map(|v| v.id)
            .filter(|vid| existing.contains(vid))
            .collect();
        for removed in existing.difference(&kept) {
            tx.execute(
                "DELETE FROM cart_items WHERE product_id = ?1 AND variation_id = ?2",
                params![id, removed],
            )?;
            tx.execute("DELETE FROM variations WHERE id = ?1", [removed])?;
        }
        let had_variations = !existing.is_empty();
        for (pos, v) in input.variations.iter().enumerate() {
            match v.id.filter(|vid| kept.contains(vid)) {
                Some(vid) => {
                    tx.execute(
                        "UPDATE variations SET name = ?1, price_cents = ?2, position = ?3 WHERE id = ?4",
                        params![v.name, v.price_cents, pos as i64, vid],
                    )?;
                }
                None => {
                    tx.execute(
                        "INSERT INTO variations (product_id, name, price_cents, position) VALUES (?1, ?2, ?3, ?4)",
                        params![id, v.name, v.price_cents, pos as i64],
                    )?;
                }
            }
        }
        // A product that gains or loses variations invalidates carts that used the other shape.
        if !had_variations && !input.variations.is_empty() {
            tx.execute(
                "DELETE FROM cart_items WHERE product_id = ?1 AND variation_id = 0",
                [id],
            )?;
        }

        if old_image.is_some() && old_image != input.image_id {
            tx.execute("DELETE FROM images WHERE id = ?1", [old_image])?;
        }
        tx.commit()?;
        get_product_on(&conn, id)
    }

    pub fn delete_product(&self, id: i64) -> DbResult<()> {
        let mut conn = self.lock();
        let image: Option<i64> = conn
            .query_row("SELECT image_id FROM products WHERE id = ?1", [id], |r| {
                r.get(0)
            })
            .optional()?
            .ok_or(DbError::NotFound)?;
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM products WHERE id = ?1", [id])?;
        if let Some(img) = image {
            tx.execute("DELETE FROM images WHERE id = ?1", [img])?;
        }
        tx.commit()?;
        Ok(())
    }

    // ----- featured -------------------------------------------------------

    pub fn list_featured(&self) -> DbResult<Vec<Product>> {
        let conn = self.lock();
        let sql = format!(
            "SELECT {PRODUCT_COLS} FROM featured f JOIN products p ON p.id = f.product_id
             ORDER BY f.position, p.id"
        );
        let mut st = conn.prepare(&sql)?;
        let products = st
            .query_map([], map_product)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(with_variations(&conn, products)?)
    }

    /// Replace the featured list with `ids`, in order. Unknown ids are ignored.
    pub fn set_featured(&self, ids: &[i64]) -> DbResult<Vec<Product>> {
        {
            let mut conn = self.lock();
            let tx = conn.transaction()?;
            tx.execute("DELETE FROM featured", [])?;
            let mut seen = HashSet::new();
            let mut pos = 0i64;
            for &id in ids {
                if !seen.insert(id) {
                    continue;
                }
                let n = tx.execute(
                    "INSERT INTO featured (product_id, position)
                     SELECT id, ?2 FROM products WHERE id = ?1",
                    params![id, pos],
                )?;
                pos += n as i64;
            }
            tx.commit()?;
        }
        self.list_featured()
    }

    // ----- settings -------------------------------------------------------

    pub fn get_settings(&self) -> DbResult<Settings> {
        let conn = self.lock();
        let mut s = Settings::default();
        let mut st = conn.prepare("SELECT key, value FROM settings")?;
        let rows = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        for row in rows {
            let (k, v) = row?;
            match k.as_str() {
                "shop_name" => s.shop_name = v,
                "tagline" => s.tagline = v,
                "currency" => s.currency = v,
                _ => {}
            }
        }
        Ok(s)
    }

    pub fn save_settings(&self, s: Settings) -> DbResult<Settings> {
        let shop_name = s.shop_name.trim().to_string();
        let currency = s.currency.trim().to_string();
        if shop_name.is_empty() {
            return invalid("Shop name is required");
        }
        if currency.is_empty() || currency.chars().count() > 5 {
            return invalid("Currency symbol must be 1–5 characters");
        }
        {
            let mut conn = self.lock();
            let tx = conn.transaction()?;
            for (k, v) in [
                ("shop_name", shop_name.as_str()),
                ("tagline", s.tagline.trim()),
                ("currency", currency.as_str()),
            ] {
                tx.execute(
                    "INSERT INTO settings (key, value) VALUES (?1, ?2)
                     ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                    params![k, v],
                )?;
            }
            tx.commit()?;
        }
        self.get_settings()
    }

    // ----- users & sessions -----------------------------------------------

    pub fn create_user(&self, name: &str, email: &str, password_hash: &str) -> DbResult<User> {
        let conn = self.lock();
        let res = conn.execute(
            "INSERT INTO users (name, email, password_hash) VALUES (?1, ?2, ?3)",
            params![name.trim(), email.trim().to_lowercase(), password_hash],
        );
        match res {
            Ok(_) => {}
            Err(rusqlite::Error::SqliteFailure(e, _))
                if e.code == rusqlite::ErrorCode::ConstraintViolation =>
            {
                return Err(DbError::Conflict(
                    "An account with that email already exists".into(),
                ))
            }
            Err(e) => return Err(e.into()),
        }
        let id = conn.last_insert_rowid();
        Ok(conn.query_row(
            "SELECT id, name, email, created_at FROM users WHERE id = ?1",
            [id],
            map_user,
        )?)
    }

    /// Returns the user and their password hash.
    pub fn user_credentials(&self, email: &str) -> DbResult<Option<(User, String)>> {
        Ok(self
            .lock()
            .query_row(
                "SELECT id, name, email, created_at, password_hash FROM users WHERE email = ?1",
                [email.trim()],
                |r| Ok((map_user(r)?, r.get(4)?)),
            )
            .optional()?)
    }

    pub fn create_session(&self, user_id: i64, token: &str) -> DbResult<()> {
        let conn = self.lock();
        let now = now_unix();
        conn.execute("DELETE FROM sessions WHERE expires_at < ?1", [now])?;
        conn.execute(
            "INSERT INTO sessions (token, user_id, expires_at) VALUES (?1, ?2, ?3)",
            params![token, user_id, now + SESSION_TTL_SECS],
        )?;
        Ok(())
    }

    pub fn session_user(&self, token: &str) -> DbResult<Option<User>> {
        Ok(self
            .lock()
            .query_row(
                "SELECT u.id, u.name, u.email, u.created_at
                 FROM sessions s JOIN users u ON u.id = s.user_id
                 WHERE s.token = ?1 AND s.expires_at > ?2",
                params![token, now_unix()],
                map_user,
            )
            .optional()?)
    }

    pub fn delete_session(&self, token: &str) -> DbResult<()> {
        self.lock()
            .execute("DELETE FROM sessions WHERE token = ?1", [token])?;
        Ok(())
    }

    pub fn list_customers(&self) -> DbResult<Vec<CustomerSummary>> {
        let conn = self.lock();
        let mut st = conn.prepare(
            "SELECT u.id, u.name, u.email, u.created_at,
                    COALESCE((SELECT SUM(c.quantity) FROM cart_items c WHERE c.user_id = u.id), 0)
             FROM users u ORDER BY u.id DESC",
        )?;
        let rows = st.query_map([], |r| {
            Ok(CustomerSummary {
                id: r.get(0)?,
                name: r.get(1)?,
                email: r.get(2)?,
                created_at: r.get(3)?,
                cart_quantity: r.get(4)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    // ----- cart -----------------------------------------------------------

    pub fn cart(&self, user_id: i64) -> DbResult<Cart> {
        let conn = self.lock();
        let mut st = conn.prepare(
            "SELECT c.id, c.product_id, p.name, p.image_id, v.id, v.name,
                    COALESCE(v.price_cents, p.price_cents), c.quantity
             FROM cart_items c
             JOIN products p ON p.id = c.product_id
             LEFT JOIN variations v ON v.id = c.variation_id AND c.variation_id <> 0
             WHERE c.user_id = ?1
             ORDER BY c.id",
        )?;
        let items = st
            .query_map([user_id], |r| {
                let unit: i64 = r.get(6)?;
                let qty: i64 = r.get(7)?;
                Ok(CartLine {
                    id: r.get(0)?,
                    product_id: r.get(1)?,
                    product_name: r.get(2)?,
                    image_id: r.get(3)?,
                    variation_id: r.get(4)?,
                    variation_name: r.get(5)?,
                    unit_price_cents: unit,
                    quantity: qty,
                    line_total_cents: unit * qty,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(Cart {
            item_count: items.iter().map(|i| i.quantity).sum(),
            total_cents: items.iter().map(|i| i.line_total_cents).sum(),
            items,
        })
    }

    pub fn add_to_cart(
        &self,
        user_id: i64,
        product_id: i64,
        variation_id: Option<i64>,
        quantity: i64,
    ) -> DbResult<()> {
        if !(1..=MAX_CART_QTY).contains(&quantity) {
            return invalid("Quantity must be between 1 and 999");
        }
        let conn = self.lock();
        let exists: bool = conn.query_row(
            "SELECT EXISTS (SELECT 1 FROM products WHERE id = ?1)",
            [product_id],
            |r| r.get::<_, i64>(0),
        )? != 0;
        if !exists {
            return Err(DbError::NotFound);
        }
        let variation_count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM variations WHERE product_id = ?1",
            [product_id],
            |r| r.get(0),
        )?;
        let vid = match (variation_count > 0, variation_id.filter(|v| *v != 0)) {
            (true, Some(v)) => {
                let ok: i64 = conn.query_row(
                    "SELECT COUNT(*) FROM variations WHERE id = ?1 AND product_id = ?2",
                    params![v, product_id],
                    |r| r.get(0),
                )?;
                if ok == 0 {
                    return invalid("That variation is no longer available");
                }
                v
            }
            (true, None) => return invalid("Please choose a variation"),
            (false, _) => 0,
        };
        conn.execute(
            "INSERT INTO cart_items (user_id, product_id, variation_id, quantity)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (user_id, product_id, variation_id)
             DO UPDATE SET quantity = MIN(quantity + excluded.quantity, ?5)",
            params![user_id, product_id, vid, quantity, MAX_CART_QTY],
        )?;
        Ok(())
    }

    /// Set a cart line's quantity; 0 or less removes the line.
    pub fn set_cart_quantity(&self, user_id: i64, item_id: i64, quantity: i64) -> DbResult<()> {
        let conn = self.lock();
        let n = if quantity <= 0 {
            conn.execute(
                "DELETE FROM cart_items WHERE id = ?1 AND user_id = ?2",
                params![item_id, user_id],
            )?
        } else {
            conn.execute(
                "UPDATE cart_items SET quantity = ?1 WHERE id = ?2 AND user_id = ?3",
                params![quantity.min(MAX_CART_QTY), item_id, user_id],
            )?
        };
        if n == 0 {
            return Err(DbError::NotFound);
        }
        Ok(())
    }

    pub fn clear_cart(&self, user_id: i64) -> DbResult<()> {
        self.lock()
            .execute("DELETE FROM cart_items WHERE user_id = ?1", [user_id])?;
        Ok(())
    }

    // ----- demo data ------------------------------------------------------

    /// Adds a handful of sample parts. Only allowed while the catalog is empty.
    pub fn seed_demo(&self) -> DbResult<usize> {
        let count: i64 = self
            .lock()
            .query_row("SELECT COUNT(*) FROM products", [], |r| r.get(0))?;
        if count > 0 {
            return Err(DbError::Conflict(
                "Sample products can only be loaded into an empty catalog".into(),
            ));
        }
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        let var = |name: &str, price: Option<i64>| Variation {
            id: None,
            name: name.into(),
            price_cents: price,
        };
        let samples = vec![
            ProductInput {
                name: "4-Piston Front Brake Caliper".into(),
                category: "Brakes".into(),
                description: "CNC-machined radial caliper with stainless pistons for stronger, more consistent stopping power on daily rides and spirited weekends.".into(),
                price_cents: 385_000,
                features: s(&["CNC-machined 6061 aluminum body", "Stainless steel pistons", "Includes pads and mounting bolts", "Anodized finish that resists fading"]),
                available_for: s(&["Yamaha NMAX 155", "Yamaha Aerox 155", "Honda Click 125i / 150i", "Honda ADV 160"]),
                image_id: None,
                variations: vec![var("Black", None), var("Red", None), var("Gold", Some(405_000))],
            },
            ProductInput {
                name: "Iridium Spark Plug".into(),
                category: "Engine".into(),
                description: "Fine-wire iridium tip for quicker starts, smoother idle and longer service life than standard nickel plugs.".into(),
                price_cents: 45_000,
                features: s(&["0.6 mm iridium center electrode", "Up to 3× longer life", "Pre-gapped from the factory"]),
                available_for: s(&["Honda Click 125i", "Honda Beat", "Suzuki Raider R150", "Yamaha Mio i 125"]),
                image_id: None,
                variations: vec![var("CR7E", None), var("CR8E", None), var("CPR8EA-9", Some(52_000))],
            },
            ProductInput {
                name: "O-Ring Drive Chain 428H".into(),
                category: "Drivetrain".into(),
                description: "Heavy-duty sealed chain that keeps grease in and grit out, so it stretches less and lasts longer.".into(),
                price_cents: 120_000,
                features: s(&["Sealed O-rings", "Heat-treated pins and bushings", "Includes master link"]),
                available_for: s(&["Honda XRM 125", "Suzuki Smash 115", "Kawasaki Barako 175", "Yamaha Sniper 155"]),
                image_id: None,
                variations: vec![var("120 links", None), var("130 links", Some(132_000))],
            },
            ProductInput {
                name: "LED Projector Headlight".into(),
                category: "Lighting".into(),
                description: "Plug-and-play LED projector with a sharp cutoff line that lights the road without blinding oncoming traffic.".into(),
                price_cents: 275_000,
                features: s(&["6000K white output", "Built-in cooling fan", "Plug-and-play harness", "IP67 water resistance"]),
                available_for: s(&["Yamaha NMAX 155 (2020+)", "Yamaha Aerox 155"]),
                image_id: None,
                variations: vec![],
            },
            ProductInput {
                name: "Fully Synthetic 10W-40 Engine Oil".into(),
                category: "Oils & Fluids".into(),
                description: "JASO MA2 rated synthetic oil that protects at high RPM and keeps wet clutches engaging cleanly.".into(),
                price_cents: 52_000,
                features: s(&["JASO MA2 / API SN", "Wet-clutch safe", "Stable under high heat"]),
                available_for: s(&["All 4-stroke motorcycles", "Scooters with wet clutch"]),
                image_id: None,
                variations: vec![var("0.8 L", Some(43_000)), var("1 L", None)],
            },
        ];
        let mut ids = Vec::new();
        for p in samples {
            ids.push(self.create_product(p)?.id);
        }
        let featured: Vec<i64> = ids.iter().copied().take(3).collect();
        self.set_featured(&featured)?;
        Ok(ids.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_db() -> Db {
        let dir = std::env::temp_dir().join(format!("shop-test-{}", crate::auth::new_token()));
        Db::open(&dir.join("shop.db")).unwrap()
    }

    #[test]
    fn catalog_featured_and_cart() {
        let db = temp_db();
        assert_eq!(db.seed_demo().unwrap(), 5);
        let all = db.list_products("", "").unwrap();
        assert_eq!(all.len(), 5);
        assert_eq!(db.list_featured().unwrap().len(), 3);
        assert_eq!(db.list_products("nmax", "").unwrap().len(), 2);

        let user = db.create_user("Ana", "Ana@Example.com", "hash").unwrap();
        assert_eq!(user.email, "ana@example.com");
        assert!(matches!(
            db.create_user("Ana", "ana@example.com", "hash"),
            Err(DbError::Conflict(_))
        ));

        let caliper = all.iter().find(|p| p.name.contains("Caliper")).unwrap();
        let gold = caliper
            .variations
            .iter()
            .find(|v| v.name == "Gold")
            .unwrap();
        assert!(db.add_to_cart(user.id, caliper.id, None, 1).is_err());
        db.add_to_cart(user.id, caliper.id, gold.id, 2).unwrap();
        db.add_to_cart(user.id, caliper.id, gold.id, 1).unwrap();
        let cart = db.cart(user.id).unwrap();
        assert_eq!(cart.items.len(), 1);
        assert_eq!(cart.item_count, 3);
        assert_eq!(cart.total_cents, 3 * 405_000);

        let line = cart.items[0].id;
        db.set_cart_quantity(user.id, line, 1).unwrap();
        assert_eq!(db.cart(user.id).unwrap().total_cents, 405_000);
        db.set_cart_quantity(user.id, line, 0).unwrap();
        assert!(db.cart(user.id).unwrap().items.is_empty());

        db.delete_product(caliper.id).unwrap();
        assert_eq!(db.list_featured().unwrap().len(), 2);
    }
}
