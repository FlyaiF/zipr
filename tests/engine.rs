use std::{
    collections::BTreeMap,
    fs,
    io::{Cursor, Read, Write},
};
use zipr_lib::{
    engine::*,
    splice::{self, Change, Directory},
};
fn archive(entries: &[(&str, &[u8], zip::CompressionMethod)]) -> Vec<u8> {
    let mut z = zip::ZipWriter::new(Cursor::new(Vec::new()));
    z.set_comment("archive comment").unwrap();
    for (n, b, m) in entries {
        z.start_file(
            *n,
            zip::write::SimpleFileOptions::default().compression_method(*m),
        )
        .unwrap();
        z.write_all(b).unwrap();
    }
    z.finish().unwrap().into_inner()
}
fn get(b: &[u8], name: &str) -> Vec<u8> {
    let mut z = zip::ZipArchive::new(Cursor::new(b)).unwrap();
    let mut v = vec![];
    z.by_name(name).unwrap().read_to_end(&mut v).unwrap();
    v
}
fn untouched(before: &[u8], after: &[u8], names: &[&str]) {
    let a = Directory::read(&mut Cursor::new(before)).unwrap();
    let b = Directory::read(&mut Cursor::new(after)).unwrap();
    for name in names {
        let x = a.records.iter().find(|r| r.name == *name).unwrap();
        let y = b.records.iter().find(|r| r.name == *name).unwrap();
        let mut xr = x.raw.clone();
        let mut yr = y.raw.clone();
        xr[42..46].fill(0);
        yr[42..46].fill(0);
        assert_eq!(xr, yr);
        let end = a
            .records
            .iter()
            .filter(|r| r.offset > x.offset)
            .map(|r| r.offset)
            .min()
            .unwrap_or(a.start);
        let len = (end - x.offset) as usize;
        assert_eq!(
            &before[x.offset as usize..x.offset as usize + len],
            &after[y.offset as usize..y.offset as usize + len]
        );
    }
}
#[test]
fn nested_replace_preserves_untouched_and_stored() {
    use zip::CompressionMethod::{Deflated, Stored};
    let lib = archive(&[
        ("Foo.class", b"old", Deflated),
        ("untouched", b"lib bytes", Stored),
    ]);
    let boot = archive(&[
        ("BOOT-INF/lib/owned-a.jar", &lib, Stored),
        ("untouched", b"boot bytes", Deflated),
    ]);
    let root = archive(&[
        ("boot.jar", &boot, Deflated),
        ("untouched", b"root bytes", Stored),
    ]);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("root.zip");
    fs::write(&path, &root).unwrap();
    let source = dir.path().join("Foo.class");
    fs::write(&source, b"new class contents").unwrap();
    let report = apply(
        &Plan {
            root: path.clone(),
            ops: vec![Op::Replace {
                target: Chain::from("boot.jar!/BOOT-INF/lib/owned-a.jar!/Foo.class"),
                source,
            }],
        },
        &ApplyOptions::default(),
        &Cache::new().unwrap(),
        &|_| {},
        &CancelToken::default(),
    )
    .unwrap();
    assert_eq!(report.rewritten_chains.len(), 3);
    let next = fs::read(&path).unwrap();
    let boot_next = get(&next, "boot.jar");
    let lib_next = get(&boot_next, "BOOT-INF/lib/owned-a.jar");
    assert_eq!(get(&lib_next, "Foo.class"), b"new class contents");
    untouched(&root, &next, &["untouched"]);
    untouched(&boot, &boot_next, &["untouched"]);
    untouched(&lib, &lib_next, &["untouched"]);
    let d = Directory::read(&mut Cursor::new(&boot_next)).unwrap();
    assert_eq!(d.records[0].method(), 0);
    assert_eq!(fs::read(report.backup_path.unwrap()).unwrap(), root);
    for (i, b) in [&next, &boot_next, &lib_next].iter().enumerate() {
        let p = dir.path().join(format!("check{i}.zip"));
        fs::write(&p, b).unwrap();
        if let Ok(out) = std::process::Command::new("unzip")
            .args(["-t"])
            .arg(p)
            .output()
        {
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stdout)
            );
        }
    }
}
#[test]
fn cancel_and_signed_refusal_leave_original() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("root.jar");
    let bytes = archive(&[
        (
            "META-INF/X.SF",
            b"signature",
            zip::CompressionMethod::Stored,
        ),
        ("META-INF/X.RSA", b"block", zip::CompressionMethod::Stored),
        ("x", b"old", zip::CompressionMethod::Stored),
    ]);
    fs::write(&path, &bytes).unwrap();
    let source = dir.path().join("x");
    fs::write(&source, b"new").unwrap();
    let plan = Plan {
        root: path.clone(),
        ops: vec![Op::Replace {
            target: Chain::from("x"),
            source,
        }],
    };
    assert!(matches!(
        apply(
            &plan,
            &ApplyOptions::default(),
            &Cache::new().unwrap(),
            &|_| {},
            &CancelToken::default()
        ),
        Err(EngineError::SignedJar { .. })
    ));
    let c = CancelToken::default();
    c.cancel();
    assert!(matches!(
        apply(
            &plan,
            &ApplyOptions::default(),
            &Cache::new().unwrap(),
            &|_| {},
            &c
        ),
        Err(EngineError::Cancelled)
    ));
    assert_eq!(fs::read(path).unwrap(), bytes);
}
#[test]
fn parallel_deflate_and_zip64_count() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("random");
    let mut state = 1234567u32;
    let data: Vec<u8> = (0..9 * 1024 * 1024)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            state as u8
        })
        .collect();
    fs::write(&source, &data).unwrap();
    let src = archive(&[("random", b"old", zip::CompressionMethod::Deflated)]);
    let changes = BTreeMap::from([("random".into(), Change { source })]);
    let mut out = vec![];
    splice::rewrite(
        &mut Cursor::new(src),
        &mut out,
        &changes,
        &CancelToken::default(),
    )
    .unwrap();
    assert_eq!(get(&out, "random"), data);
    let p = dir.path().join("parallel.zip");
    fs::write(&p, &out).unwrap();
    if let Ok(status) = std::process::Command::new("unzip")
        .arg("-tqq")
        .arg(p)
        .status()
    {
        assert!(status.success());
    }
    let mut z = zip::ZipWriter::new(Cursor::new(Vec::new()));
    for i in 0..65536 {
        z.start_file(
            format!("{i}"),
            zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored),
        )
        .unwrap();
    }
    let src = z.finish().unwrap().into_inner();
    let mut out = vec![];
    splice::rewrite(
        &mut Cursor::new(&src),
        &mut out,
        &BTreeMap::new(),
        &CancelToken::default(),
    )
    .unwrap();
    assert!(
        src == out,
        "ZIP64 bytes differ at {:?}",
        src.iter().zip(&out).position(|(a, b)| a != b)
    );
    let mut prefixed = b"#!/bin/sh\n".to_vec();
    prefixed.extend_from_slice(&src);
    out.clear();
    splice::rewrite(
        &mut Cursor::new(&prefixed),
        &mut out,
        &BTreeMap::new(),
        &CancelToken::default(),
    )
    .unwrap();
    assert!(out.starts_with(b"#!/bin/sh\n"));
    assert_eq!(
        zip::ZipArchive::new(Cursor::new(&out)).unwrap().len(),
        65536
    );
}

