//! Retained M1 acceptance artifacts for caller-selected entries; run without arguments for usage.
use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    time::Instant,
};
use zipr_lib::*;
fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args_os().collect();
    if args.len() != 9 {
        return Err("usage: bench_real SAMPLE.zip OUTPUT_DIRECTORY COMPONENT.zip BOOT.jar CLASS_LIB.jar CLASS_ENTRY REPLACEMENT_LIB.jar CONFIG_ENTRY".into());
    }
    let sample = PathBuf::from(&args[1]);
    let out = PathBuf::from(&args[2]);
    let names: Vec<_> = args[3..]
        .iter()
        .map(|arg| arg.to_str().ok_or("entry names must be UTF-8"))
        .collect::<std::result::Result<_, _>>()?;
    let [
        component_name,
        boot_name,
        batch_name,
        class_name,
        domain_name,
        xml_name,
    ] = names[..]
    else {
        return Err("missing entry names".into());
    };
    fs::create_dir_all(&out)?;
    let before = out.join("before.zip");
    let after = out.join("after.zip");
    let patches = out.join("patch");
    if before.exists() || after.exists() || patches.exists() {
        return Err("refusing to overwrite retained QA artifacts".into());
    }
    fs::create_dir(&patches)?;
    let sample_cache = Cache::new()?;
    let entries = sample_cache.list_children(&sample, &Chain::default())?;
    let component = entries
        .iter()
        .find(|e| e.name == component_name)
        .ok_or("component missing")?;
    sample_cache.extract(&sample, &Chain(vec![component.name.clone()]), &before)?;
    fs::copy(&before, &after)?;
    let cache = Cache::new()?;
    let entries = cache.list_children(&after, &Chain::default())?;
    let boot = entries
        .iter()
        .find(|e| e.name == boot_name)
        .ok_or("boot jar missing")?
        .name
        .clone();
    let class_target = Chain(vec![boot.clone(), batch_name.into(), class_name.into()]);
    let class_source = patches.join(Path::new(class_name).file_name().ok_or("class filename")?);
    let mut bytes = Vec::new();
    cache
        .read_entry(&after, &class_target)?
        .read_to_end(&mut bytes)?;
    let patched = change_source_filename(&bytes)?;
    assert_eq!(
        class_info(&bytes)?.internal_name,
        class_info(&patched)?.internal_name
    );
    fs::write(&class_source, &patched)?;
    let xml_source = patches.join("app-config.xml");
    cache.extract(&after, &Chain::from(xml_name), &xml_source)?;
    fs::OpenOptions::new()
        .append(true)
        .open(&xml_source)?
        .write_all(b"\n<!-- zipr M1 acceptance patch -->\n")?;
    let jar_source = patches.join("owned-common-1.0.0.jar");
    cache.extract(
        &after,
        &Chain(vec![boot.clone(), domain_name.into()]),
        &jar_source,
    )?;
    let marker = out.join("marker-source.txt");
    fs::write(&marker, b"zipr M1 acceptance: modified replacement jar\n")?;
    apply(
        &Plan {
            root: jar_source.clone(),
            ops: vec![Op::Add {
                parent: Chain::default(),
                name: "META-INF/zipr-m1-acceptance.txt".into(),
                source: marker.clone(),
            }],
        },
        &ApplyOptions {
            backup: false,
            write_log: false,
        },
        &Cache::new()?,
        &|_| {},
        &CancelToken::default(),
    )?;
    fs::remove_file(marker)?;
    let plan = Plan {
        root: after.clone(),
        ops: vec![
            Op::Replace {
                target: class_target,
                source: class_source,
            },
            Op::Replace {
                target: Chain::from(xml_name),
                source: xml_source,
            },
            Op::Replace {
                target: Chain(vec![boot, domain_name.into()]),
                source: jar_source,
            },
        ],
    };
    fs::write(out.join("plan.json"), serde_json::to_vec_pretty(&plan)?)?;
    let start = Instant::now();
    let report = apply(
        &plan,
        &ApplyOptions::default(),
        &cache,
        &|_| {},
        &CancelToken::default(),
    )?;
    let wall = start.elapsed().as_secs_f64();
    assert!(
        report
            .results
            .iter()
            .all(|r| r.before.as_ref() != Some(&r.after)),
        "each source must actually change its target"
    );
    let result = serde_json::json!({"apply_wall_seconds":wall,"report":report,"class_change":"SourceFile debug-name constant extended with .m1; bytecode unchanged","xml_change":"trailing XML comment","jar_change":"added META-INF/zipr-m1-acceptance.txt","cache":"warm session reused from patch planning"});
    fs::write(out.join("report.json"), serde_json::to_vec_pretty(&result)?)?;
    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(())
}
/// Modify a UTF8 SourceFile debug filename without changing bytecode or CP indices.
fn change_source_filename(
    bytes: &[u8],
) -> std::result::Result<Vec<u8>, Box<dyn std::error::Error>> {
    if bytes.len() < 10 || bytes[..4] != [0xca, 0xfe, 0xba, 0xbe] {
        return Err("invalid class".into());
    }
    let count = u16::from_be_bytes([bytes[8], bytes[9]]);
    let mut p = 10;
    let mut i = 1;
    while i < count {
        let tag = *bytes.get(p).ok_or("short constant pool")?;
        p += 1;
        let n = match tag {
            1 => {
                let len = bytes.get(p..p + 2).ok_or("short UTF8 length")?;
                let n = u16::from_be_bytes([len[0], len[1]]) as usize;
                let value = bytes.get(p + 2..p + 2 + n).ok_or("short UTF8")?;
                if value.ends_with(b".java") {
                    let len = u16::try_from(n + 3)?;
                    let mut out = bytes[..p].to_vec();
                    out.extend_from_slice(&len.to_be_bytes());
                    out.extend_from_slice(value);
                    out.extend_from_slice(b".m1");
                    out.extend_from_slice(&bytes[p + 2 + n..]);
                    return Ok(out);
                }
                2 + n
            }
            3 | 4 | 9 | 10 | 11 | 12 | 17 | 18 => 4,
            5 | 6 => {
                i += 1;
                8
            }
            7 | 8 | 16 | 19 | 20 => 2,
            15 => 3,
            _ => return Err("unknown constant pool tag".into()),
        };
        p += n;
        i += 1;
    }
    Err("class has no SourceFile debug filename".into())
}
