use std::fs;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;

use anyhow::Result;
use tempfile::tempdir;
use zip::CompressionMethod;
use zip::write::SimpleFileOptions;

use zipr_lib::archive::{self, patch_draft_extend, restore_backup};
use zipr_lib::config::Config;
use zipr_lib::patch_spec::PatchSpec;

fn write_simple_zip(path: &Path, entries: &[(&str, &[u8])]) -> Result<()> {
    let file = fs::File::create(path)?;
    let mut w = zip::ZipWriter::new(file);
    for (name, data) in entries {
        let opts = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
        w.start_file(*name, opts)?;
        w.write_all(data)?;
    }
    w.finish()?;
    Ok(())
}

#[test]
fn apply_creates_backup_and_restore_round_trips() -> Result<()> {
    let dir = tempdir()?;
    let archive = dir.path().join("app.zip");
    write_simple_zip(&archive, &[("hello.txt", b"original")])?;
    let original_bytes = fs::read(&archive)?;

    let patches = dir.path().join("patches");
    fs::create_dir_all(&patches)?;
    fs::write(patches.join("hello.txt"), b"patched")?;

    let spec_path = dir.path().join("spec.toml");
    archive::patch_draft(&archive, &patches, &spec_path, &Config::default())?;

    let summary = archive::patch_apply(&archive, &spec_path, false, &Config::default())?;
    assert_eq!(summary.replaced, 1);
    let backup_str = summary
        .backup_path
        .as_ref()
        .expect("backup_path should be set on real apply");
    let backup_path = PathBuf::from(backup_str);
    assert!(backup_path.exists(), "backup must exist after apply");

    let after_apply = fs::read(&archive)?;
    assert_ne!(
        after_apply, original_bytes,
        "archive must change after apply"
    );

    restore_backup(&archive, &backup_path)?;
    let after_restore = fs::read(&archive)?;
    assert_eq!(
        after_restore, original_bytes,
        "restored archive should be byte-identical to original"
    );
    assert!(
        !backup_path.exists(),
        "backup should be consumed on restore"
    );
    Ok(())
}

#[test]
fn dry_run_does_not_create_backup() -> Result<()> {
    let dir = tempdir()?;
    let archive = dir.path().join("app.zip");
    write_simple_zip(&archive, &[("hello.txt", b"x")])?;
    let patches = dir.path().join("patches");
    fs::create_dir_all(&patches)?;
    fs::write(patches.join("hello.txt"), b"y")?;
    let spec_path = dir.path().join("spec.toml");
    archive::patch_draft(&archive, &patches, &spec_path, &Config::default())?;

    let summary = archive::patch_apply(&archive, &spec_path, true, &Config::default())?;
    assert!(
        summary.backup_path.is_none(),
        "dry-run must not write backup"
    );
    Ok(())
}

#[test]
fn draft_extend_preserves_existing_entries_and_adds_new_ones() -> Result<()> {
    let dir = tempdir()?;
    let archive = dir.path().join("app.zip");
    write_simple_zip(
        &archive,
        &[("a.txt", b"a-original"), ("b.txt", b"b-original")],
    )?;

    let first_patches = dir.path().join("patches1");
    fs::create_dir_all(&first_patches)?;
    fs::write(first_patches.join("a.txt"), b"a-new")?;
    let spec_path = dir.path().join("spec.toml");
    archive::patch_draft(&archive, &first_patches, &spec_path, &Config::default())?;

    let original_spec = PatchSpec::read_from_file_lenient(&spec_path)?;
    assert_eq!(original_spec.entry.len(), 1);

    let second_patches = dir.path().join("patches2");
    fs::create_dir_all(&second_patches)?;
    fs::write(second_patches.join("b.txt"), b"b-new")?;

    let summary = patch_draft_extend(
        &archive,
        &spec_path,
        std::slice::from_ref(&second_patches),
        &Config::default(),
    )?;
    assert_eq!(summary.matched, 2, "should now have both entries");
    assert_eq!(summary.unresolved, 0);

    let merged = PatchSpec::read_from_file_lenient(&spec_path)?;
    assert_eq!(merged.entry.len(), 2);
    let targets: Vec<&str> = merged.entry.iter().map(|e| e.target.as_str()).collect();
    assert!(targets.iter().any(|t| t.ends_with("!/a.txt")));
    assert!(targets.iter().any(|t| t.ends_with("!/b.txt")));
    Ok(())
}

#[test]
fn draft_extend_skips_duplicate_sources() -> Result<()> {
    let dir = tempdir()?;
    let archive = dir.path().join("app.zip");
    write_simple_zip(&archive, &[("a.txt", b"a")])?;

    let patches = dir.path().join("patches");
    fs::create_dir_all(&patches)?;
    fs::write(patches.join("a.txt"), b"a-new")?;
    let spec_path = dir.path().join("spec.toml");
    archive::patch_draft(&archive, &patches, &spec_path, &Config::default())?;

    let summary = patch_draft_extend(&archive, &spec_path, &[patches], &Config::default())?;
    assert_eq!(
        summary.matched, 1,
        "duplicate source should not double-count"
    );
    let merged = PatchSpec::read_from_file_lenient(&spec_path)?;
    assert_eq!(merged.entry.len(), 1);
    Ok(())
}

#[test]
fn draft_extend_skips_duplicate_targets_from_new_sources() -> Result<()> {
    let dir = tempdir()?;
    let archive = dir.path().join("app.zip");
    write_simple_zip(&archive, &[("a.txt", b"a")])?;

    let spec_path = dir.path().join("spec.toml");
    let empty = dir.path().join("empty");
    fs::create_dir_all(&empty)?;
    archive::patch_draft(&archive, &empty, &spec_path, &Config::default())?;

    let patches1 = dir.path().join("patches1");
    let patches2 = dir.path().join("patches2");
    fs::create_dir_all(&patches1)?;
    fs::create_dir_all(&patches2)?;
    fs::write(patches1.join("a.txt"), b"a-new-1")?;
    fs::write(patches2.join("a.txt"), b"a-new-2")?;

    let summary = patch_draft_extend(
        &archive,
        &spec_path,
        &[patches1, patches2],
        &Config::default(),
    )?;
    assert_eq!(
        summary.matched, 1,
        "duplicate target should be de-duplicated"
    );
    assert_eq!(summary.unresolved, 0);

    let merged = PatchSpec::read_from_file(&spec_path)?;
    assert_eq!(merged.entry.len(), 1);
    Ok(())
}