#[test]
fn additions_update_boot_indexes_and_create_parent_dirs() {
    use zip::CompressionMethod::Stored;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("boot.jar");
    fs::write(&root,archive(&[("config/",b"",Stored),("BOOT-INF/classpath.idx",b"- \"BOOT-INF/lib/old.jar\"\n",Stored),("BOOT-INF/layers.idx",b"- \"dependencies\":\n  - \"BOOT-INF/lib/old.jar\"\n- \"application\":\n  - \"BOOT-INF/classes/\"\n",Stored)])).unwrap();
    let xml = dir.path().join("x.xml");
    fs::write(&xml, b"<config/>").unwrap();
    let jar = dir.path().join("new.jar");
    fs::write(&jar, archive(&[("x", b"x", Stored)])).unwrap();
    apply(
        &Plan {
            root: root.clone(),
            ops: vec![
                Op::Add {
                    parent: Chain::default(),
                    name: "config/new/x.xml".into(),
                    source: xml,
                },
                Op::Add {
                    parent: Chain::default(),
                    name: "BOOT-INF/lib/new.jar".into(),
                    source: jar,
                },
            ],
        },
        &ApplyOptions::default(),
        &Cache::new().unwrap(),
        &|_| {},
        &CancelToken::default(),
    )
    .unwrap();
    let bytes = fs::read(root).unwrap();
    assert_eq!(get(&bytes, "config/new/x.xml"), b"<config/>");
    assert!(
        String::from_utf8(get(&bytes, "BOOT-INF/classpath.idx"))
            .unwrap()
            .contains("- \"BOOT-INF/lib/new.jar\"")
    );
    assert!(
        String::from_utf8(get(&bytes, "BOOT-INF/layers.idx"))
            .unwrap()
            .contains("  - \"BOOT-INF/lib/new.jar\"")
    );
    let d = Directory::read(&mut Cursor::new(bytes)).unwrap();
    assert!(d.records.iter().any(|r| r.name == "config/new/"));
    assert_eq!(
        d.records
            .iter()
            .find(|r| r.name == "BOOT-INF/lib/new.jar")
            .unwrap()
            .method(),
        0
    );
}
#[test]
fn cancellation_during_rewrite_cleans_temps_and_keeps_mode() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root.zip");
    let bytes = archive(&[("x", b"old", zip::CompressionMethod::Stored)]);
    fs::write(&root, &bytes).unwrap();
    let source = dir.path().join("source");
    fs::write(&source, b"new").unwrap();
    let plan = Plan {
        root: root.clone(),
        ops: vec![Op::Replace {
            target: Chain::from("x"),
            source,
        }],
    };
    let c = CancelToken::default();
    let p = |e: ProgressEvent| {
        if matches!(e.phase, Phase::Rewrite) && e.done > 0 {
            c.cancel();
        }
    };
    assert!(matches!(
        apply(
            &plan,
            &ApplyOptions::default(),
            &Cache::new().unwrap(),
            &p,
            &c
        ),
        Err(EngineError::Cancelled)
    ));
    assert_eq!(fs::read(&root).unwrap(), bytes);
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 2);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).unwrap();
        apply(
            &plan,
            &ApplyOptions {
                backup: false,
                write_log: false,
            },
            &Cache::new().unwrap(),
            &|_| {},
            &CancelToken::default(),
        )
        .unwrap();
        assert_eq!(
            fs::metadata(root).unwrap().permissions().mode() & 0o777,
            0o755
        );
    }
}

