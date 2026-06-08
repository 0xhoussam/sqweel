//! Persistence for saved connections.
//!
//! Non-secret fields live in JSON at `~/.config/sqweel/connections.json`;
//! passwords go to the OS secret service via `keyring` (keyed by the saved
//! connection's name). Keyring failures degrade gracefully — the connection is
//! still saved, the user just re-enters the password next time.

use std::path::PathBuf;

use gtk::glib;
use serde::{Deserialize, Serialize};

use crate::db::ConnectionConfig;

const KEYRING_SERVICE: &str = "sqweel";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SavedConnection {
    pub name: String,
    pub driver_id: String,
    pub host: String,
    pub port: u16,
    pub database: String,
    pub username: String,
    pub ssl: bool,
}

impl SavedConnection {
    pub fn from_config(cfg: &ConnectionConfig) -> Self {
        Self {
            name: default_name(cfg),
            driver_id: cfg.driver_id.clone(),
            host: cfg.host.clone(),
            port: cfg.port,
            database: cfg.database.clone(),
            username: cfg.username.clone(),
            ssl: cfg.ssl,
        }
    }

    /// Build a connection config; `password` comes from the keyring (or empty).
    pub fn to_config(&self, password: String) -> ConnectionConfig {
        ConnectionConfig {
            driver_id: self.driver_id.clone(),
            host: self.host.clone(),
            port: self.port,
            database: self.database.clone(),
            username: self.username.clone(),
            password,
            ssl: self.ssl,
        }
    }

    pub fn subtitle(&self) -> String {
        format!("{}@{}:{}/{}", self.username, self.host, self.port, self.database)
    }
}

fn default_name(cfg: &ConnectionConfig) -> String {
    format!("{}@{}/{}", cfg.username, cfg.host, cfg.database)
}

fn config_path() -> PathBuf {
    let mut p = glib::user_config_dir();
    p.push("sqweel");
    p.push("connections.json");
    p
}

pub fn load() -> Vec<SavedConnection> {
    std::fs::read(config_path())
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

fn save_all(list: &[SavedConnection]) -> std::io::Result<()> {
    let path = config_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let data = serde_json::to_vec_pretty(list).expect("serialize connections");
    std::fs::write(path, data)
}

/// Insert or update a saved connection (matched by name) and persist its
/// password to the keyring. Returns the updated list.
pub fn upsert(conn: SavedConnection, password: &str) -> Vec<SavedConnection> {
    let mut list = load();
    match list.iter_mut().find(|c| c.name == conn.name) {
        Some(existing) => *existing = conn.clone(),
        None => list.push(conn.clone()),
    }
    let _ = save_all(&list);
    store_password(&conn.name, password);
    list
}

/// Remove a saved connection by name (and its keyring entry). Returns the list.
pub fn remove(name: &str) -> Vec<SavedConnection> {
    let mut list = load();
    list.retain(|c| c.name != name);
    let _ = save_all(&list);
    delete_password(name);
    list
}

fn entry(name: &str) -> Option<keyring::Entry> {
    keyring::Entry::new(KEYRING_SERVICE, name).ok()
}

pub fn store_password(name: &str, password: &str) {
    if let Some(e) = entry(name) {
        let _ = e.set_password(password);
    }
}

pub fn get_password(name: &str) -> String {
    entry(name)
        .and_then(|e| e.get_password().ok())
        .unwrap_or_default()
}

fn delete_password(name: &str) {
    if let Some(e) = entry(name) {
        let _ = e.delete_credential();
    }
}
