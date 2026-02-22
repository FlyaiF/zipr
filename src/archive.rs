use std::collections::{BTreeSet, HashMap};
use std::fs::{self, File};
use std::io::{Cursor, Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result, anyhow, bail};
use tempfile::Builder;
use time::OffsetDateTime;
use walkdir::WalkDir;
use zip::read::ZipArchive;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, DateTime, ZipWriter};

use crate::config::Config;
use crate::patch_spec::{
    Action, LevelName, LevelPolicy, MethodPolicy, MtimePolicy, PatchEntry, PatchSpec, Unresolved,
};
use crate::path_expr::{ZipExpr, make_expr, normalize_path_for_zip, parse_zip_expr};

#[derive(Debug, Clone)]
struct DraftInput {
    source_norm: String,
    rel_norm: String,
    base: String,
}

#[derive(Debug, Clone)]
pub struct ListedEntry {
    pub expr: String,
    pub size: u64,
    pub compressed_size: u64,
}

#[derive(Debug, Clone)]
pub struct DiffEntry {
    pub path: String,
    pub kind: DiffKind,
    pub content_changed: bool,
    pub metadata_changes: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffKind {
    Added,
    Removed,
    Modified,
}

#[derive(Debug, Clone)]
pub struct DraftSummary {
    pub matched: usize,
    pub unresolved: usize,
}

#[derive(Debug, Clone)]
pub struct ApplySummary {
    pub replaced: usize,
    pub deleted: usize,
}

#[derive(Debug, Clone)]
struct EntryMeta {
    method: CompressionMethod,
    mtime: Option<DateTime>,
    unix_mode: Option<u32>,
}

#[derive(Debug, Clone)]
struct EntrySnapshot {
    size: u64,
    compressed_size: u64,
    crc32: u32,
    method: CompressionMethod,
    mtime: Option<DateTime>,
    unix_mode: Option<u32>,
    comment: String,
}

#[derive(Debug, Clone)]
pub struct ReplaceOptions {
    pub method: MethodPolicy,
    pub level: LevelPolicy,
    pub mtime: MtimePolicy,
}

impl Default for ReplaceOptions {
    fn default() -> Self {
        Self {
            method: MethodPolicy::Inherit,
            level: LevelPolicy::Name(LevelName::Inherit),
            mtime: MtimePolicy::Source,
        }
    }
}

pub fn list_recursive(path: &Path, config: &Config) -> Result<Vec<ListedEntry>> {
    let file = File::open(path)?;
    let mut archive = ZipArchive::new(file)?;
    let mut out = Vec::new();
    let root = path.to_string_lossy().to_string();
    recurse_list(&mut archive, &root, config, &mut out)?;
    Ok(out)
}

pub fn diff_recursive(left: &Path, right: &Path, config: &Config) -> Result<Vec<DiffEntry>> {
    let left_entries = snapshot_archive(left, config)?;
    let right_entries = snapshot_archive(right, config)?;
    let mut keys = BTreeSet::new();
    keys.extend(left_entries.keys().cloned());
    keys.extend(right_entries.keys().cloned());

    let mut out = Vec::new();
    for key in keys {
        match (left_entries.get(&key), right_entries.get(&key)) {
            (None, Some(_r)) => out.push(DiffEntry {
                path: key,
                kind: DiffKind::Added,
                content_changed: true,
                metadata_changes: vec![],
            }),
            (Some(_l), None) => out.push(DiffEntry {
                path: key,
                kind: DiffKind::Removed,
                content_changed: true,
                metadata_changes: vec![],
            }),
            (Some(l), Some(r)) => {
                let content_changed = l.size != r.size || l.crc32 != r.crc32;
                let metadata_changes = diff_metadata(l, r);
                if content_changed || !metadata_changes.is_empty() {
                    out.push(DiffEntry {
                        path: key,
                        kind: DiffKind::Modified,
                        content_changed,
                        metadata_changes,
                    });
                }
            }
            (None, None) => {}
        }
    }
    Ok(out)
}

fn snapshot_archive(path: &Path, config: &Config) -> Result<HashMap<String, EntrySnapshot>> {
    let bytes = fs::read(path)?;
    let mut archive = ZipArchive::new(Cursor::new(bytes))?;
    let mut out = HashMap::new();
    snapshot_recursive(&mut archive, "", config, &mut out)?;
    Ok(out)
}

fn snapshot_recursive<R: Read + Seek>(
    archive: &mut ZipArchive<R>,
    prefix: &str,
    config: &Config,
    out: &mut HashMap<String, EntrySnapshot>,
) -> Result<()> {
    for i in 0..archive.len() {
        let mut file = archive.by_index(i)?;
        if file.is_dir() {
            continue;
        }
        let name = file.name().to_string();
        let path = make_expr(prefix, &name);
        out.insert(
            path.clone(),
            EntrySnapshot {
                size: file.size(),
                compressed_size: file.compressed_size(),
                crc32: file.crc32(),
                method: file.compression(),
                mtime: file.last_modified(),
                unix_mode: file.unix_mode(),
                comment: file.comment().to_string(),
            },
        );

        if config.is_archive_name(&name) {
            let mut nested = Vec::new();
            file.read_to_end(&mut nested)?;
            if let Ok(mut nested_archive) = ZipArchive::new(Cursor::new(nested)) {
                snapshot_recursive(&mut nested_archive, &path, config, out)?;
            }
        }
    }
    Ok(())
}

fn diff_metadata(left: &EntrySnapshot, right: &EntrySnapshot) -> Vec<String> {
    let mut changes = Vec::new();
    if left.method != right.method {
        changes.push(format!("method: {:?} -> {:?}", left.method, right.method));
    }
    if left.compressed_size != right.compressed_size {
        changes.push(format!(
            "compressed_size: {} -> {}",
            left.compressed_size, right.compressed_size
        ));
    }
    if left.unix_mode != right.unix_mode {
        changes.push(format!(
            "unix_mode: {} -> {}",
            fmt_mode(left.unix_mode),
            fmt_mode(right.unix_mode)
        ));
    }
    if left.mtime != right.mtime {
        changes.push(format!(
            "mtime: {} -> {}",
            fmt_time(left.mtime),
            fmt_time(right.mtime)
        ));
    }
    if left.comment != right.comment {
        changes.push(format!(
            "comment: {} -> {}",
            quote_comment(&left.comment),
            quote_comment(&right.comment)
        ));
    }
    changes
}

fn fmt_mode(v: Option<u32>) -> String {
    match v {
        Some(x) => format!("0o{x:o}"),
        None => "none".to_string(),
    }
}

fn fmt_time(v: Option<DateTime>) -> String {
    match v {
        Some(x) => x.to_string(),
        None => "none".to_string(),
    }
}

fn quote_comment(v: &str) -> String {
    format!("{v:?}")
}

fn recurse_list<R: Read + Seek>(
    archive: &mut ZipArchive<R>,
    prefix: &str,
    config: &Config,
    out: &mut Vec<ListedEntry>,
) -> Result<()> {
    for i in 0..archive.len() {
        let mut file = archive.by_index(i)?;
        if file.is_dir() {
            continue;
        }
        let name = file.name().to_string();
        let expr = make_expr(prefix, &name);
        out.push(ListedEntry {
            expr: expr.clone(),
            size: file.size(),
            compressed_size: file.compressed_size(),
        });

        if config.is_archive_name(&name) {
            let mut nested = Vec::new();
            file.read_to_end(&mut nested)?;
            if let Ok(mut nested_archive) = ZipArchive::new(Cursor::new(nested)) {
                recurse_list(&mut nested_archive, &expr, config, out)?;
            }
        }
    }
    Ok(())
}

pub fn get(expr: &ZipExpr, config: &Config) -> Result<Vec<u8>> {
    let mut bytes = fs::read(&expr.root_archive)?;
    for seg in expr.archive_chain() {
        let inner = read_from_bytes(&bytes, seg)
            .with_context(|| format!("archive segment `{seg}` not found in nested chain"))?;
        if !config.is_archive_name(seg) {
            bail!("segment `{seg}` is not recognized as an archive extension");
        }
        bytes = inner;
    }
    read_from_bytes(&bytes, expr.target()?).with_context(|| "target entry not found".to_string())
}

pub fn delete(expr: &ZipExpr, config: &Config) -> Result<()> {
    let source_bytes = fs::read(&expr.root_archive)?;
    let (next, changed) =
        mutate_archive_bytes(&source_bytes, &expr.segments, Mutation::Delete, config)?;
    if !changed {
        bail!("target entry not found: {}", expr.target()?);
    }
    write_atomically(&expr.root_archive, &next)
}

pub fn replace(expr: &ZipExpr, source: &Path, config: &Config, opts: ReplaceOptions) -> Result<()> {
    let source_bytes = fs::read(source)?;
    let source_meta = fs::metadata(source).ok();
    let source_mtime = source_meta.and_then(|m| m.modified().ok());
    let mutation = Mutation::Replace {
        bytes: source_bytes,
        opts,
        source_mtime,
    };
    let original = fs::read(&expr.root_archive)?;
    let (next, changed) = mutate_archive_bytes(&original, &expr.segments, mutation, config)?;
    if !changed {
        bail!("target entry not found: {}", expr.target()?);
    }
    write_atomically(&expr.root_archive, &next)
}

pub fn patch_draft(
    archive_path: &Path,
    source_dir: &Path,
    output: &Path,
    config: &Config,
) -> Result<DraftSummary> {
    let inputs = collect_inputs(source_dir)?;
    let (spec, summary) = build_patch_spec(archive_path, inputs, config)?;
    spec.write_to_file(output)?;
    Ok(summary)
}

pub fn patch_apply(
    archive_path: &Path,
    spec_path: &Path,
    dry_run: bool,
    config: &Config,
) -> Result<ApplySummary> {
    let spec = PatchSpec::read_from_file(spec_path)?;
    patch_apply_spec(archive_path, &spec, dry_run, config)
}

pub fn patch_apply_spec(
    archive_path: &Path,
    spec: &PatchSpec,
    dry_run: bool,
    config: &Config,
) -> Result<ApplySummary> {
    let mut current = fs::read(archive_path)?;
    let mut summary = ApplySummary {
        replaced: 0,
        deleted: 0,
    };

    for e in &spec.entry {
        let expr = parse_zip_expr(&format!(
            "{}!/{target}",
            normalize_path_for_zip(archive_path),
            target = strip_root_from_target(&e.target)
        ))?;

        match e.action {
            Action::Delete => {
                let (next, changed) =
                    mutate_archive_bytes(&current, &expr.segments, Mutation::Delete, config)?;
                if !changed {
                    bail!("target not found during delete: {}", e.target);
                }
                current = next;
                summary.deleted += 1;
            }
            Action::Replace => {
                let source = e
                    .source
                    .as_ref()
                    .ok_or_else(|| anyhow!("replace entry missing source: {}", e.target))?;
                let bytes = fs::read(source)
                    .with_context(|| format!("failed to read source file `{source}` for patch"))?;
                let source_mtime = fs::metadata(source).ok().and_then(|m| m.modified().ok());
                let opts = ReplaceOptions {
                    method: e.method.clone(),
                    level: e.level.clone(),
                    mtime: e.mtime.clone(),
                };
                let (next, changed) = mutate_archive_bytes(
                    &current,
                    &expr.segments,
                    Mutation::Replace {
                        bytes,
                        opts,
                        source_mtime,
                    },
                    config,
                )?;
                if !changed {
                    bail!("target not found during replace: {}", e.target);
                }
                current = next;
                summary.replaced += 1;
            }
        }
    }

    if !dry_run {
        write_atomically(archive_path, &current)?;
    }
    Ok(summary)
}

fn build_patch_spec(
    archive_path: &Path,
    inputs: Vec<DraftInput>,
    config: &Config,
) -> Result<(PatchSpec, DraftSummary)> {
    let listed = list_recursive(archive_path, config)?;
    let mut by_full = HashMap::<String, String>::new();
    let mut by_name = HashMap::<String, Vec<String>>::new();
    for item in &listed {
        let mut parts = item.expr.split("!/");
        let _root = parts.next();
        let rel = parts.collect::<Vec<_>>().join("!/");
        if rel.is_empty() {
            continue;
        }
        by_full.insert(rel.clone(), item.expr.clone());
        let base = rel.rsplit('/').next().unwrap_or(&rel).to_string();
        by_name.entry(base).or_default().push(item.expr.clone());
    }

    let mut entry = Vec::new();
    let mut unresolved = Vec::new();
    for input in inputs {
        if let Some(target) = by_full.get(&input.rel_norm) {
            entry.push(PatchEntry {
                target: target.clone(),
                source: Some(input.source_norm.clone()),
                action: Action::Replace,
                method: MethodPolicy::Inherit,
                level: LevelPolicy::Name(LevelName::Inherit),
                mtime: MtimePolicy::Source,
                comment: crate::patch_spec::CommentPolicy::Inherit,
            });
            continue;
        }

        if let Some(cands) = by_name.get(&input.base) {
            if cands.len() == 1 {
                entry.push(PatchEntry {
                    target: cands[0].clone(),
                    source: Some(input.source_norm.clone()),
                    action: Action::Replace,
                    method: MethodPolicy::Inherit,
                    level: LevelPolicy::Name(LevelName::Inherit),
                    mtime: MtimePolicy::Source,
                    comment: crate::patch_spec::CommentPolicy::Inherit,
                });
            } else {
                unresolved.push(Unresolved {
                    source: input.source_norm.clone(),
                    reason: "multiple targets".to_string(),
                    candidates: cands.clone(),
                });
            }
            continue;
        }

        unresolved.push(Unresolved {
            source: input.source_norm.clone(),
            reason: "no target matched".to_string(),
            candidates: vec![],
        });
    }

    let spec = PatchSpec {
        version: 1,
        archive: Some(normalize_path_for_zip(archive_path)),
        generated_at: Some(format!("{:?}", SystemTime::now())),
        entry,
        unresolved,
    };
    let summary = DraftSummary {
        matched: spec.entry.len(),
        unresolved: spec.unresolved.len(),
    };
    Ok((spec, summary))
}

pub fn draft_from_path(
    archive_path: &Path,
    source: &Path,
    config: &Config,
) -> Result<(PatchSpec, DraftSummary)> {
    let inputs = collect_inputs(source)?;
    build_patch_spec(archive_path, inputs, config)
}

fn collect_inputs(source: &Path) -> Result<Vec<DraftInput>> {
    if source.is_dir() {
        let mut out = Vec::new();
        for d in WalkDir::new(source)
            .into_iter()
            .filter_map(Result::ok)
            .filter(|e| e.file_type().is_file())
        {
            let src_path = d.path();
            let rel = src_path.strip_prefix(source).unwrap_or(src_path);
            let rel_norm = normalize_path_for_zip(rel);
            let src_norm = normalize_path_for_zip(src_path);
            let base = rel
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default();
            out.push(DraftInput {
                source_norm: src_norm,
                rel_norm,
                base,
            });
        }
        return Ok(out);
    }

    if source.is_file() {
        let src_norm = normalize_path_for_zip(source);
        let base = source
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| src_norm.clone());
        let rel_norm = source
            .file_name()
            .map(|n| normalize_path_for_zip(Path::new(&n)))
            .unwrap_or_else(|| src_norm.clone());
        return Ok(vec![DraftInput {
            source_norm: src_norm,
            rel_norm,
            base,
        }]);
    }