// Hand-built records cover metadata that ZipWriter normalizes or cannot emit.
fn metadata_fixture(changed_name: &[u8], changed: &[u8]) -> Vec<u8> {
    fn p16(v: &mut Vec<u8>, n: u16) {
        v.extend_from_slice(&n.to_le_bytes());
    }
    fn p32(v: &mut Vec<u8>, n: u32) {
        v.extend_from_slice(&n.to_le_bytes());
    }
    let mut out = b"#!/bin/sh\nexec java -jar \"$0\" \"$@\"\n".to_vec();
    let mut cd = vec![];
    let gbk = encoding_rs::GBK.encode("中文.txt").0.into_owned();
    for (name, data, descriptor) in [
        (changed_name, changed, false),
        (gbk.as_slice(), b"unchanged".as_slice(), true),
    ] {
        let offset = out.len() as u32;
        let crc = crc32fast::hash(data);
        let extra = [
            0x55, 0x54, 5, 0, 1, 0, 0, 0, 0, 0x75, 0x78, 5, 0, 1, 1, 42, 1, 43,
        ];
        p32(&mut out, 0x04034b50);
        p16(&mut out, 20);
        p16(&mut out, if descriptor { 8 } else { 0 });
        p16(&mut out, 0);
        p16(&mut out, 0);
        p16(&mut out, 0x5821);
        for n in [crc, data.len() as u32, data.len() as u32] {
            p32(&mut out, if descriptor { 0 } else { n });
        }
        p16(&mut out, name.len() as u16);
        p16(&mut out, extra.len() as u16);
        out.extend_from_slice(name);
        out.extend_from_slice(&extra);
        out.extend_from_slice(data);
        if descriptor {
            p32(&mut out, 0x08074b50);
            p32(&mut out, crc);
            p32(&mut out, data.len() as u32);
            p32(&mut out, data.len() as u32);
        }
        p32(&mut cd, 0x02014b50);
        p16(&mut cd, 0x0314);
        p16(&mut cd, 20);
        p16(&mut cd, if descriptor { 8 } else { 0 });
        p16(&mut cd, 0);
        p16(&mut cd, 0);
        p16(&mut cd, 0x5821);
        p32(&mut cd, crc);
        p32(&mut cd, data.len() as u32);
        p32(&mut cd, data.len() as u32);
        p16(&mut cd, name.len() as u16);
        p16(&mut cd, extra.len() as u16);
        p16(&mut cd, 7);
        p16(&mut cd, 0);
        p16(&mut cd, 1);
        p32(&mut cd, 0o100755 << 16 | 0x20);
        p32(&mut cd, offset);
        cd.extend_from_slice(name);
        cd.extend_from_slice(&extra);
        cd.extend_from_slice(b"comment");
    }
    let offset = out.len() as u32;
    out.extend_from_slice(&cd);
    p32(&mut out, 0x06054b50);
    p16(&mut out, 0);
    p16(&mut out, 0);
    p16(&mut out, 2);
    p16(&mut out, 2);
    p32(&mut out, cd.len() as u32);
    p32(&mut out, offset);
    p16(&mut out, 7);
    out.extend_from_slice(b"comment");
    out
}
#[test]
fn metadata_byte_identity_at_every_level() {
    let lib = metadata_fixture(b"Foo.class", b"old");
    let boot = metadata_fixture(b"BOOT-INF/lib/a.jar", &lib);
    let root_bytes = metadata_fixture(b"boot.jar", &boot);
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root.zip");
    fs::write(&root, &root_bytes).unwrap();
    let source = dir.path().join("patch");
    fs::write(&source, b"replacement class").unwrap();
    apply(
        &Plan {
            root: root.clone(),
            ops: vec![Op::Replace {
                target: Chain::from("boot.jar!/BOOT-INF/lib/a.jar!/Foo.class"),
                source,
            }],
        },
        &ApplyOptions::default(),
        &Cache::new().unwrap(),
        &|_| {},
        &CancelToken::default(),
    )
    .unwrap();
    let next = fs::read(root).unwrap();
    let boot_next = get(&next, "boot.jar");
    let lib_next = get(&boot_next, "BOOT-INF/lib/a.jar");
    for (a, b) in [(&root_bytes, &next), (&boot, &boot_next), (&lib, &lib_next)] {
        untouched(a, b, &["中文.txt"]);
        assert!(b.starts_with(b"#!/bin/sh"));
        assert!(b.ends_with(b"comment"));
    }
    assert_eq!(get(&lib_next, "Foo.class"), b"replacement class");
}
#[test]
fn whole_signed_jar_replacement_is_allowed() {
    let dir = tempfile::tempdir().unwrap();
    let signed = archive(&[
        ("META-INF/X.RSA", b"block", zip::CompressionMethod::Stored),
        (
            "META-INF/X.SF",
            b"signature",
            zip::CompressionMethod::Stored,
        ),
    ]);
    let root = dir.path().join("root.zip");
    fs::write(
        &root,
        archive(&[("signed.jar", &signed, zip::CompressionMethod::Stored)]),
    )
    .unwrap();
    let source = dir.path().join("replacement.jar");
    fs::write(&source, &signed).unwrap();
    apply(
        &Plan {
            root,
            ops: vec![Op::Replace {
                target: Chain::from("signed.jar"),
                source,
            }],
        },
        &ApplyOptions::default(),
        &Cache::new().unwrap(),
        &|_| {},
        &CancelToken::default(),
    )
    .unwrap();
}

