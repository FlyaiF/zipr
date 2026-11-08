use std::{
    fs,
    io::{Cursor, Write},
};
use zip::{CompressionMethod, ZipWriter, write::SimpleFileOptions};
use zipr_lib::{ApplyOptions, Cache, CancelToken, Chain, EngineError, Op, Plan, Scope, apply};
fn archive(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut w = ZipWriter::new(Cursor::new(Vec::new()));
    for (name, bytes) in entries {
        w.start_file(
            *name,
            SimpleFileOptions::default().compression_method(CompressionMethod::Stored),
        )
        .unwrap();
        w.write_all(bytes).unwrap();
    }
    w.finish().unwrap().into_inner()
}
#[test]
fn damaged_component_is_skipped_but_healthy_writes_succeed() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("release.zip");
    let component = archive(&[("shared.txt", b"unique payload")]);
    let mut bytes = archive(&[("damaged.zip", &component), ("healthy.zip", &component)]);
    let offset = bytes
        .windows(14)
        .position(|b| b == b"unique payload")
        .unwrap();
    bytes[offset] ^= 1;
    fs::write(&root, bytes).unwrap();
    let cache = Cache::new().unwrap();
    assert_eq!(
        cache.list_children(&root, &Chain::default()).unwrap().len(),
        2
    );
    assert!(cache.problems().is_empty()); // Opening the root never streams its children.
    let mut first_cost = None;
    for _ in 0..2 {
        assert!(matches!(
            cache.list_children(&root, &Chain::from("damaged.zip")),
            Err(EngineError::Corrupt { .. })
        ));
        let cost = cache.crc_metrics();
        assert!(cost.0 > 0);
        if let Some(first) = first_cost {
            assert_eq!(cost, first);
        }
        first_cost = Some(cost);
    }
    let index = cache
        .index_names(
            &root,
            &Chain::default(),
            &Scope::default(),
            &|_| {},
            &CancelToken::default(),
        )
        .unwrap();
    assert_eq!(
        index["shared.txt"],
        vec![Chain::from("healthy.zip!/shared.txt")]
    );
    assert_eq!(cache.problems().len(), 1);
    assert_eq!(cache.problems()[0].chain, Chain::from("damaged.zip"));
    let entries = cache.list_children(&root, &Chain::default()).unwrap();
    assert!(
        entries
            .iter()
            .find(|e| e.name == "damaged.zip")
            .unwrap()
            .corrupt
            .is_some()
    );
    assert!(
        entries
            .iter()
            .find(|e| e.name == "healthy.zip")
            .unwrap()
            .corrupt
            .is_none()
    );
    let source = temp.path().join("patch.txt");
    fs::write(&source, b"changed").unwrap();
    let plan = |target| Plan {
        root: root.clone(),
        ops: vec![Op::Replace {
            target: Chain::from(target),
            source: source.clone(),
        }],
    };
    let opts = ApplyOptions {
        backup: false,
        write_log: false,
    };
    assert!(matches!(
        apply(
            &plan("damaged.zip!/shared.txt"),
            &opts,
            &cache,
            &|_| {},
            &CancelToken::default()
        ),
        Err(EngineError::Corrupt { .. })
    ));
    apply(
        &plan("healthy.zip!/shared.txt"),
        &opts,
        &cache,
        &|_| {},
        &CancelToken::default(),
    )
    .unwrap();
    assert!(
        cache
            .list_children(&root, &Chain::from("healthy.zip"))
            .is_ok()
    );
}
#[test]
fn unparseable_nested_archive_is_reported_and_serialized() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("release.zip");
    fs::write(
        &root,
        archive(&[("invalid.zip", b"not an archive"), ("ok.txt", b"ok")]),
    )
    .unwrap();
    let cache = Cache::new().unwrap();
    let err = cache
        .list_children(&root, &Chain::from("invalid.zip"))
        .unwrap_err();
    let value = serde_json::to_value(err).unwrap();
    assert_eq!(value["kind"], "corrupt");
    assert_eq!(value["chain"][0], "invalid.zip");
    assert!(value["detail"].as_str().unwrap().contains("EOCD"));
    let index = cache
        .index_names(
            &root,
            &Chain::default(),
            &Scope::default(),
            &|_| {},
            &CancelToken::default(),
        )
        .unwrap();
    assert!(index.contains_key("ok.txt"));
}

#[test]
fn invalid_deflate_stream_is_corrupt_and_does_not_abort_indexing() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("release.zip");
    let mut w = ZipWriter::new(Cursor::new(Vec::new()));
    w.start_file(
        "broken.zip",
        SimpleFileOptions::default().compression_method(CompressionMethod::Deflated),
    )
    .unwrap();
    w.write_all(&archive(&[("settings.txt", b"payload")]))
        .unwrap();
    w.start_file(
        "healthy.txt",
        SimpleFileOptions::default().compression_method(CompressionMethod::Stored),
    )
    .unwrap();
    w.write_all(b"healthy").unwrap();
    let mut bytes = w.finish().unwrap().into_inner();
    let mut zip = zip::ZipArchive::new(Cursor::new(&bytes)).unwrap();
    let entry = zip.by_name("broken.zip").unwrap();
    let start = entry.data_start().unwrap() as usize;
    let len = entry.compressed_size() as usize;
    drop(entry);
    drop(zip);
    bytes[start..start + len].fill(0xff);
    fs::write(&root, bytes).unwrap();
    let cache = Cache::new().unwrap();
    let index = cache
        .index_names(
            &root,
            &Chain::default(),
            &Scope::default(),
            &|_| {},
            &CancelToken::default(),
        )
        .unwrap();
    assert!(index.contains_key("healthy.txt"));
    assert_eq!(cache.problems().len(), 1);
    assert!(matches!(
        cache.list_children(&root, &Chain::from("broken.zip")),
        Err(EngineError::Corrupt { .. })
    ));
}
