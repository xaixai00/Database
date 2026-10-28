//! Shared code for the two shop executables.
//!
//! * `db`   – the local SQLite database both apps read and write
//! * `auth` – password hashing and session tokens
//! * `web`  – error type, image/CSS handlers and the server runner

pub mod auth;
pub mod db;
pub mod web;

pub use db::{default_db_path, Db, DbError};

/// Stylesheet shared by the admin and customer pages.
pub const BASE_CSS: &str = include_str!("../assets/base.css");

/// Brand logo, drawn faintly behind both apps (see `body::before` in base.css).
pub const LOGO_SVG: &str = include_str!("../assets/logo.svg");