#[test]
fn replaces_gbk_name_and_forced_zip64_local_header() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root.zip");
    let source = dir.path().join("patch");
    fs::write(&source, b"new payload").unwrap();
    let bytes = metadata_fixture(b"x", b"old");
    fs::write(&root, &bytes).unwrap();
    apply(
        &Plan {
            root: root.clone(),
            ops: vec![Op::Replace {
                target: Chain::from("中文.txt"),
                source: source.clone(),
            }],
        },
        &ApplyOptions {
            backup: false,
            write_log: false,
        },
        &Cache::new().unwrap(),
        &|_| {},
        &CancelToken::default(),
    )
    .unwrap();
    let next = fs::read(&root).unwrap();
    let d = Directory::read(&mut Cursor::new(next)).unwrap();
    let rec = d.records.iter().find(|r| r.name == "中文.txt").unwrap();
    assert_eq!(rec.raw_name, encoding_rs::GBK.encode("中文.txt").0.as_ref());
    assert_eq!(rec.size, 11);
    let mut payload = vec![];
    Cache::new()
        .unwrap()
        .read_entry(&root, &Chain::from("中文.txt"))
        .unwrap()
        .read_to_end(&mut payload)
        .unwrap();
    assert_eq!(payload, b"new payload");
    let mut z = zip::ZipWriter::new(fs::File::create(&root).unwrap());
    z.start_file(
        "x",
        zip::write::SimpleFileOptions::default().large_file(true),
    )
    .unwrap();
    z.write_all(b"before").unwrap();
    z.finish().unwrap();
    apply(
        &Plan {
            root: root.clone(),
            ops: vec![Op::Replace {
                target: Chain::from("x"),
                source,
            }],
        },
        &ApplyOptions {
            backup: false,
            write_log: false,
        },
        &Cache::new().unwrap(),
        &|_| {},
        &CancelToken::default(),
    )
    .unwrap();
    assert_eq!(get(&fs::read(root).unwrap(), "x"), b"new payload");
}
#[test]
#[cfg(target_os = "macos")]
fn preserves_macos_acl_and_extended_attributes() {
    use std::process::Command;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root.zip");
    let source = dir.path().join("patch");
    fs::write(&source, b"new").unwrap();
    fs::write(
        &root,
        archive(&[("x", b"old", zip::CompressionMethod::Stored)]),
    )
    .unwrap();
    assert!(
        Command::new("/usr/bin/xattr")
            .args(["-w", "zipr.test", "metadata"])
            .arg(&root)
            .status()
            .unwrap()
            .success()
    );
    assert!(
        Command::new("/bin/chmod")
            .args(["+a", "everyone allow read"])
            .arg(&root)
            .status()
            .unwrap()
            .success()
    );
    apply(
        &Plan {
            root: root.clone(),
            ops: vec![Op::Replace {
                target: Chain::from("x"),
                source,
            }],
        },
        &ApplyOptions {
            backup: false,
            write_log: false,
        },
        &Cache::new().unwrap(),
        &|_| {},
        &CancelToken::default(),
    )
    .unwrap();
    let attr = Command::new("/usr/bin/xattr")
        .args(["-p", "zipr.test"])
        .arg(&root)
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&attr.stdout).trim(), "metadata");
    let acl = Command::new("/bin/ls")
        .arg("-le")
        .arg(&root)
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&acl.stdout).contains("everyone allow read"));
}
#[test]
fn cache_reuses_inflates_and_remembers_signature_probes() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root.zip");
    let inner = archive(&[
        ("META-INF/X.SF", b"sig", zip::CompressionMethod::Stored),
        ("META-INF/X.RSA", b"block", zip::CompressionMethod::Stored),
    ]);
    fs::write(
        &root,
        archive(&[("inner.jar", &inner, zip::CompressionMethod::Deflated)]),
    )
    .unwrap();
    let cache = Cache::new().unwrap();
    assert_eq!(
        cache.list_children(&root, &Chain::default()).unwrap()[0].is_signed,
        None
    );
    let events = std::cell::Cell::new(0);
    // Cached opens report completion, but must never start another inflate.
    let p = |event: ProgressEvent| {
        if event.done == 0 {
            events.set(events.get() + 1);
        }
    };
    cache
        .open(
            &root,
            &Chain::from("inner.jar"),
            &p,
            &CancelToken::default(),
        )
        .unwrap();
    assert!(events.get() > 0);
    events.set(0);
    cache
        .open(
            &root,
            &Chain::from("inner.jar"),
            &p,
            &CancelToken::default(),
        )
        .unwrap();
    assert_eq!(events.get(), 0);
    assert!(cache.is_signed(&root, &Chain::from("inner.jar")).unwrap());
    assert_eq!(
        cache.list_children(&root, &Chain::default()).unwrap()[0].is_signed,
        Some(true)
    );
    cache.clear().unwrap();
    assert_eq!(
        cache.list_children(&root, &Chain::default()).unwrap()[0].is_signed,
        None
    );
    fs::write(
        &root,
        archive(&[(
            "inner.jar",
            &archive(&[("longer-name", b"changed", zip::CompressionMethod::Stored)]),
            zip::CompressionMethod::Deflated,
        )]),
    )
    .unwrap();
    assert!(!cache.is_signed(&root, &Chain::from("inner.jar")).unwrap());
}

