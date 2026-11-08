//! Typed, cancellable archive sessions and transactional patching.
use crate::splice::{self, Change, Directory};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Instant,
};
#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("{path}: {source}")]
    AtPath {
        path: PathBuf,
        source: Box<EngineError>,
    },
    #[error("{0}")]
    Io(#[from] std::io::Error),
    #[error("invalid archive or plan: {0}")]
    Invalid(String),
    #[error("corrupt archive {chain:?}: {detail}")]
    Corrupt { chain: Chain, detail: String },
    #[error("signed jar cannot be modified: {chain:?}")]
    SignedJar { chain: Chain },
    #[error("file in use")]
    FileLocked,
    #[error("insufficient free disk space")]
    NoSpace,
    #[error("verification failed: {0}")]
    VerifyFailed(String),
    #[error("cancelled")]
    Cancelled,
}
impl EngineError {
    pub fn with_path(self, path: impl Into<PathBuf>) -> Self {
        Self::AtPath {
            path: path.into(),
            source: Box::new(self),
        }
    }
}
impl Serialize for EngineError {
    fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        if let Self::AtPath { path, source } = self {
            let mut value = serde_json::to_value(source).map_err(serde::ser::Error::custom)?;
            let paths = value
                .as_object_mut()
                .ok_or_else(|| serde::ser::Error::custom("error object"))?
                .entry("paths")
                .or_insert_with(|| serde_json::json!([]));
            if let Some(paths) = paths.as_array_mut() {
                paths.push(serde_json::json!(path));
            }
            value["message"] = serde_json::json!(self.to_string());
            return value.serialize(s);
        }
        let mut v = s.serialize_struct(
            "EngineError",
            if matches!(self, Self::Corrupt { .. }) {
                4
            } else if matches!(self, Self::Io(_)) {
                5
            } else {
                3
            },
        )?;
        v.serialize_field(
            "kind",
            match self {
                Self::Io(_) => "io",
                Self::AtPath { .. } => "unknown",
                Self::Invalid(_) => "invalid",
                Self::Corrupt { .. } => "corrupt",
                Self::SignedJar { .. } => "signed_jar",
                Self::FileLocked => "file_locked",
                Self::NoSpace => "no_space",
                Self::VerifyFailed(_) => "verify_failed",
                Self::Cancelled => "cancelled",
            },
        )?;
        v.serialize_field("message", &self.to_string())?;
        v.serialize_field(
            "chain",
            &match self {
                Self::SignedJar { chain } | Self::Corrupt { chain, .. } => Some(chain),
                _ => None,
            },
        )?;
        if let Self::Io(error) = self {
            v.serialize_field("io_kind", &format!("{:?}", error.kind()))?;
            v.serialize_field("os_message", &error.to_string())?;
        }
        if let Self::Corrupt { detail, .. } = self {
            v.serialize_field("detail", detail)?;
        }
        v.end()
    }
}
pub type Result<T> = std::result::Result<T, EngineError>;
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub struct Chain(pub Vec<String>);
impl From<&str> for Chain {
    fn from(s: &str) -> Self {
        Self(if s.is_empty() {
            vec![]
        } else {
            s.split("!/").map(str::to_owned).collect()
        })
    }
}
#[derive(Clone, Default)]
pub struct CancelToken(Arc<AtomicBool>);
impl CancelToken {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Relaxed);
    }
    pub fn check(&self) -> Result<()> {
        if self.0.load(Ordering::Relaxed) {
            Err(EngineError::Cancelled)
        } else {
            Ok(())
        }
    }
}
#[derive(Clone, Debug, Serialize)]
pub enum Phase {
    Inflate,
    Rewrite,
    Verify,
    Commit,
}
#[derive(Clone, Debug, Serialize)]
pub struct ProgressEvent {
    pub phase: Phase,
    pub done: u64,
    pub total: u64,
    pub chain: Chain,
}
pub trait Progress {
    fn on(&self, ev: ProgressEvent);
}
impl<F: Fn(ProgressEvent)> Progress for F {
    fn on(&self, ev: ProgressEvent) {
        self(ev)
    }
}
fn event(p: &dyn Progress, phase: Phase, chain: &Chain, done: u64, total: u64) {
    p.on(ProgressEvent {
        phase,
        done,
        total,
        chain: chain.clone(),
    });
}
pub struct Source {
    file: File,
    start: u64,
    len: u64,
    pos: u64,
}
impl Source {
    fn open(p: &Path) -> Result<Self> {
        let file = File::open(p).map_err(|e| EngineError::from(e).with_path(p))?;
        let len = file.metadata()?.len();
        Ok(Self {
            file,
            start: 0,
            len,
            pos: 0,
        })
    }
}
impl Read for Source {
    fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
        let n = b.len().min(self.len.saturating_sub(self.pos) as usize);
        self.file.seek(SeekFrom::Start(self.start + self.pos))?;
        let n = self.file.read(&mut b[..n])?;
        self.pos += n as u64;
        Ok(n)
    }
}
impl Seek for Source {
    fn seek(&mut self, s: SeekFrom) -> std::io::Result<u64> {
        let p = match s {
            SeekFrom::Start(n) => n as i128,
            SeekFrom::Current(n) => self.pos as i128 + n as i128,
            SeekFrom::End(n) => self.len as i128 + n as i128,
        };
        if p < 0 || p > u64::MAX as i128 {
            return Err(std::io::Error::other("invalid seek"));
        }
        self.pos = p as u64;
        Ok(self.pos)
    }
}
/// Short, private session directory. Keep std's Windows verbatim prefix:
/// stripping it with display-path normalization would reintroduce MAX_PATH.
pub fn session_tempdir() -> std::io::Result<tempfile::TempDir> {
    let parent = std::env::temp_dir().canonicalize()?;
    tempfile::Builder::new().prefix("zr-").tempdir_in(parent)
}