    bail!("source path does not exist: {}", source.display());
}

fn strip_root_from_target(target: &str) -> String {
    if let Some((_, tail)) = target.split_once("!/") {
        tail.to_string()
    } else {
        target.to_string()
    }
}

enum Mutation {
    Delete,
    Replace {
        bytes: Vec<u8>,
        opts: ReplaceOptions,
        source_mtime: Option<SystemTime>,
    },
}

fn mutate_archive_bytes(
    source: &[u8],
    segments: &[String],
    mutation: Mutation,
    config: &Config,
) -> Result<(Vec<u8>, bool)> {
    if segments.is_empty() {
        bail!("empty path segments");
    }

    let mut src_archive = ZipArchive::new(Cursor::new(source))?;
    let out_cursor = Cursor::new(Vec::<u8>::new());
    let mut writer = ZipWriter::new(out_cursor);

    let mut changed = false;
    let target = &segments[0];

    for i in 0..src_archive.len() {
        let mut file = src_archive.by_index(i)?;
        let name = file.name().to_string();
        let file_meta = EntryMeta {
            method: file.compression(),
            mtime: file.last_modified(),
            unix_mode: file.unix_mode(),
        };

        if name != *target {
            writer.raw_copy_file(file)?;
            continue;
        }

        if segments.len() > 1 {
            if !config.is_archive_name(&name) {
                bail!("segment `{name}` is not an archive but nested path continues");
            }
            let mut nested_bytes = Vec::new();
            file.read_to_end(&mut nested_bytes)?;
            let (next_nested, nested_changed) = mutate_archive_bytes(
                &nested_bytes,
                &segments[1..],
                mutation_for_nested(&mutation),
                config,
            )?;
            changed |= nested_changed;
            if nested_changed {
                let nested_meta = file_meta;
                copy_new_entry(
                    &mut writer,
                    &name,
                    &next_nested,
                    nested_meta,
                    &ReplaceOptions::default(),
                    file.last_modified(),
                )?;
            } else {
                writer.raw_copy_file(file)?;
            }
            continue;
        }

        match &mutation {
            Mutation::Delete => {
                changed = true;
            }
            Mutation::Replace {
                bytes,
                opts,
                source_mtime,
            } => {
                changed = true;
                copy_new_entry(
                    &mut writer,
                    &name,
                    bytes,
                    file_meta,
                    opts,
                    source_mtime.and_then(to_zip_time),
                )?;
            }
        }
    }

    let cursor = writer.finish()?;
    Ok((cursor.into_inner(), changed))
}