#[test]
fn signature_requires_matching_nonempty_sf_and_block() {
    use zip::CompressionMethod::Stored;
    let cases = [
        (vec![("META-INF/BOOT.SF", b"".as_slice())], false),
        (
            vec![
                ("META-INF/KEY.SF", b"signature".as_slice()),
                ("META-INF/KEY.RSA", b"block".as_slice()),
            ],
            true,
        ),
        (vec![("META-INF/KEY.RSA", b"block".as_slice())], false),
        (
            vec![
                ("META-INF/KEY.SF", b"".as_slice()),
                ("META-INF/KEY.RSA", b"block".as_slice()),
            ],
            false,
        ),
        (
            vec![
                ("META-INF/OTHER.SF", b"signature".as_slice()),
                ("META-INF/KEY.RSA", b"block".as_slice()),
            ],
            false,
        ),
        (
            vec![
                ("meta-inf/key.sf", b"signature".as_slice()),
                ("META-INF/KEY.EC", b"block".as_slice()),
            ],
            true,
        ),
        (
            vec![
                ("META-INF/KEY.SF", b"signature".as_slice()),
                ("META-INF/KEY.DSA", b"block".as_slice()),
            ],
            true,
        ),
    ];
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("patch");
    fs::write(&source, b"new").unwrap();
    for (i, (files, expected)) in cases.into_iter().enumerate() {
        let root = dir.path().join(format!("case{i}.jar"));
        let mut entries: Vec<_> = files.into_iter().map(|(n, b)| (n, b, Stored)).collect();
        entries.push(("x", b"old", Stored));
        fs::write(&root, archive(&entries)).unwrap();
        let cache = Cache::new().unwrap();
        assert_eq!(cache.is_signed(&root, &Chain::default()).unwrap(), expected);
        let result = apply(
            &Plan {
                root,
                ops: vec![Op::Replace {
                    target: Chain::from("x"),
                    source: source.clone(),
                }],
            },
            &ApplyOptions {
                backup: false,
                write_log: false,
            },
            &cache,
            &|_| {},
            &CancelToken::default(),
        );
        if expected {
            assert!(matches!(result, Err(EngineError::SignedJar { .. })));
        } else {
            result.unwrap();
        }
    }
}

