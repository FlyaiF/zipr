//! Benchmark caller-selected entries; run without arguments for usage.
use std::{path::PathBuf, time::Instant};
use zipr_lib::{engine::*, inspect::Scope};
fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().collect();
    if args.len() < 8 {
        return Err(EngineError::Invalid(
            "usage: bench SAMPLE.zip COMPONENT.zip BOOT.jar CLASS_LIB.jar CLASS_ENTRY REPLACEMENT_LIB.jar CONFIG_ENTRY [JAR_GLOB ...]".into(),
        ));
    }
    let root = PathBuf::from(&args[1]);
    let cache = Cache::new()?;
    let start = Instant::now();
    let entries = cache.list_children(&root, &Chain::default())?;
    println!(
        "root list: {:.2} ms",
        start.elapsed().as_secs_f64() * 1000.0
    );
    let component = entries
        .iter()
        .find(|e| e.name == args[2])
        .ok_or_else(|| EngineError::Invalid("component not found".into()))?;
    let component_chain = Chain(vec![component.name.clone()]);
    let start = Instant::now();
    let entries = cache.list_children(&root, &component_chain)?;
    println!(
        "component list: {:.2} ms",
        start.elapsed().as_secs_f64() * 1000.0
    );
    let boot = entries
        .iter()
        .find(|e| e.name == args[3])
        .ok_or_else(|| EngineError::Invalid("boot jar not found".into()))?;
    let boot_name = boot.name.clone();
    let mut boot_chain = component_chain.clone();
    boot_chain.0.push(boot_name.clone());
    for label in ["first", "cached"] {
        let start = Instant::now();
        let entries = cache.list_children(&root, &boot_chain)?;
        println!(
            "boot lib list {label}: {:.2} ms ({} libraries)",
            start.elapsed().as_secs_f64() * 1000.0,
            entries
                .iter()
                .filter(|e| e.name.starts_with("BOOT-INF/lib/"))
                .count()
        );
    }
    println!(
        "suggestions: {:?}",
        cache.suggest_owned_globs(&root, &boot_chain)?
    );
    cache.clear()?;
    let start = Instant::now();
    let index = cache.index_names(
        &root,
        &component_chain,
        &Scope {
            include_chains: vec![],
            jar_globs: if args.len() > 8 {
                args[8..].to_vec()
            } else {
                vec![args[4].rsplit('/').next().unwrap_or(&args[4]).into()]
            },
        },
        &|_| {},
        &CancelToken::default(),
    )?;
    println!(
        "cold owned index: {:.3} s ({} filenames)",
        start.elapsed().as_secs_f64(),
        index.len()
    );
    let temp = tempfile::tempdir()?;
    let target = temp.path().join("component.zip");
    cache.extract(&root, &component_chain, &target)?;
    let chains = [
        Chain(vec![
            component.name.clone(),
            boot_name.clone(),
            args[4].clone(),
            args[5].clone(),
        ]),
        Chain(vec![component.name.clone(), args[7].clone()]),
        Chain(vec![component.name.clone(), boot_name, args[6].clone()]),
    ];
    let mut ops = vec![];
    for (i, chain) in chains.into_iter().enumerate() {
        let source = temp.path().join(format!("patch{i}"));
        cache.extract(&root, &chain, &source)?;
        ops.push(Op::Replace {
            target: Chain(chain.0[1..].to_vec()),
            source,
        });
    }
    // Warm the same session that Apply receives, as the UI does during planning.
    let target_cache = Cache::new()?;
    target_cache.list_children(&target, &Chain::from(boot.name.as_str()))?;
    let start = Instant::now();
    let report = apply(
        &Plan { root: target, ops },
        &ApplyOptions::default(),
        &target_cache,
        &|_| {},
        &CancelToken::default(),
    )?;
    println!(
        "apply total: {:.3} s; phases: {:?}",
        start.elapsed().as_secs_f64(),
        report.durations_ms
    );
    Ok(())
}
