use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow, bail};

#[derive(Debug, Clone)]
pub struct ZipExpr {
    pub root_archive: PathBuf,
    pub segments: Vec<String>,
}

impl ZipExpr {
    pub fn target(&self) -> Result<&str> {
        self.segments
            .last()
            .map(String::as_str)
            .ok_or_else(|| anyhow!("zip expression has no entry target"))
    }

    pub fn archive_chain(&self) -> &[String] {
        if self.segments.is_empty() {
            &[]
        } else {
            &self.segments[..self.segments.len() - 1]
        }
    }
}

pub fn parse_zip_expr(input: &str) -> Result<ZipExpr> {
    let parts: Vec<&str> = input.split("!/").collect();
    if parts.len() < 2 {
        bail!("invalid zip expression: expected `outer.zip!/path/in/archive`");
    }

    let root_archive = PathBuf::from(parts[0]);
    if root_archive.as_os_str().is_empty() {
        bail!("invalid zip expression: empty root archive");
    }

    let mut segments = Vec::new();
    for p in parts.into_iter().skip(1) {
        let trimmed = p.trim_matches('/');
        if trimmed.is_empty() {
            bail!("invalid zip expression: empty segment");
        }
        segments.push(trimmed.replace('\\', "/"));
    }

    Ok(ZipExpr {
        root_archive,
        segments,
    })
}

pub fn make_expr(prefix: &str, entry_name: &str) -> String {
    if prefix.is_empty() {
        entry_name.to_string()
    } else {
        format!("{prefix}!/{entry_name}")
    }
}

pub fn normalize_path_for_zip(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use super::parse_zip_expr;

    #[test]
    fn parse_basic() {
        let expr = parse_zip_expr("a.zip!/x.txt").expect("parse");
        assert_eq!(expr.root_archive.to_string_lossy(), "a.zip");
        assert_eq!(expr.archive_chain(), &[] as &[String]);
        assert_eq!(expr.target().expect("target"), "x.txt");
    }

    #[test]
    fn parse_nested() {
        let expr = parse_zip_expr("a.zip!/b.jar!/x.txt").expect("parse");
        assert_eq!(expr.archive_chain().len(), 1);
        assert_eq!(expr.archive_chain()[0], "b.jar");
        assert_eq!(expr.target().expect("target"), "x.txt");
    }
}