fn mutation_for_nested(m: &Mutation) -> Mutation {
    match m {
        Mutation::Delete => Mutation::Delete,
        Mutation::Replace {
            bytes,
            opts,
            source_mtime,
        } => Mutation::Replace {
            bytes: bytes.clone(),
            opts: opts.clone(),
            source_mtime: *source_mtime,
        },
    }
}

fn copy_new_entry(
    writer: &mut ZipWriter<Cursor<Vec<u8>>>,
    name: &str,
    data: &[u8],
    original: EntryMeta,
    opts: &ReplaceOptions,
    source_time: Option<DateTime>,
) -> Result<()> {
    let method = resolve_method(&original, opts);
    let level = resolve_level(method, &opts.level, original.method);
    let mut file_opts = SimpleFileOptions::default().compression_method(method);
    if let Some(level) = level {
        file_opts = file_opts.compression_level(Some(i64::from(level)));
    }
    if let Some(mode) = original.unix_mode {
        file_opts = file_opts.unix_permissions(mode);
    }

    let mtime = match opts.mtime {
        MtimePolicy::Keep => original.mtime,
        MtimePolicy::Source => source_time.or(original.mtime),
        MtimePolicy::Now => to_zip_time(SystemTime::now()).or(original.mtime),
    };
    if let Some(t) = mtime {
        file_opts = file_opts.last_modified_time(t);
    }

    writer.start_file(name, file_opts)?;
    if method == CompressionMethod::Stored {
        let _ = crc32fast::hash(data);
    }
    writer.write_all(data)?;
    Ok(())
}