#[test]
fn backup_and_patch_log_use_local_transaction_time() {
    // Run this under a non-UTC TZ to catch accidentally returning to now_utc.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root.zip");
    fs::write(
        &root,
        archive(&[("a", b"old", zip::CompressionMethod::Stored)]),
    )
    .unwrap();
    let source = dir.path().join("a");
    fs::write(&source, b"new").unwrap();
    let before = chrono::Local::now();
    let report = apply(
        &Plan {
            root,
            ops: vec![Op::Replace {
                target: Chain::from("a"),
                source,
            }],
        },
        &ApplyOptions::default(),
        &Cache::new().unwrap(),
        &|_| {},
        &CancelToken::default(),
    )
    .unwrap();
    let after = chrono::Local::now();
    let backup = report
        .backup_path
        .unwrap()
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let log = report
        .log_path
        .unwrap()
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let stamp = &backup["root.zip.bak-".len()..];
    let parsed = chrono::NaiveDateTime::parse_from_str(stamp, "%Y%m%d-%H%M%S").unwrap();
    assert!(parsed >= before.naive_local() - chrono::Duration::seconds(1));
    assert!(parsed <= after.naive_local());
    assert!(log.starts_with(&format!("root.zip.patch-{stamp}-")));
}

#[cfg(windows)]
#[test]
fn foreign_exclusive_handle_is_locked_but_idle_cache_is_not() {
    use std::os::windows::fs::OpenOptionsExt;
    use zip::CompressionMethod::{Deflated, Stored};

    for method in [Stored, Deflated] {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root.zip");
        let inner = archive(&[("x", b"old", method)]);
        let original = archive(&[("inner.jar", &inner, method)]);
        fs::write(&root, &original).unwrap();
        let source = dir.path().join("patch");
        fs::write(&source, b"new payload").unwrap();
        let cache = Cache::new().unwrap();
        let target = Chain::from("inner.jar!/x");
        cache.list_children(&root, &Chain::default()).unwrap();
        cache
            .list_children(&root, &Chain::from("inner.jar"))
            .unwrap();
        let mut bytes = Vec::new();
        cache
            .read_entry(&root, &target)
            .unwrap()
            .read_to_end(&mut bytes)
            .unwrap();
        assert_eq!(bytes, b"old");

        // This handle is foreign to the engine. Acquiring it also proves that
        // list/read/inflate left no root handle in the still-live cache.
        let foreign = fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&root)
            .unwrap();
        let plan = Plan {
            root: root.clone(),
            ops: vec![Op::Replace { target, source }],
        };
        let result = apply(
            &plan,
            &ApplyOptions::default(),
            &cache,
            &|_| {},
            &CancelToken::default(),
        );
        assert!(matches!(result, Err(EngineError::FileLocked)), "{result:?}");
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 2);
        drop(foreign);
        assert_eq!(fs::read(&root).unwrap(), original);

        let report = apply(
            &plan,
            &ApplyOptions::default(),
            &cache,
            &|_| {},
            &CancelToken::default(),
        )
        .unwrap();
        assert_eq!(
            get(&get(&fs::read(&root).unwrap(), "inner.jar"), "x"),
            b"new payload"
        );
        let backup = report.backup_path.unwrap();
        assert_eq!(fs::read(&backup).unwrap(), original);
        zipr_lib::archive::restore_backup(&root, &backup).unwrap();
        assert_eq!(fs::read(&root).unwrap(), original);
        assert!(!backup.exists());
        cache.clear().unwrap();
        // Successful commit/restore also leave no handles on the root or log.
        let exclusive = fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&root)
            .unwrap();
        drop(exclusive);
        fs::remove_file(&root).unwrap();
        fs::remove_file(report.log_path.unwrap()).unwrap();
    }
}

