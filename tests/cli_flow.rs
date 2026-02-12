use std::fs;
use std::io::{Cursor, Write};
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use filetime::{FileTime, set_file_mtime};
use std::process::Command;
use tempfile::tempdir;
use zip::CompressionMethod;
use zip::DateTime;
use zip::read::ZipArchive;
use zip::write::SimpleFileOptions;

fn write_zip(path: &Path, entries: Vec<(&str, Vec<u8>, CompressionMethod)>) -> Result<()> {
    let file = fs::File::create(path)?;
    let mut writer = zip::ZipWriter::new(file);
    for (name, data, method) in entries {
        let opts = SimpleFileOptions::default().compression_method(method);
        writer.start_file(name, opts)?;
        writer.write_all(&data)?;
    }
    writer.finish()?;
    Ok(())
}

fn build_nested_archive(path: &Path) -> Result<()> {
    let mut inner_buf = Cursor::new(Vec::<u8>::new());
    {
        let mut inner = zip::ZipWriter::new(&mut inner_buf);
        inner.start_file(
            "a.txt",
            SimpleFileOptions::default().compression_method(CompressionMethod::Deflated),
        )?;
        inner.write_all(b"inner-content")?;
        inner.finish()?;
    }
    let inner_bytes = inner_buf.into_inner();
    write_zip(
        path,
        vec![
            (
                "top.txt",
                b"top-content".to_vec(),
                CompressionMethod::Deflated,
            ),
            ("inner.jar", inner_bytes, CompressionMethod::Stored),
        ],
    )
}

#[test]
fn nested_list_get_replace_delete_flow() -> Result<()> {
    let dir = tempdir()?;
    let archive = dir.path().join("outer.zip");
    build_nested_archive(&archive)?;

    let output = bin_cmd(dir.path()).arg("list").arg(&archive).output()?;
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout)?;
    assert!(stdout.contains("!/inner.jar!/a.txt"));

    let expr = format!("{}!/inner.jar!/a.txt", archive.display());
    let output = bin_cmd(dir.path()).arg("get").arg(&expr).output()?;
    assert!(output.status.success());
    assert_eq!(output.stdout, b"inner-content");

    let replacement = dir.path().join("new-a.txt");
    fs::write(&replacement, b"patched")?;
    let source_mtime = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    set_file_mtime(&replacement, FileTime::from_system_time(source_mtime))?;
    let top_meta_before = top_entry_meta(&archive, "top.txt")?;
    let output = bin_cmd(dir.path())
        .arg("replace")
        .arg(&expr)
        .arg(&replacement)
        .output()?;
    assert!(output.status.success());
    let top_meta_after = top_entry_meta(&archive, "top.txt")?;
    assert_eq!(top_meta_before.0, top_meta_after.0);
    assert_eq!(top_meta_before.1, top_meta_after.1);

    let replaced_dt = nested_entry_time(&archive, "inner.jar", "a.txt")?;
    let expected_dt = to_zip_dt(source_mtime)?;
    assert_eq!(replaced_dt, expected_dt);

    let output = bin_cmd(dir.path()).arg("get").arg(&expr).output()?;
    assert!(output.status.success());
    assert_eq!(output.stdout, b"patched");

    let del_expr = format!("{}!/top.txt", archive.display());
    let output = bin_cmd(dir.path()).arg("delete").arg(&del_expr).output()?;
    assert!(output.status.success());

    let output = bin_cmd(dir.path()).arg("get").arg(&del_expr).output()?;
    assert!(!output.status.success());
    Ok(())
}

#[test]
fn patch_draft_and_apply() -> Result<()> {
    let dir = tempdir()?;
    let archive = dir.path().join("outer.zip");
    build_nested_archive(&archive)?;
    let patches = dir.path().join("patches");
    fs::create_dir_all(&patches)?;
    fs::write(patches.join("a.txt"), b"from-patch")?;

    let draft = dir.path().join("patch.draft.toml");
    let output = bin_cmd(dir.path())
        .arg("patch")
        .arg("draft")
        .arg(&archive)
        .arg("--from-dir")
        .arg(&patches)
        .arg("-o")
        .arg(&draft)
        .output()?;
    assert!(output.status.success());
    assert!(draft.exists());

    let output = bin_cmd(dir.path())
        .arg("patch")
        .arg("apply")
        .arg(&archive)
        .arg("--spec")
        .arg(&draft)
        .output()?;
    assert!(output.status.success());

    let expr = format!("{}!/inner.jar!/a.txt", archive.display());
    let output = bin_cmd(dir.path()).arg("get").arg(&expr).output()?;
    assert!(output.status.success());
    assert_eq!(output.stdout, b"from-patch");
    Ok(())
}

