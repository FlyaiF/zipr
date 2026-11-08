//! JVM metadata and scoped filename matching.
use crate::engine::*;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, io::Read, path::Path};
#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct ClassInfo {
    pub internal_name: String,
    pub major: u16,
}
pub fn class_info(bytes: &[u8]) -> Result<ClassInfo> {
    struct Reader<'a> {
        b: &'a [u8],
        p: usize,
    }
    impl<'a> Reader<'a> {
        fn take(&mut self, n: usize) -> Result<&'a [u8]> {
            let v = self
                .b
                .get(self.p..self.p + n)
                .ok_or_else(|| EngineError::Invalid("truncated class".into()))?;
            self.p += n;
            Ok(v)
        }
        fn u8(&mut self) -> Result<u8> {
            Ok(self.take(1)?[0])
        }
        fn u16(&mut self) -> Result<u16> {
            let b = self.take(2)?;
            Ok(u16::from_be_bytes([b[0], b[1]]))
        }
    }
    let mut r = Reader { b: bytes, p: 0 };
    if r.take(4)? != [0xca, 0xfe, 0xba, 0xbe] {
        return Err(EngineError::Invalid("class magic".into()));
    }
    r.u16()?;
    let major = r.u16()?;
    let count = r.u16()? as usize;
    let mut names = vec![None; count];
    let mut classes = vec![0; count];
    let mut i = 1;
    while i < count {
        match r.u8()? {
            1 => {
                let n = r.u16()? as usize;
                names[i] = Some(String::from_utf8_lossy(r.take(n)?).into_owned());
            }
            7 => classes[i] = r.u16()? as usize,
            3 | 4 | 9 | 10 | 11 | 12 | 17 | 18 => {
                r.take(4)?;
            }
            5 | 6 => {
                r.take(8)?;
                i += 1;
            }
            8 | 16 | 19 | 20 => {
                r.take(2)?;
            }
            15 => {
                r.take(3)?;
            }
            _ => return Err(EngineError::Invalid("constant pool tag".into())),
        }
        i += 1;
    }
    r.u16()?;
    let class = r.u16()? as usize;
    let name = classes
        .get(class)
        .and_then(|n| names.get(*n))
        .and_then(Clone::clone)
        .ok_or_else(|| EngineError::Invalid("this_class".into()))?;
    Ok(ClassInfo {
        internal_name: name,
        major,
    })
}
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Scope {
    pub include_chains: Vec<Chain>,
    pub jar_globs: Vec<String>,
}
pub type Index = BTreeMap<String, Vec<Chain>>;
#[derive(Debug, Serialize)]
pub struct GlobSuggestion {
    pub glob: String,
    pub version: String,
    pub count: usize,
}
impl Cache {
    pub fn index_names(
        &self,
        root: &Path,
        chain: &Chain,
        scope: &Scope,
        progress: &dyn Progress,
        cancel: &CancelToken,
    ) -> Result<Index> {
        let patterns = scope
            .jar_globs
            .iter()
            .map(|p| glob::Pattern::new(p).map_err(|e| EngineError::Invalid(e.to_string())))
            .collect::<Result<Vec<_>>>()?;
        let mut index = Index::new();
        let mut todo = vec![chain.clone()];
        while let Some(parent) = todo.pop() {
            cancel.check()?;
            let entries = match self.list_children_with_progress(root, &parent, progress, cancel) {
                Ok(entries) => entries,
                Err(EngineError::Corrupt { .. }) if !parent.0.is_empty() => continue,
                Err(error) => return Err(error),
            };
            for r in entries {
                cancel.check()?;
                let mut target = parent.clone();
                target.0.push(r.name.clone());
                let file = r.name.rsplit('/').next().unwrap_or(&r.name);
                if file.is_empty() {
                    continue;
                }
                let selected = scope.include_chains.is_empty()
                    || scope.include_chains.iter().any(|c| {
                        target.0.starts_with(&c.0) || {
                            let t = target.0.join("!/");
                            let c = c.0.join("!/");
                            t.starts_with(&format!("{c}/"))
                        }
                    });
                if selected {
                    index
                        .entry(file.to_owned())
                        .or_default()
                        .push(target.clone());
                }
                if [".jar", ".zip", ".war", ".ear"]
                    .iter()
                    .any(|e| file.ends_with(e))
                {
                    let lib =
                        r.name.starts_with("BOOT-INF/lib/") || r.name.starts_with("WEB-INF/lib/");
                    let plugin = r.name.split('/').any(|s| s == "plugins");
                    let explicit = scope
                        .include_chains
                        .iter()
                        .any(|c| c.0.join("!/").contains("plugins/"));
                    if (!plugin || explicit) && (!lib || patterns.iter().any(|p| p.matches(file))) {
                        todo.push(target);
                    }
                }
            }
        }
        Ok(index)
    }
    pub fn suggest_owned_globs(&self, root: &Path, chain: &Chain) -> Result<Vec<GlobSuggestion>> {
        let entries = self.list_children(root, chain)?;
        let mut groups: BTreeMap<(String, String), usize> = BTreeMap::new();
        for e in entries {
            if !e.name.starts_with("BOOT-INF/lib/") {
                continue;
            }
            let n = e
                .name
                .rsplit('/')
                .next()
                .unwrap_or("")
                .trim_end_matches(".jar");
            let parts: Vec<_> = n.split('-').collect();
            if let Some(i) = parts
                .iter()
                .position(|s| s.starts_with(|c: char| c.is_ascii_digit()))
            {
                if i == 0 {
                    continue;
                }
                let prefix = parts[0];
                let version = parts[i..].join("-");
                *groups.entry((format!("{prefix}-*"), version)).or_default() += 1;
            }
        }
        let mut versions: BTreeMap<String, usize> = BTreeMap::new();
        for ((_, v), n) in &groups {
            if v.split('.').count() >= 4 {
                *versions.entry(v.clone()).or_default() += n;
            }
        }
        let dominant = versions.into_iter().max_by_key(|(_, n)| *n).map(|(v, _)| v);
        Ok(groups
            .into_iter()
            .filter(|((_, v), _)| Some(v) == dominant.as_ref())
            .map(|((glob, version), count)| GlobSuggestion {
                glob,
                version,
                count,
            })
            .collect())
    }
    pub fn java_target_of(&self, root: &Path, chain: &Chain) -> Result<Option<u16>> {
        let entries = self.list_children(root, chain)?;
        let mut counts = BTreeMap::new();
        for e in entries
            .iter()
            .filter(|e| e.name.starts_with("BOOT-INF/classes/") && e.name.ends_with(".class"))
            .take(32)
        {
            let mut c = chain.clone();
            c.0.push(e.name.clone());
            let mut b = Vec::new();
            self.read_entry(root, &c)?
                .take(1024 * 1024)
                .read_to_end(&mut b)?;
            if let Ok(info) = class_info(&b) {
                *counts.entry(info.major).or_insert(0) += 1;
            }
        }
        if counts.is_empty() {
            let globs = self.suggest_owned_globs(root, chain)?;
            let patterns: Vec<_> = globs
                .iter()
                .filter_map(|s| glob::Pattern::new(&s.glob).ok())
                .collect();
            for lib in entries
                .iter()
                .filter(|e| {
                    e.name.starts_with("BOOT-INF/lib/")
                        && patterns
                            .iter()
                            .any(|p| p.matches(e.name.rsplit('/').next().unwrap_or("")))
                })
                .take(8)
            {
                let mut nested = chain.clone();
                nested.0.push(lib.name.clone());
                for class in self
                    .list_children(root, &nested)?
                    .into_iter()
                    .filter(|e| e.name.ends_with(".class"))
                    .take(4)
                {
                    let mut target = nested.clone();
                    target.0.push(class.name);
                    let mut b = Vec::new();
                    self.read_entry(root, &target)?
                        .take(1024 * 1024)
                        .read_to_end(&mut b)?;
                    if let Ok(info) = class_info(&b) {
                        *counts.entry(info.major).or_insert(0) += 1;
                    }
                }
            }
        }
        if let Some((major, _)) = counts.into_iter().max_by_key(|(_, n)| *n) {
            return Ok(Some(major));
        }
        if entries.iter().any(|e| e.name == "META-INF/MANIFEST.MF") {
            let mut c = chain.clone();
            c.0.push("META-INF/MANIFEST.MF".into());
            let mut s = String::new();
            self.read_entry(root, &c)?
                .take(1024 * 1024)
                .read_to_string(&mut s)?;
            for line in s.lines() {
                if let Some((k, v)) = line.split_once(':')
                    && ["Build-Jdk-Spec", "Java-Version"].contains(&k)
                {
                    let v = v
                        .trim()
                        .trim_start_matches("1.")
                        .split('.')
                        .next()
                        .unwrap_or("");
                    if let Ok(n) = v.parse::<u16>() {
                        return Ok(Some(n + 44));
                    }
                }
            }
        }
        Ok(None)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_class() {
        let b = [
            0xca, 0xfe, 0xba, 0xbe, 0, 0, 0, 52, 0, 3, 1, 0, 3, b'a', b'/', b'B', 7, 0, 1, 0, 1, 0,
            2,
        ];
        assert_eq!(
            class_info(&b).unwrap(),
            ClassInfo {
                internal_name: "a/B".into(),
                major: 52
            }
        );
        for n in 0..b.len() {
            assert!(class_info(&b[..n]).is_err());
        }
    }
}