#[test]
fn warmed_nested_apply_emits_completed_progress_for_each_phase() {
    use std::cell::RefCell;
    use zip::CompressionMethod::Deflated;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root.zip");
    let source = dir.path().join("entry.txt");
    let inner = archive(&[("entry.txt", b"old", Deflated)]);
    fs::write(&root, archive(&[("inner.zip", &inner, Deflated)])).unwrap();
    fs::write(&source, b"replacement").unwrap();
    let cache = Cache::new().unwrap();
    cache
        .list_children(&root, &Chain::from("inner.zip"))
        .unwrap();
    let events = RefCell::new(Vec::new());
    apply(
        &Plan {
            root,
            ops: vec![Op::Replace {
                target: Chain::from("inner.zip!/entry.txt"),
                source,
            }],
        },
        &ApplyOptions {
            backup: false,
            write_log: false,
        },
        &cache,
        &|event: ProgressEvent| events.borrow_mut().push(event),
        &CancelToken::default(),
    )
    .unwrap();
    for phase in ["Inflate", "Rewrite", "Verify", "Commit"] {
        assert!(
            events
                .borrow()
                .iter()
                .any(|e| format!("{:?}", e.phase) == phase && e.total > 0 && e.done == e.total),
            "{phase}"
        );
    }
}

#[test]
fn io_error_serialization_keeps_path_and_original_os_message() {
    let os = std::io::Error::from_raw_os_error(2);
    let original = os.to_string();
    let error = EngineError::from(os).with_path("missing.zip");
    let value = serde_json::to_value(error).unwrap();
    assert_eq!(value["kind"], "io");
    assert_eq!(value["paths"][0], "missing.zip");
    assert!(value["message"].as_str().unwrap().contains(&original));
}