#[test]
fn diff_shows_added_removed_modified_and_metadata() -> Result<()> {
    let dir = tempdir()?;
    let left = dir.path().join("left.zip");
    let right = dir.path().join("right.zip");

    let mut nested_left_buf = Cursor::new(Vec::<u8>::new());
    {
        let mut nested = zip::ZipWriter::new(&mut nested_left_buf);
        nested.start_file(
            "same.txt",
            SimpleFileOptions::default().compression_method(CompressionMethod::Deflated),
        )?;
        nested.write_all(b"old-content")?;
        nested.start_file(
            "old-only.txt",
            SimpleFileOptions::default().compression_method(CompressionMethod::Deflated),
        )?;
        nested.write_all(b"left")?;
        nested.finish()?;
    }

    let mut nested_right_buf = Cursor::new(Vec::<u8>::new());
    {
        let mut nested = zip::ZipWriter::new(&mut nested_right_buf);
        nested.start_file(
            "same.txt",
            SimpleFileOptions::default().compression_method(CompressionMethod::Deflated),
        )?;
        nested.write_all(b"new-content")?;
        nested.start_file(
            "new-only.txt",
            SimpleFileOptions::default().compression_method(CompressionMethod::Deflated),
        )?;
        nested.write_all(b"right")?;
        nested.finish()?;
    }

    write_zip(
        &left,
        vec![
            ("meta.txt", b"same".to_vec(), CompressionMethod::Deflated),
            (
                "nested.jar",
                nested_left_buf.into_inner(),
                CompressionMethod::Stored,
            ),
        ],
    )?;
    write_zip(
        &right,
        vec![
            ("meta.txt", b"same".to_vec(), CompressionMethod::Stored),
            (
                "nested.jar",
                nested_right_buf.into_inner(),
                CompressionMethod::Stored,
            ),
        ],
    )?;

    let output = bin_cmd(dir.path())
        .arg("diff")
        .arg(&left)
        .arg(&right)
        .output()?;
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout)?;
    assert!(stdout.contains("M meta.txt"));
    assert!(stdout.contains("meta: method: Deflated -> Stored"));
    assert!(stdout.contains("M nested.jar!/same.txt"));
    assert!(stdout.contains("A nested.jar!/new-only.txt"));
    assert!(stdout.contains("D nested.jar!/old-only.txt"));
    Ok(())
}

fn bin_cmd(cwd: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_zipr"));
    cmd.current_dir(cwd);
    cmd
}

fn top_entry_meta(path: &Path, entry: &str) -> Result<(CompressionMethod, u64)> {
    let file = fs::File::open(path)?;
    let mut archive = ZipArchive::new(file)?;
    let f = archive.by_name(entry)?;
    Ok((f.compression(), f.compressed_size()))
}

fn nested_entry_time(path: &Path, nested: &str, entry: &str) -> Result<DateTime> {
    let file = fs::File::open(path)?;
    let mut archive = ZipArchive::new(file)?;
    let mut nested_file = archive.by_name(nested)?;
    let mut bytes = Vec::new();
    use std::io::Read as _;
    nested_file.read_to_end(&mut bytes)?;
    let mut nested_archive = ZipArchive::new(Cursor::new(bytes))?;
    let inner = nested_archive.by_name(entry)?;
    inner
        .last_modified()
        .ok_or_else(|| anyhow::anyhow!("missing mtime"))
}

fn to_zip_dt(st: SystemTime) -> Result<DateTime> {
    let odt: time::OffsetDateTime = st.into();
    Ok(DateTime::try_from(odt)?)
}
