//! Persisted settings: which node to talk to.

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager};

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Config {
    pub rpc_http: String,
    pub rpc_ws: String,
}

impl Default for Config {
    fn default() -> Self {
        Config { rpc_http: crate::rpc::PUBLIC_HTTP.into(), rpc_ws: String::new() }
    }
}

fn path(app: &AppHandle) -> Option<std::path::PathBuf> {
    let dir = app.path().app_config_dir().ok()?;
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir.join("config.json"))
}

pub fn load(app: &AppHandle) -> Config {
    path(app)
        .and_then(|p| std::fs::read(p).ok())
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

pub fn save(app: &AppHandle, c: &Config) -> Result<(), String> {
    let p = path(app).ok_or("no config dir")?;
    std::fs::write(p, serde_json::to_vec_pretty(c).map_err(|e| e.to_string())?).map_err(|e| e.to_string())
}
