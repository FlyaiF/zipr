use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
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
    /// True if this entry is itself an archive (zip/jar/war/ear). Whether the
    /// nested archive's children are present in the same list depends on
    /// which list function was called: `list_recursive` includes them,
    /// `list_top_level` does not.
    pub is_archive: bool,
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
    /// Path to the pre-apply backup of the archive. May be relative if the
    /// input archive path was relative. `None` for dry-run.
    pub backup_path: Option<String>,
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

/// List only the top-level entries of an archive (no nested-archive expansion).
/// Callers that need a nested archive's children must request them separately
/// via `list_segment` using the entry's `expr`.
pub fn list_top_level(path: &Path, config: &Config) -> Result<Vec<ListedEntry>> {
    let file = File::open(path)?;
    let mut archive = ZipArchive::new(file)?;
    let mut out = Vec::new();
    let root = path.to_string_lossy().to_string();
    list_one_level(&mut archive, &root, config, &mut out)?;
    Ok(out)
}

/// List the top-level entries of the archive denoted by `zip_expr`. This may
/// be the root archive (when there are no segments) or any nested archive
/// addressed by the full segment chain (e.g. `outer.war!/inner.jar` returns
/// inner.jar's children).
pub fn list_segment(zip_expr: &ZipExpr, config: &Config) -> Result<Vec<ListedEntry>> {
    if zip_expr.segments.is_empty() {
        return list_top_level(&zip_expr.root_archive, config);
    }

    let mut bytes = fs::read(&zip_expr.root_archive)?;
    let mut prefix = normalize_path_for_zip(&zip_expr.root_archive);
    for seg in &zip_expr.segments {
        if !config.is_archive_name(seg) {
            bail!("segment `{seg}` is not recognized as an archive extension");
        }
        bytes = read_from_bytes(&bytes, seg)
            .with_context(|| format!("archive segment `{seg}` not found in nested chain"))?;
        prefix = make_expr(&prefix, seg);
    }

    let mut archive = ZipArchive::new(Cursor::new(bytes))?;
    let mut out = Vec::new();
    list_one_level(&mut archive, &prefix, config, &mut out)?;
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
        let is_archive = config.is_archive_name(&name);
        out.push(ListedEntry {
            expr: expr.clone(),
            size: file.size(),
            compressed_size: file.compressed_size(),
            is_archive,
        });

        if is_archive {
            let mut nested = Vec::new();
            file.read_to_end(&mut nested)?;
            if let Ok(mut nested_archive) = ZipArchive::new(Cursor::new(nested)) {
                recurse_list(&mut nested_archive, &expr, config, out)?;
            }
        }
    }
    Ok(())
}