/// Resolve the existing parent before creating a file (std preserves \\?\ on Windows).
pub fn creation_path(path: &Path) -> std::io::Result<PathBuf> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let name = path
        .file_name()
        .ok_or_else(|| std::io::Error::other("missing filename"))?;
    Ok(parent.canonicalize()?.join(name))
}

fn cache_path(dir: &Path, key: &str, salt: u64) -> PathBuf {
    let mut hash = Sha256::new();
    hash.update(key.as_bytes());
    hash.update(salt.to_le_bytes());
    let hash = hash.finalize();
    let name = hash[..8]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    dir.join(format!("{name}.bin"))
}

// Cache only owns paths and metadata; each operation opens its own Source.
// Callers must drop returned Sources before starting a mutation.
pub struct Cache {
    crc_nanos: AtomicU64,
    crc_bytes: AtomicU64,
    dir: tempfile::TempDir,
    files: Mutex<BTreeMap<String, PathBuf>>,
    verified: Mutex<BTreeMap<String, std::result::Result<(), String>>>,
    problems: Mutex<BTreeMap<Chain, String>>,
    signatures: Mutex<BTreeMap<String, bool>>,
}
impl Cache {
    pub fn new() -> Result<Self> {
        Ok(Self {
            crc_nanos: AtomicU64::new(0),
            crc_bytes: AtomicU64::new(0),
            dir: session_tempdir()?,
            files: Mutex::new(BTreeMap::new()),
            verified: Mutex::new(BTreeMap::new()),
            problems: Mutex::new(BTreeMap::new()),
            signatures: Mutex::new(BTreeMap::new()),
        })
    }
    pub fn clear(&self) -> Result<()> {
        let mut files = self
            .files
            .lock()
            .map_err(|_| EngineError::Invalid("cache lock".into()))?;
        for p in files.values() {
            fs::remove_file(p)?;
        }
        files.clear();
        self.verified
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
        self.problems
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
        self.signatures
            .lock()
            .map_err(|_| EngineError::Invalid("cache lock".into()))?
            .clear();
        Ok(())
    }
    /// Cumulative cost of first-access STORED CRC verification in this session.
    pub fn crc_metrics(&self) -> (u64, u64) {
        (
            self.crc_bytes.load(Ordering::Relaxed),
            self.crc_nanos.load(Ordering::Relaxed),
        )
    }
    pub fn problems(&self) -> Vec<CorruptArchive> {
        self.problems
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .map(|(chain, detail)| CorruptArchive {
                chain: chain.clone(),
                detail: detail.clone(),
            })
            .collect()
    }
    pub(crate) fn corrupt(&self, chain: &Chain, detail: impl ToString) -> EngineError {
        let detail = detail.to_string();
        self.problems
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(chain.clone(), detail.clone());
        EngineError::Corrupt {
            chain: chain.clone(),
            detail,
        }
    }
    pub fn open(
        &self,
        root: &Path,
        chain: &Chain,
        progress: &dyn Progress,
        cancel: &CancelToken,
    ) -> Result<Source> {
        let root = root
            .canonicalize()
            .map_err(|e| EngineError::from(e).with_path(root))?;
        let meta = fs::metadata(&root)?;
        let mut s = Source::open(&root)?;
        let mut path = Chain::default();
        for name in &chain.0 {
            cancel.check()?;
            path.0.push(name.clone());
            let d = Directory::read(&mut s)
                .map_err(|e| self.corrupt(&Chain(path.0[..path.0.len() - 1].to_vec()), e))?;
            let rec = d
                .records
                .iter()
                .find(|r| r.name == *name)
                .ok_or_else(|| EngineError::Invalid(format!("entry not found: {name}")))?;
            let at = rec
                .data_offset(&mut s)
                .map_err(|e| self.corrupt(&path, e))?;
            let key = format!("{root:?}:{}:{:?}:{path:?}", meta.len(), meta.modified()?);
            if rec.method() == 0 {
                // Verify a seek view only when first accessed, then reuse the result.
                let mut verified = self
                    .verified
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let result = if let Some(result) = verified.get(&key) {
                    result.clone()
                } else {
                    let began = Instant::now();
                    s.seek(SeekFrom::Start(at))?;
                    let mut remaining = rec.size;
                    let mut hash = crc32fast::Hasher::new();
                    let mut buffer = [0; 128 * 1024];
                    while remaining > 0 {
                        cancel.check()?;
                        let limit = remaining.min(buffer.len() as u64) as usize;
                        let n = s.read(&mut buffer[..limit])?;
                        if n == 0 {
                            break;
                        }
                        hash.update(&buffer[..n]);
                        remaining -= n as u64;
                    }
                    let result = if remaining != 0 || hash.finalize() != rec.crc() {
                        Err("CRC/size mismatch".to_owned())
                    } else {
                        Ok(())
                    };
                    self.crc_nanos
                        .fetch_add(began.elapsed().as_nanos() as u64, Ordering::Relaxed);
                    self.crc_bytes
                        .fetch_add(rec.size - remaining, Ordering::Relaxed);
                    verified.insert(key.clone(), result.clone());
                    result
                };
                result.map_err(|detail| self.corrupt(&path, detail))?;
                s.start += at;
                s.len = rec.size;
                s.pos = 0;
                continue;
            }
            if rec.method() != 8 {
                return Err(EngineError::Invalid("unsupported compression".into()));
            }
            let mut files = self
                .files
                .lock()
                .map_err(|_| EngineError::Invalid("cache lock".into()))?;
            if let Some(p) = files.get(&key) {
                s = Source::open(p)?;
                event(progress, Phase::Inflate, &path, rec.size, rec.size);
                continue;
            }
            event(progress, Phase::Inflate, &path, 0, rec.size);
            s.seek(SeekFrom::Start(at))?;
            let mut decoder = flate2::read::DeflateDecoder::new(s.take(rec.compressed));
            let mut tmp = tempfile::NamedTempFile::new_in(self.dir.path())
                .map_err(|e| EngineError::from(e).with_path(self.dir.path()))?;
            let mut h = crc32fast::Hasher::new();
            let mut n = 0;
            let mut b = [0; 128 * 1024];
            loop {
                cancel.check()?;
                let k = decoder.read(&mut b).map_err(|e| self.corrupt(&path, e))?;
                if k == 0 {
                    break;
                }
                h.update(&b[..k]);
                tmp.write_all(&b[..k])
                    .map_err(|e| EngineError::from(e).with_path(tmp.path()))?;
                n += k as u64;
                event(progress, Phase::Inflate, &path, n, rec.size);
            }
            if n != rec.size || h.finalize() != rec.crc() {
                return Err(self.corrupt(&path, "CRC/size mismatch"));
            }
            drop(decoder);
            // The logical key stays in memory; no archive names enter filesystem paths.
            // Salt collisions instead of aliasing two distinct cache entries.
            let mut salt = 0u64;
            let p = loop {
                let p = cache_path(self.dir.path(), &key, salt);
                if !files.values().any(|existing| existing == &p) {
                    break p;
                }
                salt += 1;
            };
            tmp.into_temp_path()
                .persist_noclobber(&p)
                .map_err(|e| EngineError::Io(e.error).with_path(&p))?;
            s = Source::open(&p)?;
            files.insert(key, p);
        }
        Ok(s)
    }
    pub fn list_children(&self, root: &Path, chain: &Chain) -> Result<Vec<EntryInfo>> {
        self.list_children_with_progress(root, chain, &|_| {}, &CancelToken::default())
    }
    pub fn list_children_with_progress(
        &self,
        root: &Path,
        chain: &Chain,
        progress: &dyn Progress,
        cancel: &CancelToken,
    ) -> Result<Vec<EntryInfo>> {
        let mut archive = chain.clone();
        let prefix = if archive.0.last().is_some_and(|n| n.ends_with('/')) {
            archive.0.pop().unwrap_or_default()
        } else {
            String::new()
        };
        let mut s = self.open(root, &archive, progress, cancel)?;
        let canonical = root.canonicalize()?;
        let meta = fs::metadata(&canonical)?;
        let modified = meta.modified()?;
        let signatures = self
            .signatures
            .lock()
            .map_err(|_| EngineError::Invalid("cache lock".into()))?;
        Ok(Directory::read(&mut s)
            .map_err(|e| self.corrupt(&archive, e))?
            .records
            .into_iter()
            .filter(|r| r.name.starts_with(&prefix) && r.name != prefix)
            .map(|r| EntryInfo {
                corrupt: {
                    let mut child = archive.clone();
                    child.0.push(r.name.clone());
                    self.problems
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .get(&child)
                        .cloned()
                },
                name: r.name.clone(),
                raw_name: r.raw_name.clone(),
                name_is_utf8: std::str::from_utf8(&r.raw_name).is_ok(),
                is_dir: r.name.ends_with('/'),
                method: match r.method() {
                    0 => "stored",
                    8 => "deflated",
                    _ => "other",
                }
                .into(),
                compressed_size: r.compressed,
                size: r.size,
                crc32: r.crc(),
                mtime: ((splice::u16le(&r.raw, 14) as u32) << 16)
                    | splice::u16le(&r.raw, 12) as u32,
                is_archive: is_archive(&r.name),
                is_signed: {
                    let mut child = archive.clone();
                    child.0.push(r.name.clone());
                    signatures
                        .get(&format!(
                            "{canonical:?}:{}:{:?}:{child:?}",
                            meta.len(),
                            modified
                        ))
                        .copied()
                },
            })
            .collect())
    }
    pub fn is_signed(&self, root: &Path, chain: &Chain) -> Result<bool> {
        let canonical = root.canonicalize()?;
        let meta = fs::metadata(&canonical)?;
        let key = format!(
            "{canonical:?}:{}:{:?}:{chain:?}",
            meta.len(),
            meta.modified()?
        );
        let mut source = self.open(root, chain, &|_| {}, &CancelToken::default())?;
        let value = signed(&Directory::read(&mut source).map_err(|e| self.corrupt(chain, e))?);
        self.signatures
            .lock()
            .map_err(|_| EngineError::Invalid("cache lock".into()))?
            .insert(key, value);
        Ok(value)
    }
    pub fn read_entry(&self, root: &Path, chain: &Chain) -> Result<Source> {
        self.open(root, chain, &|_| {}, &CancelToken::default())
    }
    pub fn extract(&self, root: &Path, chain: &Chain, dest: &Path) -> Result<()> {
        let mut s = self.read_entry(root, chain)?;
        let dest = creation_path(dest)?;
        let mut f = File::create(&dest).map_err(|e| EngineError::from(e).with_path(&dest))?;
        splice::copy(&mut s, &mut f, &CancelToken::default())?;
        Ok(())
    }
}
fn is_archive(n: &str) -> bool {
    [".jar", ".zip", ".war", ".ear"]
        .iter()
        .any(|e| n.to_ascii_lowercase().ends_with(e))
}
#[derive(Clone, Debug, Serialize)]
pub struct CorruptArchive {
    pub chain: Chain,
    pub detail: String,
}
#[derive(Debug, Serialize)]
pub struct EntryInfo {
    pub corrupt: Option<String>,
    pub name: String,
    pub raw_name: Vec<u8>,
    pub name_is_utf8: bool,
    pub is_dir: bool,
    pub method: String,
    pub compressed_size: u64,
    pub size: u64,
    pub crc32: u32,
    pub mtime: u32,
    pub is_archive: bool,
    pub is_signed: Option<bool>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Plan {
    pub root: PathBuf,
    pub ops: Vec<Op>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Op {
    Replace {
        target: Chain,
        source: PathBuf,
    },
    Add {
        parent: Chain,
        name: String,
        source: PathBuf,
    },
}
#[derive(Debug, Serialize, Deserialize)]
pub struct ApplyOptions {
    pub backup: bool,
    pub write_log: bool,
}
impl Default for ApplyOptions {
    fn default() -> Self {
        Self {
            backup: true,
            write_log: true,
        }
    }
}
#[derive(Debug, Serialize)]
pub struct OpResult {
    pub chain: Chain,
    pub source: PathBuf,
    pub before: Option<String>,
    pub after: String,
}
#[derive(Debug, Serialize)]
pub struct ApplyReport {
    pub output_path: PathBuf,
    pub backup_path: Option<PathBuf>,
    pub log_path: Option<PathBuf>,
    pub results: Vec<OpResult>,
    pub rewritten_chains: Vec<Chain>,
    pub durations_ms: BTreeMap<String, u128>,
}
fn hash(mut r: impl Read, c: &CancelToken) -> Result<String> {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    let mut b = [0; 128 * 1024];
    loop {
        c.check()?;
        let n = r.read(&mut b)?;
        if n == 0 {
            break;
        }
        h.update(&b[..n]);
    }
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}
fn signed(d: &Directory) -> bool {
    let names: std::collections::BTreeSet<String> = d
        .records
        .iter()
        .filter(|r| r.size > 0)
        .map(|r| r.name.to_ascii_uppercase())
        .filter(|n| n.starts_with("META-INF/") && !n[9..].contains('/') && n.ends_with(".SF"))
        .collect();
    d.records.iter().any(|r| {
        let n = r.name.to_ascii_uppercase();
        if !n.starts_with("META-INF/") || n[9..].contains('/') {
            return false;
        }
        [".RSA", ".DSA", ".EC"].iter().any(|ext| {
            n.strip_suffix(ext)
                .is_some_and(|base| names.contains(&format!("{base}.SF")))
        })
    })
}
/// A no-sharing probe must never overlap engine-owned readers. Its handle is
/// released here, before callers read, back up, or replace the archive.
pub(crate) fn probe_lock(path: &Path) -> Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        let probe = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .share_mode(0)
            .open(path)
            .map_err(|e| match e.raw_os_error() {
                Some(32 | 33) => EngineError::FileLocked,
                _ => EngineError::Io(e),
            })?;
        drop(probe);
    }
    #[cfg(not(windows))]
    let _ = path;
    Ok(())
}

