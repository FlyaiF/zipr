use std::collections::HashSet;
use std::fs;
use std::path::Path;

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PatchSpec {
    pub version: u32,
    #[serde(default)]
    pub archive: Option<String>,
    #[serde(default)]
    pub generated_at: Option<String>,
    #[serde(default)]
    pub entry: Vec<PatchEntry>,
    #[serde(default)]
    pub unresolved: Vec<Unresolved>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PatchEntry {
    pub target: String,
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default = "default_action")]
    pub action: Action,
    #[serde(default = "default_inherit")]
    pub method: MethodPolicy,
    #[serde(default = "default_level_policy")]
    pub level: LevelPolicy,
    #[serde(default = "default_mtime")]
    pub mtime: MtimePolicy,
    #[serde(default = "default_comment")]
    pub comment: CommentPolicy,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Unresolved {
    pub source: String,
    pub reason: String,
    pub candidates: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Action {
    Replace,
    Delete,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum MethodPolicy {
    Inherit,
    Stored,
    Deflated,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum LevelPolicy {
    Name(LevelName),
    Fixed(u8),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum LevelName {
    Inherit,
    Default,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum MtimePolicy {
    Source,
    Now,
    Keep,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum CommentPolicy {
    Inherit,
}

impl PatchSpec {
    pub fn read_from_file(path: &Path) -> Result<Self> {
        let raw = fs::read_to_string(path)?;
        let spec = toml::from_str::<Self>(&raw)?;
        spec.validate()?;
        Ok(spec)
    }

    pub fn write_to_file(&self, path: &Path) -> Result<()> {
        let raw = toml::to_string_pretty(self)?;
        fs::write(path, raw)?;
        Ok(())
    }

    pub fn validate(&self) -> Result<()> {
        if self.version != 1 {
            bail!(
                "unsupported patch spec version {}, expected 1",
                self.version
            );
        }
        if !self.unresolved.is_empty() {
            bail!("patch spec contains unresolved entries; fix or remove them before apply");
        }

        let mut seen = HashSet::new();
        for e in &self.entry {
            if !seen.insert(e.target.clone()) {
                bail!("duplicate target in patch spec: {}", e.target);
            }
            match e.action {
                Action::Replace => {
                    if e.source.is_none() {
                        bail!("replace entry missing source for target {}", e.target);
                    }
                }
                Action::Delete => {}
            }
            if e.comment != CommentPolicy::Inherit {
                bail!("only comment=inherit is supported");
            }
            match (&e.method, &e.level) {
                (MethodPolicy::Stored, LevelPolicy::Fixed(_)) => {
                    bail!("stored method cannot use a fixed compression level")
                }
                (_, LevelPolicy::Fixed(v)) if *v > 9 => {
                    bail!("compression level must be between 0 and 9")
                }
                _ => {}
            }
        }
        Ok(())
    }
}

fn default_action() -> Action {
    Action::Replace
}

fn default_inherit() -> MethodPolicy {
    MethodPolicy::Inherit
}

fn default_level_policy() -> LevelPolicy {
    LevelPolicy::Name(LevelName::Inherit)
}

fn default_mtime() -> MtimePolicy {
    MtimePolicy::Source
}

fn default_comment() -> CommentPolicy {
    CommentPolicy::Inherit
}

#[cfg(test)]
mod tests {
    use super::{LevelPolicy, PatchSpec};

    #[test]
    fn validate_level_range() {
        let raw = r#"
version = 1

[[entry]]
target = "a.zip!/x.txt"
source = "x.txt"
action = "replace"
method = "deflated"
level = 10
"#;
        let spec = toml::from_str::<PatchSpec>(raw).expect("parse");
        assert!(spec.validate().is_err());
    }

    #[test]
    fn parse_default_level_policy() {
        let raw = r#"
version = 1

[[entry]]
target = "a.zip!/x.txt"
source = "x.txt"
action = "replace"
"#;
        let spec = toml::from_str::<PatchSpec>(raw).expect("parse");
        assert!(matches!(spec.entry[0].level, LevelPolicy::Name(_)));
    }
}
