//! Configuración persistida en `%APPDATA%/com.baruk.riff/config.json`.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tauri::Manager;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// API key de Groq. Vacía hasta que el usuario la pega en Ajustes.
    #[serde(default)]
    pub api_key: String,
    /// Atajo global en formato de tauri-plugin-global-shortcut, p. ej. "Alt+R".
    #[serde(default = "default_shortcut")]
    pub shortcut: String,
    /// Segunda etapa: pulir puntuación y quitar muletillas con un LLM.
    #[serde(default = "default_true")]
    pub polish: bool,
    /// Idioma del dictado para Whisper.
    #[serde(default = "default_language")]
    pub language: String,
}

fn default_shortcut() -> String {
    "Alt+R".to_string()
}

fn default_true() -> bool {
    true
}

fn default_language() -> String {
    "es".to_string()
}

impl Default for Config {
    fn default() -> Self {
        Self {
            api_key: String::new(),
            shortcut: default_shortcut(),
            polish: true,
            language: default_language(),
        }
    }
}

fn file_path(app: &tauri::AppHandle) -> Option<PathBuf> {
    let dir = app.path().app_config_dir().ok()?;
    Some(dir.join("config.json"))
}

pub fn load(app: &tauri::AppHandle) -> Config {
    let Some(path) = file_path(app) else {
        return Config::default();
    };
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return Config::default();
    };
    serde_json::from_str(&raw).unwrap_or_default()
}

pub fn save(app: &tauri::AppHandle, config: &Config) -> Result<(), String> {
    let path = file_path(app).ok_or("no se pudo resolver el directorio de configuración")?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let raw = serde_json::to_string_pretty(config).map_err(|e| e.to_string())?;
    std::fs::write(&path, raw).map_err(|e| e.to_string())
}
