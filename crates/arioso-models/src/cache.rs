//! Where models live on each platform.

use std::path::PathBuf;

use arioso_config::Config;

/// Platform data directory for arioso, before any config override.
#[must_use]
pub fn data_dir() -> PathBuf {
    if cfg!(target_os = "windows") {
        let base = std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        return base.join("arioso");
    }
    if cfg!(target_os = "macos") {
        let base = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        return base
            .join("Library")
            .join("Application Support")
            .join("arioso");
    }
    if let Some(xdg) = std::env::var_os("XDG_DATA_HOME") {
        return PathBuf::from(xdg).join("arioso");
    }
    let base = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    base.join(".local").join("share").join("arioso")
}

/// Directory holding model files and the manifest.
#[must_use]
pub fn models_dir(config: &Config) -> PathBuf {
    config
        .models
        .dir
        .clone()
        .unwrap_or_else(|| data_dir().join("models"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_override_wins() {
        let mut cfg = Config::default();
        cfg.models.dir = Some(PathBuf::from("/tmp/arioso-test"));
        assert_eq!(models_dir(&cfg), PathBuf::from("/tmp/arioso-test"));
    }

    #[test]
    fn default_ends_with_models() {
        let dir = models_dir(&Config::default());
        assert!(dir.ends_with("models"), "{dir:?}");
    }
}
