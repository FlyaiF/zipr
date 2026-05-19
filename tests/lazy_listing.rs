use std::fs;
use std::io::{Cursor, Write};
use std::path::Path;

use anyhow::Result;
use tempfile::tempdir;
use zip::CompressionMethod;
use zip::write::SimpleFileOptions;

use zipr_lib::archive::{list_segment, list_top_level};
use zipr_lib::config::Config;
use zipr_lib::path_expr::parse_zip_expr;

fn write_archive(path: &Path, entries: &[(&str, &[u8])]) -> Result<()> {
    let file = fs::File::create(path)?;
    let mut w = zip::ZipWriter::new(file);
    for (name, data) in entries {
        let opts = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
        w.start_file(*name, opts)?;
        w.write_all(data)?;
    }
    w.finish()?;
    Ok(())
}

fn make_nested_archive_bytes(entries: &[(&str, &[u8])]) -> Result<Vec<u8>> {
    let mut buf = Cursor::new(Vec::<u8>::new());
    {
        let mut w = zip::ZipWriter::new(&mut buf);
        for (name, data) in entries {
            let opts = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
            w.start_file(*name, opts)?;
            w.write_all(data)?;
        }
        w.finish()?;
    }
    Ok(buf.into_inner())
}

#[test]
fn list_top_level_does_not_recurse_into_nested_archives() -> Result<()> {
    let dir = tempdir()?;
    let archive = dir.path().join("outer.war");
    let nested =
        make_nested_archive_bytes(&[("com/Foo.class", b"foo"), ("com/Bar.class", b"bar")])?;
    write_archive(
        &archive,
        &[("top.txt", b"hello"), ("BOOT-INF/lib/inner.jar", &nested)],
    )?;

    let items = list_top_level(&archive, &Config::default())?;

    let exprs: Vec<&str> = items.iter().map(|e| e.expr.as_str()).collect();
    // Should see top-level entries only, NOT inner.jar's children.
    assert!(exprs.iter().any(|e| e.ends_with("!/top.txt")));
    assert!(
        exprs
            .iter()
            .any(|e| e.ends_with("!/BOOT-INF/lib/inner.jar"))
    );
    assert!(
        !exprs
            .iter()
            .any(|e| e.contains("Foo.class") || e.contains("Bar.class")),
        "nested entries must not be returned by list_top_level: {exprs:?}"
    );

    // is_archive flag should be set on the nested archive entry.
    let inner = items
        .iter()
        .find(|e| e.expr.ends_with("/inner.jar"))
        .expect("inner.jar entry present");
    assert!(inner.is_archive, "inner.jar should be flagged is_archive");

    let top = items
        .iter()
        .find(|e| e.expr.ends_with("/top.txt"))
        .expect("top.txt present");
    assert!(!top.is_archive);
    Ok(())
}

#[test]
fn list_segment_returns_children_of_a_nested_archive() -> Result<()> {
    let dir = tempdir()?;
    let archive = dir.path().join("outer.war");
    let nested =
        make_nested_archive_bytes(&[("com/Foo.class", b"foo"), ("com/Bar.class", b"bar")])?;
    write_archive(&archive, &[("BOOT-INF/lib/inner.jar", &nested)])?;

    let segment_expr = format!("{}!/BOOT-INF/lib/inner.jar", archive.display());
    let parsed = parse_zip_expr(&segment_expr)?;
    let items = list_segment(&parsed, &Config::default())?;

    let names: Vec<&str> = items.iter().map(|e| e.expr.as_str()).collect();
    assert!(
        names.iter().any(|e| e.contains("Foo.class")),
        "expected Foo.class in segment listing: {names:?}"
    );
    assert!(names.iter().any(|e| e.contains("Bar.class")));
    Ok(())
}