pub fn apply(
    plan: &Plan,
    opts: &ApplyOptions,
    cache: &Cache,
    progress: &dyn Progress,
    cancel: &CancelToken,
) -> Result<ApplyReport> {
    cancel.check()?;
    let timing = InflateTiming {
        next: progress,
        started: std::cell::RefCell::new(BTreeMap::new()),
        milliseconds: std::cell::Cell::new(0),
    };
    let progress: &dyn Progress = &timing;
    let began = Instant::now();
    // Probe before opening any readers, and close the probe before doing work.
    probe_lock(&plan.root)?;
    let root = plan.root.canonicalize()?;
    let parent = root
        .parent()
        .ok_or_else(|| EngineError::Invalid("root parent".into()))?;
    let meta = fs::metadata(&root)?;
    if fs2::available_space(parent)? < meta.len().saturating_mul(if opts.backup { 3 } else { 2 }) {
        return Err(EngineError::NoSpace);
    }
    let mut targets = Vec::new();
    let mut source_bytes = 0u64;
    for op in &plan.ops {
        let (chain, source) = match op {
            Op::Replace { target, source } => (target.clone(), source),
            Op::Add {
                parent,
                name,
                source,
            } => {
                let mut chain = parent.clone();
                chain.0.push(name.clone());
                (chain, source)
            }
        };
        source_bytes = source_bytes.saturating_add(
            fs::metadata(source)
                .map_err(|e| EngineError::from(e).with_path(source))?
                .len(),
        );
        targets.push(chain);
    }
    targets.sort();
    if targets
        .windows(2)
        .any(|pair| pair[1].0.starts_with(&pair[0].0))
    {
        return Err(EngineError::Invalid(
            "duplicate or overlapping operations".into(),
        ));
    }
    let depth = targets.iter().map(|c| c.0.len() as u64).max().unwrap_or(1);
    if fs2::available_space(parent)?
        < meta
            .len()
            .saturating_mul(depth + 2)
            .saturating_add(source_bytes)
    {
        return Err(EngineError::NoSpace);
    }
    let mut groups: BTreeMap<Chain, BTreeMap<String, Change>> = BTreeMap::new();
    let mut results = Vec::new();
    for op in &plan.ops {
        cancel.check()?;
        let (mut chain, source, add) = match op {
            Op::Replace { target, source } => (target.clone(), source, false),
            Op::Add {
                parent,
                name,
                source,
            } => {
                let mut c = parent.clone();
                c.0.push(name.clone());
                (c, source, true)
            }
        };
        let full = chain.clone();
        let name = chain
            .0
            .pop()
            .ok_or_else(|| EngineError::Invalid("empty target".into()))?;
        if name.starts_with('/') || name.split('/').any(|s| s == "..") {
            return Err(EngineError::Invalid("unsafe entry name".into()));
        }
        let after = hash(
            File::open(source).map_err(|e| EngineError::from(e).with_path(source))?,
            cancel,
        )?;
        for depth in 0..=chain.0.len() {
            let ancestor = Chain(chain.0[..depth].to_vec());
            let mut s = cache.open(&root, &ancestor, progress, cancel)?;
            let d = Directory::read(&mut s).map_err(|e| cache.corrupt(&ancestor, e))?;
            if signed(&d) {
                return Err(EngineError::SignedJar { chain: ancestor });
            }
        }
        let mut s = cache.open(&root, &chain, progress, cancel)?;
        let d = Directory::read(&mut s)?;
        let exists = d.records.iter().any(|r| r.name == name);
        if exists == add {
            return Err(EngineError::Invalid(format!(
                "target not found or already exists: {full:?}"
            )));
        }
        let before = if exists {
            Some(hash(cache.open(&root, &full, progress, cancel)?, cancel)?)
        } else {
            None
        };
        if groups
            .entry(chain)
            .or_default()
            .insert(
                name,
                Change {
                    source: source.clone(),
                },
            )
            .is_some()
        {
            return Err(EngineError::Invalid("duplicate target".into()));
        }
        results.push(OpResult {
            chain: full,
            source: source.clone(),
            before,
            after,
        });
    }
    let mut report = ApplyReport {
        output_path: root.clone(),
        backup_path: None,
        log_path: None,
        results,
        rewritten_chains: vec![],
        durations_ms: BTreeMap::new(),
    };
    report
        .durations_ms
        .insert("preflight".into(), began.elapsed().as_millis());
    let start = Instant::now();
    let inflate_before_rewrite = timing.milliseconds.get();
    let mut temps = Vec::new();
    let mut final_file = None;
    while !groups.is_empty() {
        let chain = groups
            .keys()
            .max_by_key(|c| c.0.len())
            .cloned()
            .ok_or_else(|| EngineError::Invalid("empty groups".into()))?;
        let mut changes = groups
            .remove(&chain)
            .ok_or_else(|| EngineError::Invalid("missing group".into()))?;
        let mut s = cache.open(&root, &chain, progress, cancel)?;
        prepare_additions(&mut s, &mut changes, &mut temps, cancel)?;
        let mut tmp = tempfile::NamedTempFile::new_in(parent)?;
        event(progress, Phase::Rewrite, &chain, 0, s.len);
        let total = s.len;
        let mut writer = ProgressWriter {
            inner: &mut tmp,
            progress,
            chain: &chain,
            total,
            done: 0,
        };
        splice::rewrite(&mut s, &mut writer, &changes, cancel)?;
        let written = writer.done;
        event(progress, Phase::Rewrite, &chain, written, written);
        #[cfg(test)]
        CORRUPT_OUTPUT.with(|enabled| {
            if enabled.get() {
                let _ = tmp.as_file_mut().seek(SeekFrom::Start(0));
                let _ = tmp.as_file_mut().write_all(b"BAD!");
            }
        });
        drop(s);
        tmp.as_file().sync_all()?;
        let tmp = tmp.into_temp_path();
        event(
            progress,
            Phase::Verify,
            &chain,
            0,
            fs::metadata(&tmp)?.len(),
        );
        let verify_start = Instant::now();
        verify(&tmp, &changes, cancel)?;
        let verified = fs::metadata(&tmp)?.len();
        event(progress, Phase::Verify, &chain, verified, verified);
        *report.durations_ms.entry("verify".into()).or_default() +=
            verify_start.elapsed().as_millis();
        report.rewritten_chains.push(chain.clone());
        if chain.0.is_empty() {
            final_file = Some(tmp);
            break;
        }
        let mut parent_chain = chain.clone();
        let name = parent_chain
            .0
            .pop()
            .ok_or_else(|| EngineError::Invalid("parent".into()))?;
        if groups
            .entry(parent_chain)
            .or_default()
            .insert(
                name,
                Change {
                    source: tmp.to_path_buf(),
                },
            )
            .is_some()
        {
            return Err(EngineError::Invalid("overlapping operations".into()));
        }
        temps.push(tmp);
    }
    report
        .durations_ms
        .insert("rewrite_and_verify".into(), start.elapsed().as_millis());
    let rewrite_ms = start
        .elapsed()
        .as_millis()
        .saturating_sub(*report.durations_ms.get("verify").unwrap_or(&0))
        .saturating_sub(timing.milliseconds.get() - inflate_before_rewrite);
    report.durations_ms.insert("rewrite".into(), rewrite_ms);
    report
        .durations_ms
        .insert("inflate".into(), timing.milliseconds.get());
    if let Some(tmp) = final_file {
        cancel.check()?;
        {
            // FlushFileBuffers requires write access on Windows. Open before
            // restoring permissions, then close before backup or replacement.
            let sync_file = fs::OpenOptions::new().write(true).open(&tmp)?;
            fs::set_permissions(&tmp, meta.permissions())?;
            preserve_metadata(&root, &tmp)?;
            sync_file.sync_all()?;
        }
        // Nested outputs are no longer needed; none retain an open handle.
        drop(temps);
        let stamp = chrono::Local::now();
        let suffix = filename_timestamp(stamp);
        event(progress, Phase::Commit, &Chain::default(), 0, meta.len());
        cancel.check()?;
        let current = fs::metadata(&root)?;
        if current.len() != meta.len() || current.modified()? != meta.modified()? {
            return Err(EngineError::Invalid("root changed during apply".into()));
        }
        let commit_start = Instant::now();
        if opts.backup {
            for attempt in 0..1000 {
                let extra = if attempt == 0 {
                    String::new()
                } else {
                    format!("-{attempt}")
                };
                let path = PathBuf::from(format!("{}.bak-{suffix}{extra}", root.display()));
                match fs::hard_link(&root, &path) {
                    Ok(()) => {
                        report.backup_path = Some(path);
                        break;
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                    Err(_) => {
                        match fs::OpenOptions::new()
                            .write(true)
                            .create_new(true)
                            .open(&path)
                        {
                            Ok(mut f) => {
                                let copied = File::open(&root)
                                    .and_then(|mut source| std::io::copy(&mut source, &mut f))
                                    .and_then(|_| f.sync_all());
                                drop(f);
                                if let Err(e) = copied {
                                    let _ = fs::remove_file(&path);
                                    return Err(e.into());
                                }
                                report.backup_path = Some(path);
                                break;
                            }
                            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                            Err(e) => return Err(e.into()),
                        }
                    }
                }
            }
            if report.backup_path.is_none() {
                return Err(EngineError::Invalid("backup namespace exhausted".into()));
            }
        }
        // Prepare the complete log before replacing the original; serialization or
        // disk errors cannot turn a failed apply into a modified target.
        report
            .durations_ms
            .insert("commit".into(), commit_start.elapsed().as_millis());
        report
            .durations_ms
            .insert("total".into(), began.elapsed().as_millis());
        let mut log_guard = None;
        if opts.write_log {
            let mut log = tempfile::NamedTempFile::new_in(parent)?;
            let path = PathBuf::from(format!(
                "{}.patch-{suffix}-{}.json",
                root.display(),
                stamp.timestamp_subsec_nanos()
            ));
            report.log_path = Some(path.clone());
            serde_json::to_writer_pretty(&mut log, &report)
                .map_err(|e| EngineError::Invalid(e.to_string()))?;
            log.as_file().sync_all()?;
            log.into_temp_path()
                .persist_noclobber(&path)
                .map_err(|e| EngineError::Io(e.error))?;
            log_guard = Some(path);
        }
        if let Err(e) = cancel.check() {
            if let Some(path) = &log_guard {
                let _ = fs::remove_file(path);
            }
            if let Some(path) = &report.backup_path {
                let _ = fs::remove_file(path);
            }
            return Err(e);
        }
        if let Err(e) = commit_file(tmp, &root) {
            if let Some(path) = log_guard {
                let _ = fs::remove_file(path);
            }
            return Err(e);
        }
        report
            .durations_ms
            .insert("commit".into(), commit_start.elapsed().as_millis());
        event(
            progress,
            Phase::Commit,
            &Chain::default(),
            meta.len(),
            meta.len(),
        );
    }
    report
        .durations_ms
        .insert("total".into(), began.elapsed().as_millis());
    Ok(report)
}
fn verify(path: &Path, changes: &BTreeMap<String, Change>, cancel: &CancelToken) -> Result<()> {
    let mut raw = File::open(path)?;
    let directory =
        Directory::read(&mut raw).map_err(|e| EngineError::VerifyFailed(e.to_string()))?;
    for name in changes.keys() {
        cancel.check()?;
        let rec = directory
            .records
            .iter()
            .find(|r| &r.name == name)
            .ok_or_else(|| EngineError::VerifyFailed(format!("missing {name}")))?;
        let offset = rec
            .data_offset(&mut raw)
            .map_err(|e| EngineError::VerifyFailed(e.to_string()))?;
        raw.seek(SeekFrom::Start(offset))?;
        let mut reader: Box<dyn Read + '_> = match rec.method() {
            0 => Box::new((&mut raw).take(rec.compressed)),
            8 => Box::new(flate2::read::DeflateDecoder::new(
                (&mut raw).take(rec.compressed),
            )),
            _ => return Err(EngineError::VerifyFailed("unsupported method".into())),
        };
        let mut crc = crc32fast::Hasher::new();
        let mut size = 0;
        let mut buf = [0; 128 * 1024];
        loop {
            cancel.check()?;
            let n = reader
                .read(&mut buf)
                .map_err(|e| EngineError::VerifyFailed(e.to_string()))?;
            if n == 0 {
                break;
            }
            crc.update(&buf[..n]);
            size += n as u64;
        }
        if size != rec.size
            || crc.finalize() != rec.crc()
            || (rec.method() == 0 && rec.compressed != rec.size)
        {
            return Err(EngineError::VerifyFailed(format!("CRC or size: {name}")));
        }
    }
    Ok(())
}
fn prepare_additions(
    src: &mut Source,
    changes: &mut BTreeMap<String, Change>,
    temps: &mut Vec<tempfile::TempPath>,
    cancel: &CancelToken,
) -> Result<()> {
    let d = Directory::read(src)?;
    let added: Vec<_> = changes
        .keys()
        .filter(|n| !d.records.iter().any(|r| &r.name == *n))
        .cloned()
        .collect();
    if d.records.iter().any(|r| r.name.ends_with('/')) {
        for name in &added {
            let mut directory = String::new();
            let parts: Vec<_> = name.split('/').collect();
            for part in &parts[..parts.len() - 1] {
                directory.push_str(part);
                directory.push('/');
                if !d.records.iter().any(|r| r.name == directory)
                    && !changes.contains_key(&directory)
                {
                    let f = tempfile::NamedTempFile::new()?;
                    changes.insert(
                        directory.clone(),
                        Change {
                            source: f.path().to_owned(),
                        },
                    );
                    temps.push(f.into_temp_path());
                }
            }
        }
    }
    for prefix in ["BOOT-INF", "WEB-INF"] {
        let libs: Vec<_> = added
            .iter()
            .filter(|n| {
                n.starts_with(&format!("{prefix}/lib/"))
                    && n.ends_with(".jar")
                    && !n[prefix.len() + 5..].contains('/')
            })
            .collect();
        if libs.is_empty() {
            continue;
        }
        for index in ["classpath.idx", "layers.idx"] {
            let name = format!("{prefix}/{index}");
            let Some(rec) = d.records.iter().find(|r| r.name == name) else {
                continue;
            };
            if changes.contains_key(&name) {
                return Err(EngineError::Invalid(
                    "explicit index replacement conflicts with added jars".into(),
                ));
            }
            let at = rec.data_offset(src)?;
            src.seek(SeekFrom::Start(at))?;
            let mut reader: Box<dyn Read + '_> = if rec.method() == 0 {
                Box::new(src.take(rec.compressed))
            } else {
                Box::new(flate2::read::DeflateDecoder::new(src.take(rec.compressed)))
            };
            let mut text = String::new();
            reader.read_to_string(&mut text)?;
            for lib in &libs {
                if index == "classpath.idx" {
                    if !text.ends_with('\n') {
                        text.push('\n');
                    }
                    text.push_str(&format!("- \"{lib}\"\n"));
                } else {
                    let covered = text
                        .lines()
                        .filter_map(|l| {
                            l.trim()
                                .strip_prefix("- \"")
                                .and_then(|s| s.strip_suffix('"'))
                        })
                        .any(|s| s == lib.as_str() || (s.ends_with('/') && lib.starts_with(s)));
                    if !covered {
                        let mut lines: Vec<String> = text.lines().map(str::to_owned).collect();
                        let layer = lines
                            .iter()
                            .position(|l| l == "- \"application\":")
                            .or_else(|| lines.iter().rposition(|l| l.starts_with("- ")))
                            .ok_or_else(|| {
                                EngineError::Invalid("layers.idx has no layers".into())
                            })?;
                        let end = (layer + 1..lines.len())
                            .find(|&i| lines[i].starts_with("- "))
                            .unwrap_or(lines.len());
                        lines.insert(end, format!("  - \"{lib}\""));
                        text = lines.join("\n") + "\n";
                    }
                }
            }
            cancel.check()?;
            let mut f = tempfile::NamedTempFile::new()?;
            f.write_all(text.as_bytes())?;
            changes.insert(
                name,
                Change {
                    source: f.path().to_owned(),
                },
            );
            temps.push(f.into_temp_path());
        }
    }
    Ok(())
}