fn resolve_method(original: &EntryMeta, opts: &ReplaceOptions) -> CompressionMethod {
    match opts.method {
        MethodPolicy::Inherit => original.method,
        MethodPolicy::Stored => CompressionMethod::Stored,
        MethodPolicy::Deflated => CompressionMethod::Deflated,
    }
}

fn resolve_level(
    method: CompressionMethod,
    level_policy: &LevelPolicy,
    original: CompressionMethod,
) -> Option<u8> {
    if method == CompressionMethod::Stored {
        return None;
    }
    match level_policy {
        LevelPolicy::Fixed(v) => Some(*v),
        LevelPolicy::Name(LevelName::Default) => None,
        LevelPolicy::Name(LevelName::Inherit) => {
            if original == CompressionMethod::Stored {
                None
            } else {
                None
            }
        }
    }
}

fn read_from_bytes(bytes: &[u8], name: &str) -> Result<Vec<u8>> {
    let mut archive = ZipArchive::new(Cursor::new(bytes))?;
    let mut file = archive
        .by_name(name)
        .with_context(|| format!("entry `{name}` not found"))?;
    let mut out = Vec::new();
    file.read_to_end(&mut out)?;
    Ok(out)
}

fn write_atomically(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let tmp_dir = parent.join(".zipr-tmp");
    fs::create_dir_all(&tmp_dir)?;
    let mut tf = Builder::new()
        .prefix("zipr-")
        .suffix(".tmp")
        .tempfile_in(&tmp_dir)?;
    tf.write_all(bytes)?;
    tf.flush()?;
    let persisted: PathBuf = tf.into_temp_path().keep()?;
    fs::rename(&persisted, path)?;
    Ok(())
}

fn to_zip_time(st: SystemTime) -> Option<DateTime> {
    let odt: OffsetDateTime = st.into();
    DateTime::try_from(odt).ok()
}