fn list_one_level<R: Read + Seek>(
    archive: &mut ZipArchive<R>,
    prefix: &str,
    config: &Config,
    out: &mut Vec<ListedEntry>,
) -> Result<()> {
    for i in 0..archive.len() {
        let file = archive.by_index(i)?;
        if file.is_dir() {
            continue;
        }
        let name = file.name().to_string();
        let expr = make_expr(prefix, &name);
        let is_archive = config.is_archive_name(&name);
        out.push(ListedEntry {
            expr,
            size: file.size(),
            compressed_size: file.compressed_size(),
            is_archive,
        });
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

/// Extend an existing patch spec with additional source files/directories.
/// Existing entries and unresolved items are preserved verbatim; new inputs whose
/// `source` already appears in the spec are skipped.
pub fn patch_draft_extend(
    archive_path: &Path,
    spec_path: &Path,
    additional_sources: &[PathBuf],
    config: &Config,
) -> Result<DraftSummary> {
    let mut existing = PatchSpec::read_from_file_lenient(spec_path)?;

    let mut seen_sources: HashSet<String> = HashSet::new();
    for e in &existing.entry {
        if let Some(s) = &e.source {
            seen_sources.insert(s.clone());
        }
    }
    for u in &existing.unresolved {
        seen_sources.insert(u.source.clone());
    }
    let existing_targets: HashSet<String> =
        existing.entry.iter().map(|e| e.target.clone()).collect();

    let mut new_inputs: Vec<DraftInput> = Vec::new();
    for src in additional_sources {
        let inputs = collect_inputs(src)?;
        for input in inputs {
            if seen_sources.contains(&input.source_norm) {
                continue;
            }
            seen_sources.insert(input.source_norm.clone());
            new_inputs.push(input);
        }
    }

    let (new_spec, _) = build_patch_spec(archive_path, new_inputs, config)?;
    for entry in new_spec.entry {
        if !existing_targets.contains(&entry.target) {
            existing.entry.push(entry);
        }
    }
    for u in new_spec.unresolved {
        existing.unresolved.push(u);
    }

    existing.write_to_file(spec_path)?;
    Ok(DraftSummary {
        matched: existing.entry.len(),
        unresolved: existing.unresolved.len(),
    })
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
        backup_path: None,
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
        let backup = write_backup(archive_path)?;
        summary.backup_path = Some(normalize_path_for_zip(&backup));
        write_atomically(archive_path, &current)?;
    }
    Ok(summary)
}

fn write_backup(archive_path: &Path) -> Result<PathBuf> {
    let mut source = File::open(archive_path).with_context(|| {
        format!(
            "failed to open archive `{}` for backup",
            archive_path.display()
        )
    })?;
    for attempt in 0..1000 {
        let backup = backup_path_for(archive_path, attempt);
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&backup)
        {
            Ok(mut dest) => {
                std::io::copy(&mut source, &mut dest).with_context(|| {
                    format!("failed to write backup `{}` before apply", backup.display())
                })?;
                return Ok(backup);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => {
                return Err(e).with_context(|| {
                    format!(
                        "failed to create backup `{}` before apply",
                        backup.display()
                    )
                });
            }
        }
    }
    bail!(
        "failed to choose an unused backup path for `{}`",
        archive_path.display()
    );
}

fn backup_path_for(archive_path: &Path, attempt: u16) -> PathBuf {
    let now = OffsetDateTime::now_utc();
    let stamp = format!(
        "{:04}{:02}{:02}T{:02}{:02}{:02}.{:09}Z",
        now.year(),
        u8::from(now.month()),
        now.day(),
        now.hour(),
        now.minute(),
        now.second(),
        now.nanosecond()
    );
    let mut path = archive_path.as_os_str().to_os_string();
    if attempt == 0 {
        path.push(format!(".bak-{stamp}"));
    } else {
        path.push(format!(".bak-{stamp}-{attempt}"));
    }
    PathBuf::from(path)
}

/// Restore an archive from a previously-created backup file.
/// The backup is read and the archive is rewritten atomically from those bytes.
/// If the restore succeeds, the backup file is then removed.
/// The backup path must be a sibling of the archive (same parent directory).
pub fn restore_backup(archive_path: &Path, backup_path: &Path) -> Result<()> {
    if !backup_path.exists() {
        bail!("backup file not found: {}", backup_path.display());
    }
    let archive_parent = archive_path.parent().unwrap_or_else(|| Path::new("."));
    let backup_parent = backup_path.parent().unwrap_or_else(|| Path::new("."));
    if archive_parent != backup_parent {
        bail!(
            "backup `{}` must live next to archive `{}`",
            backup_path.display(),
            archive_path.display()
        );
    }
    let bytes = fs::read(backup_path)
        .with_context(|| format!("failed to read backup `{}`", backup_path.display()))?;
    write_atomically(archive_path, &bytes)?;
    fs::remove_file(backup_path).with_context(|| {
        format!(
            "restored archive but failed to remove backup `{}`",
            backup_path.display()
        )
    })?;
    Ok(())
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
            if is_generated_patch_file(rel) {
                continue;
            }
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

fn is_generated_patch_file(path: &Path) -> bool {
    matches!(
        path.file_name().and_then(|name| name.to_str()),
        Some("patch.draft.toml" | "patch.draft.orig.toml")
    )
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
    _original: CompressionMethod,
) -> Option<u8> {
    if method == CompressionMethod::Stored {
        return None;
    }
    match level_policy {
        LevelPolicy::Fixed(v) => Some(*v),
        LevelPolicy::Name(LevelName::Default) => None,
        LevelPolicy::Name(LevelName::Inherit) => None,
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
    replace_file(&persisted, path)?;
    Ok(())
}

#[cfg(not(windows))]
fn replace_file(source: &Path, destination: &Path) -> Result<()> {
    fs::rename(source, destination)
        .with_context(|| format!("failed to replace `{}`", destination.display()))
}

#[cfg(windows)]
fn replace_file(source: &Path, destination: &Path) -> Result<()> {
    if !destination.exists() {
        return fs::rename(source, destination)
            .with_context(|| format!("failed to move `{}` into place", source.display()));
    }

    let parent = destination.parent().unwrap_or_else(|| Path::new("."));
    let tmp_dir = parent.join(".zipr-tmp");
    fs::create_dir_all(&tmp_dir)?;
    let backup = Builder::new()
        .prefix("zipr-backup-")
        .suffix(".bak")
        .tempfile_in(&tmp_dir)?;
    let backup_path = backup.path().to_path_buf();
    drop(backup);
    if backup_path.exists() {
        fs::remove_file(&backup_path)?;
    }

    fs::rename(destination, &backup_path).with_context(|| {
        format!(
            "failed to move existing archive `{}` to backup",
            destination.display()
        )
    })?;

    match fs::rename(source, destination) {
        Ok(()) => {
            let _ = fs::remove_file(&backup_path);
            Ok(())
        }
        Err(replace_err) => {
            if let Err(restore_err) = fs::rename(&backup_path, destination) {
                return Err(anyhow!(
                    "failed to replace `{}`: {}; restore from `{}` also failed: {}",
                    destination.display(),
                    replace_err,
                    backup_path.display(),
                    restore_err
                ));
            }
            Err(replace_err)
                .with_context(|| format!("failed to replace `{}`", destination.display()))
        }
    }
}

fn to_zip_time(st: SystemTime) -> Option<DateTime> {
    let odt: OffsetDateTime = st.into();
    DateTime::try_from(odt).ok()
}
