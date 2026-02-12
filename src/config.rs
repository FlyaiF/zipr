use std::collections::HashSet;
use std::fs;
use std::path::Path;

use anyhow::Result;
use serde::Deserialize;

#[derive(Debug, Clone)]
pub struct Config {
    pub archive_exts: HashSet<String>,
    pub default_level: i64,
}

#[derive(Debug, Deserialize)]
struct FileConfig {
    archive: Option<ArchiveConfig>,
    compression: Option<CompressionConfig>,
}

#[derive(Debug, Deserialize)]
struct ArchiveConfig {
    extensions: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
struct CompressionConfig {
    default_level: Option<i64>,
}

impl Default for Config {
    fn default() -> Self {
        let archive_exts = ["zip", "jar", "war"]
            .into_iter()
            .map(|s| s.to_string())
            .collect();
        Self {
            archive_exts,
            default_level: 6,
        }
    }
}

impl Config {
    pub fn load(cli_archive_exts: Option<&str>) -> Result<Self> {
        let mut cfg = Self::default();
        if Path::new("zipr.toml").exists() {
            let raw = fs::read_to_string("zipr.toml")?;
            let file_cfg: FileConfig = toml::from_str(&raw)?;
            if let Some(a) = file_cfg.archive.and_then(|x| x.extensions) {
                cfg.archive_exts = normalize_exts(a);
            }
            if let Some(level) = file_cfg.compression.and_then(|x| x.default_level) {
                cfg.default_level = level.clamp(0, 9);
            }
        }

        if let Some(exts) = cli_archive_exts {
            let parsed = exts
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(normalize_ext)
                .collect::<HashSet<_>>();
            if !parsed.is_empty() {
                cfg.archive_exts = parsed;
            }
        }
        Ok(cfg)
    }

    pub fn is_archive_name(&self, name: &str) -> bool {
        let ext = name.rsplit('.').next().map(normalize_ext);
        ext.is_some_and(|e| self.archive_exts.contains(&e))
    }
}

fn normalize_exts(v: Vec<String>) -> HashSet<String> {
    v.into_iter().map(|s| normalize_ext(&s)).collect()
}

fn normalize_ext(s: &str) -> String {
    s.trim().trim_start_matches('.').to_ascii_lowercase()
}
