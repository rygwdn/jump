use crate::utils::expand_path;
use dirs;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use std::process;

/// Get the default frecency database path
fn default_frecency_db_path() -> String {
    dirs::data_dir()
        .unwrap_or_else(|| expand_path("~/.local/share"))
        .join("jumpr")
        .join("frecency.db")
        .to_string_lossy()
        .to_string()
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct JumprConfig {
    /// Path to world trees directory
    pub world_path: Option<String>,
    /// List of source directories to scan for repositories
    pub src_paths: Vec<String>,
    /// Maximum depth to scan for repositories (optional, defaults to 3)
    pub depth_limit: Option<usize>,
    /// Path to frecency database file
    pub frecency_db_path: String,
}

impl Default for JumprConfig {
    fn default() -> Self {
        JumprConfig {
            world_path: Some("~/world/trees".to_string()),
            src_paths: vec!["~/src".to_string()],
            depth_limit: Some(3),
            frecency_db_path: default_frecency_db_path(),
        }
    }
}

pub struct ConfigManager;

impl ConfigManager {
    /// Creates the default config file if it doesn't exist
    pub fn create_default_config_if_missing() -> Result<(), std::io::Error> {
        let config_path = Self::get_config_path();

        // Check if config file already exists
        if config_path.exists() {
            return Ok(());
        }

        // Create parent directories if they don't exist
        if let Some(parent) = config_path.parent() {
            fs::create_dir_all(parent)?;
        }

        // Create default config
        let default_config = JumprConfig::default();
        let json = serde_json::to_string_pretty(&default_config)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;

        // Write to file
        fs::write(&config_path, json)?;

        Ok(())
    }

    /// Loads the full jumpr configuration
    pub fn load_config() -> JumprConfig {
        Self::load_config_with_options(true)
    }

    /// Loads the full jumpr configuration with options
    pub fn load_config_with_options(create_if_missing: bool) -> JumprConfig {
        let config_path = Self::get_config_path();

        // Create default config file if it doesn't exist
        if create_if_missing && !config_path.exists() {
            if let Err(e) = Self::create_default_config_if_missing() {
                eprintln!(
                    "Error: Failed to create default config file at {}: {}",
                    config_path.display(),
                    e
                );
            }
        }

        match try_load_config(&config_path) {
            Ok(config) => config,
            Err(e) => {
                eprintln!("Error: {e}");
                eprintln!("Config file: {}", config_path.display());
                process::exit(1);
            }
        }
    }

    /// Gets the path to the configuration file
    pub fn get_config_path() -> PathBuf {
        // Check for environment variable override
        if let Ok(config_path) = std::env::var("JUMPR_CONFIG") {
            return PathBuf::from(config_path);
        }

        let config_dir = dirs::config_dir().unwrap_or_else(|| expand_path("~/.config"));

        config_dir.join("jumpr").join("config.json")
    }
}

fn try_load_config(path: &PathBuf) -> Result<JumprConfig, String> {
    let default_config = JumprConfig::default();

    if !path.exists() {
        return Ok(default_config);
    }

    let contents =
        fs::read_to_string(path).map_err(|e| format!("Failed to read configuration file: {e}"))?;

    let file_value: serde_json::Value = serde_json::from_str(&contents)
        .map_err(|e| format!("Failed to parse configuration file: {e}"))?;

    // Start with defaults, then merge non-null values from file
    let mut merged = serde_json::to_value(&default_config)
        .map_err(|e| format!("Failed to serialize default config: {e}"))?;

    if let (Some(base), Some(overrides)) = (merged.as_object_mut(), file_value.as_object()) {
        for (key, value) in overrides {
            if !value.is_null() {
                base.insert(key.clone(), value.clone());
            }
        }
    }

    let config: JumprConfig = serde_json::from_value(merged)
        .map_err(|e| format!("Failed to deserialize configuration: {e}"))?;

    // Validate that paths are not empty strings
    if config.src_paths.iter().any(|p| p.trim().is_empty()) {
        return Err("Configuration contains empty source paths".to_string());
    }

    Ok(config)
}