struct InflateTiming<'a> {
    next: &'a dyn Progress,
    started: std::cell::RefCell<BTreeMap<Chain, Instant>>,
    milliseconds: std::cell::Cell<u128>,
}
impl Progress for InflateTiming<'_> {
    fn on(&self, event: ProgressEvent) {
        if matches!(event.phase, Phase::Inflate) {
            if event.done == 0 {
                self.started
                    .borrow_mut()
                    .insert(event.chain.clone(), Instant::now());
            }
            if event.done == event.total
                && let Some(start) = self.started.borrow_mut().remove(&event.chain)
            {
                self.milliseconds
                    .set(self.milliseconds.get() + start.elapsed().as_millis());
            }
        }
        self.next.on(event);
    }
}

struct ProgressWriter<'a, W> {
    inner: &'a mut W,
    progress: &'a dyn Progress,
    chain: &'a Chain,
    total: u64,
    done: u64,
}
impl<W: Write> Write for ProgressWriter<'_, W> {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        let n = self.inner.write(b)?;
        self.done += n as u64;
        event(
            self.progress,
            Phase::Rewrite,
            self.chain,
            self.done,
            self.total,
        );
        Ok(n)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}
#[cfg(test)]
thread_local! {static CORRUPT_OUTPUT: std::cell::Cell<bool> = const {std::cell::Cell::new(false)};}
#[cfg(not(windows))]
fn commit_file(tmp: tempfile::TempPath, root: &Path) -> Result<()> {
    tmp.persist(root).map_err(|e| EngineError::Io(e.error))?;
    Ok(())
}
#[cfg(windows)]
fn commit_file(tmp: tempfile::TempPath, root: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    let root: Vec<u16> = root.as_os_str().encode_wide().chain(Some(0)).collect();
    let source: Vec<u16> = tmp.as_os_str().encode_wide().chain(Some(0)).collect();
    // ReplaceFileW merges the replaced file's security descriptor and streams.
    // Both pointers are NUL-terminated and remain live throughout the call.
    let ok = unsafe {
        windows_sys::Win32::Storage::FileSystem::ReplaceFileW(
            root.as_ptr(),
            source.as_ptr(),
            std::ptr::null(),
            0,
            std::ptr::null(),
            std::ptr::null(),
        )
    };
    if ok == 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn preserve_metadata(source: &Path, dest: &Path) -> Result<()> {
    use std::{ffi::CString, os::unix::ffi::OsStrExt};
    unsafe extern "C" {
        fn copyfile(
            from: *const std::ffi::c_char,
            to: *const std::ffi::c_char,
            state: *mut std::ffi::c_void,
            flags: u32,
        ) -> i32;
    }
    let from = CString::new(source.as_os_str().as_bytes())
        .map_err(|_| EngineError::Invalid("NUL in path".into()))?;
    let to = CString::new(dest.as_os_str().as_bytes())
        .map_err(|_| EngineError::Invalid("NUL in path".into()))?;
    // copyfile.h: COPYFILE_ACL=1, COPYFILE_XATTR=4. No data or stat copying.
    // Valid C strings remain alive and the optional state is null.
    let result = unsafe { copyfile(from.as_ptr(), to.as_ptr(), std::ptr::null_mut(), 1 | 4) };
    if result != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}
#[cfg(all(unix, not(target_os = "macos")))]
fn preserve_metadata(source: &Path, dest: &Path) -> Result<()> {
    for name in xattr::list(source)? {
        if let Some(value) = xattr::get(source, &name)? {
            xattr::set(dest, &name, &value)?;
        }
    }
    Ok(())
}
#[cfg(not(unix))]
fn preserve_metadata(_source: &Path, _dest: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn long_archive_chain_uses_short_cache_paths() {
        let dir = session_tempdir().unwrap();
        let root = dir.path().join("root.zip");
        let name = format!("{}/boot.jar", "component/".repeat(40));
        assert!(name.len() > 300);
        let mut z = zip::ZipWriter::new(File::create(&root).unwrap());
        z.start_file(
            &name,
            zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated),
        )
        .unwrap();
        z.write_all(b"inflated content").unwrap();
        z.finish().unwrap();
        let cache = Cache::new().unwrap();
        for _ in 0..2 {
            let mut source = cache
                .open(
                    &root,
                    &Chain(vec![name.clone()]),
                    &|_| {},
                    &CancelToken::default(),
                )
                .unwrap();
            let mut data = Vec::new();
            source.read_to_end(&mut data).unwrap();
            assert_eq!(data, b"inflated content");
        }
        let files = cache.files.lock().unwrap();
        assert_eq!(files.len(), 1);
        let path = files.values().next().unwrap();
        assert_eq!(path.file_name().unwrap().to_string_lossy().len(), 20);
        // A representative Windows TEMP root, independent of the test runner's TEMP.
        let windows = Path::new(r"C:\Users\example\AppData\Local\Temp\zr-123456");
        assert!(cache_path(windows, &name, 0).to_string_lossy().len() < 120);
        assert_ne!(cache_path(windows, &name, 0), cache_path(windows, &name, 1));
        assert_ne!(
            cache_path(windows, &name, 0),
            cache_path(windows, "other", 0)
        );
        drop(files);
        cache.clear().unwrap();
        assert_eq!(fs::read_dir(cache.dir.path()).unwrap().count(), 0);
    }

    #[cfg(windows)]
    #[test]
    fn creates_files_beyond_max_path() {
        let dir = session_tempdir().unwrap();
        assert!(dir.path().to_string_lossy().starts_with(r"\\?\"));
        let mut parent = dir.path().to_path_buf();
        for _ in 0..8 {
            parent.push("long-directory-name-for-path-regression");
        }
        fs::create_dir_all(&parent).unwrap();
        let path = creation_path(&parent.join("output.bin")).unwrap();
        assert!(path.as_os_str().len() > 260);
        fs::write(&path, b"ok").unwrap();
        assert_eq!(fs::read(path).unwrap(), b"ok");
    }

    #[test]
    fn verification_failure_keeps_original() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root.zip");
        let source = dir.path().join("source");
        fs::write(&source, b"new").unwrap();
        let mut z = zip::ZipWriter::new(File::create(&root).unwrap());
        z.start_file("x", zip::write::SimpleFileOptions::default())
            .unwrap();
        z.write_all(b"old").unwrap();
        z.finish().unwrap();
        let before = fs::read(&root).unwrap();
        CORRUPT_OUTPUT.with(|v| v.set(true));
        let result = apply(
            &Plan {
                root: root.clone(),
                ops: vec![Op::Replace {
                    target: Chain::from("x"),
                    source,
                }],
            },
            &ApplyOptions::default(),
            &Cache::new().unwrap(),
            &|_| {},
            &CancelToken::default(),
        );
        CORRUPT_OUTPUT.with(|v| v.set(false));
        assert!(
            matches!(result, Err(EngineError::VerifyFailed(_))),
            "{result:?}"
        );
        assert_eq!(fs::read(root).unwrap(), before);
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 2);
    }
}

// Keep human-readable transaction names in the machine's local timezone.
pub(crate) fn filename_timestamp<T: chrono::TimeZone>(stamp: chrono::DateTime<T>) -> String
where
    T::Offset: std::fmt::Display,
{
    stamp.format("%Y%m%d-%H%M%S").to_string()
}

#[cfg(test)]
mod filename_tests {
    use super::*;
    #[test]
    fn transaction_names_use_local_wall_clock_across_date_boundary() {
        let utc = chrono::DateTime::parse_from_rfc3339("2026-11-08T20:04:05Z").unwrap();
        let local = utc.with_timezone(&chrono::FixedOffset::east_opt(8 * 3600).unwrap());
        assert_eq!(filename_timestamp(local), "20261109-040405");
        let local = utc.with_timezone(&chrono::FixedOffset::west_opt(7 * 3600).unwrap());
        assert_eq!(filename_timestamp(local), "20261108-130405");
        let now = chrono::Local::now();
        assert_eq!(
            filename_timestamp(now),
            now.format("%Y%m%d-%H%M%S").to_string()
        );
    }
}
