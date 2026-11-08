# M1 engine API

`zipr_lib` exposes a synchronous Rust API with serde results and typed errors.
Run it on a background worker in Tauri. The engine does not require a UI runtime.

```rust,no_run
use zipr_lib::{apply, ApplyOptions, Cache, CancelToken, Chain, Op, Plan};

# fn example() -> Result<(), zipr_lib::EngineError> {
let cache = Cache::new()?;
let cancel = CancelToken::default();
let root = std::path::PathBuf::from("component.zip");
let entries = cache.list_children(&root, &Chain::default())?;
let plan = Plan {
    root,
    ops: vec![Op::Replace {
        target: Chain::from("app.jar!/BOOT-INF/lib/owned.jar!/demo/Foo.class"),
        source: "patches/Foo.class".into(),
    }],
};
let report = apply(&plan, &ApplyOptions::default(), &cache, &|event| {
    // Forward ProgressEvent to the frontend; avoid blocking this callback.
    println!("{event:?}");
}, &cancel)?;
# Ok(())
# }
```

## Paths and sessions

`Chain(Vec<String>)` is relative to `Plan.root`. Each element is a complete path
inside one archive. `Chain::default()` denotes the root archive. `!/` separates
archive boundaries; ordinary `/` remains part of an entry name. Do not prepend
the root filesystem path to a `Chain`.

`Cache::open(root, chain, progress, cancel)` returns a `Source: Read + Seek`.
STORED entries are bounded views of the original file, including consecutive
STORED nesting. DEFLATED entries are inflated into the session's temporary
directory once, checking CRC and size. Keys contain the canonical root path,
size, modification time, and complete chain. Reuse the same Cache for browsing,
matching, and apply. `clear()` removes cached files; dropping Cache removes the
session directory. Close outstanding Source readers before clearing on Windows.

`list_children` reads one archive's central directory without opening child
archives. Entry names retain their complete paths inside that archive; the UI
can group these into directory nodes. An optional trailing directory segment,
for example `app.jar!/BOOT-INF/lib/`, filters the listing by that prefix.
`list_children_with_progress` accepts progress and cancellation for cold inflates.

`EntryInfo` includes decoded name, original name bytes, `name_is_utf8`, directory
and archive flags, stored/deflated/other method, sizes, CRC, packed DOS mtime
(date in high 16 bits, time in low 16 bits), and lazy `is_signed: None`.
`Cache::is_signed` explicitly probes signatures. UTF-8 names are decoded directly;
otherwise GBK is used. All writes retain original name bytes.

`read_entry` returns a bounded reader. `extract` streams into a destination file.
For cancellable extraction, use `open` with a token and copy in chunks while
checking the same token.

## Apply transaction

`Op::Replace` selects an existing entry. `Op::Add { parent, name, source }` adds a
new path inside the selected archive. Duplicate and ancestor/descendant conflicts
are rejected. Every source and target is preflighted. The root is checked for a
Windows sharing lock, available disk space is conservatively estimated, and
signed ancestor archives are refused. Replacing a signed jar as an entire entry
is permitted. A jar is signed only when META-INF directly contains a signature
block (`*.RSA`, `*.DSA`, or `*.EC`) AND a matching non-empty `*.SF` with the same
base name (case-insensitive). A lone block, unmatched SF, or empty SF does not
qualify. In particular, an empty `META-INF/BOOT.SF` is a packaging marker, not a
signature. Both `Cache::is_signed` and apply preflight use this same rule.

Affected archives are grouped and rewritten bottom-up. Intermediate archives go
to temporary files, and cached inflates are reused. Verification happens before
commit: rewritten entries' local headers, central directory, CRCs and sizes are
checked. Rewritten child archives have already been verified before being
embedded in their parent. Untouched payloads are not inflated for verification.

The final output is in the root's directory. Source-file mode is retained. Unix
uses atomic persist/rename; Windows uses `ReplaceFileW` to preserve the original
security descriptor and associated streams. A final metadata check detects
root size/mtime changes during apply. This is not a cross-process editing lock.

Default `ApplyOptions` enables backup and log. Backups use
`<root>.bak-YYYYMMDD-HHMMSS`, with a numeric suffix on collisions. They use a hard
link where possible, otherwise an exclusive copy. Logs use
`<root>.patch-YYYYMMDD-HHMMSS-<nanoseconds>.json`; they are prepared before commit.
They contain source paths, full target chains, before/after SHA-256 hashes,
rewritten archive chains, and phase durations. The log's commit/total timing is
measured immediately before the atomic replacement; the returned report includes
the replacement as well. Cancellation before commit removes temporary output and
leaves the original unchanged. Errors serialize as `kind`, `message`, and optional
`chain`.

Progress phases are Inflate, Rewrite, Verify, and Commit, with chain and byte
counters. Compression checks cancellation per input chunk; copied output also
checks per chunk. Call `CancelToken::cancel()` from another thread or a progress
callback. Commit is a short atomic replacement, not an interruptible partial write.

## ZIP fidelity and compression

`splice::rewrite(Read + Seek, Write, changes, cancel)` does not use
`ZipWriter::raw_copy_file`. It copies untouched local-header/data/descriptor spans
verbatim and copies central records with only local offsets adjusted. Prefix
launch scripts, comments, attributes, extra fields, and ZIP64 trailers survive.
Replacements preserve compression method, raw name, flags other than descriptor
handling, versions, attributes, comments and extra fields. CRC/sizes are rebuilt;
mtime comes from the source, including existing 0x5455 fields. ZIP64 fields are
updated or promoted as required. New archive entries are STORED; other new files
are DEFLATED. Missing parents are emitted when explicit directories are in use.

DEFLATE uses level 6. Above 8 MiB, independent 1 MiB chunks are compressed by
available CPU workers, ending with non-final sync-flush boundaries and a single
final block. This is one standard raw DEFLATE stream. Memory is bounded by worker
count and central-directory metadata, not archive payload size. Independent
chunks trade a little compression ratio for bounded memory and parallelism.

Adding a direct jar under BOOT-INF/lib or WEB-INF/lib updates existing classpath
and layer indexes. Directory-prefix layer coverage is honored; otherwise the
application layer, or last layer, receives the new path. Explicit replacement
of an index combined with a jar addition is rejected as a conflicting plan.

## Matching helpers

- `class_info(bytes)` returns JVM internal name and class-file major version.
- `Cache::java_target_of` samples boot classes, then suggested owned libraries,
  then manifest Build-Jdk-Spec/Java-Version.
- `Cache::index_names` builds filename-to-Chain candidates, with cancellation and
  inflate progress. Scope carries include chains and jar filename globs. Nested
  BOOT-INF/lib and WEB-INF/lib jars require a matching glob; plugins are excluded
  unless explicitly included.
- `Cache::suggest_owned_globs` groups library prefixes and version suffixes and
  returns dominant four-component version groups with counts. It is a heuristic,
  and the UI should ask the user to confirm these groups.

## CLI compatibility

`zipr list archive.zip --json` returns a lazy central-directory listing.
`zipr patch apply archive.zip --spec patch.toml --json` returns a JSON summary.
All replacement plans and the standalone `replace` command use the new engine.
Legacy method, fixed compression-level, and mtime overrides have been removed:
unsupported values fail explicitly before writing (also in dry-run). Supported
policies are `method=inherit`, `level=inherit` or `default` (both level 6), and
`mtime=source`. They cannot fall back to the old writer. Mixed replace/delete
plans are rejected; run deletion separately. Delete-only operations retain the
legacy implementation and are outside the new replace/add fidelity guarantee.
The new typed Plan API exposes replace/add with M1 compression and timestamps.
Unprefixed nested patch paths retain their first segment.

## Validation and benchmarks

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo test --test engine_boot -- --ignored --nocapture
cargo run --release --example bench -- /path/to/release.zip \
  app-starter.zip app.jar BOOT-INF/lib/owned-batch-1.0.0.jar \
  com/example/Worker.class BOOT-INF/lib/owned-common-1.0.0.jar \
  config/app-config.xml 'owned-*' 'app-*'
# Exact M1 targets, retaining before.zip, after.zip, and patch sources:
cargo run --release --example bench_real -- /path/to/release.zip /path/to/new/qa-directory \
  app-starter.zip app.jar BOOT-INF/lib/owned-batch-1.0.0.jar \
  com/example/Worker.class BOOT-INF/lib/owned-common-1.0.0.jar config/app-config.xml
```

Entry arguments are exact paths within their containing archive. The optional
`bench` globs control indexing; by default it indexes the supplied class library.

Two generated startup tests are runnable with the ignored-test command above:

- Spring Boot loader 2.7.18 on Java 8: JAVA_HOME defaults to `~/.local/sdks/java`.
- Spring Boot loader 3.5.0 on Java 17: JAVA17_HOME defaults to
  `~/Library/Java/JavaVirtualMachines/openjdk-17.0.2/Contents/Home`.
  This test uses `org.springframework.boot.loader.launch.JarLauncher`, includes
  the empty BOOT.SF marker, and asserts the patched class is actually read from
  a `NestedJarFile`. It verifies both pre-patch and post-patch application output.

MAVEN_BIN defaults to `~/.local/sdks/maven/bin/mvn`. Maven fetches the pinned
loader versions if absent. Both tests were run and passed. Fixtures are generated
in temporary directories; no binary fixtures are committed.

Measured on macOS in release mode using a 1.08 GB release archive with 8 component zips, 447 STORED BOOT-INF/lib jars, 4 nesting levels:

| Operation | Measured | Target |
|---|---:|---:|
| Root STORED listing | 0.37 ms | <100 ms |
| Component STORED listing | 0.45 ms | <100 ms |
| Boot first listing, including inflate | 1,204 ms | reported |
| Boot cached listing | 3.18 ms | <300 ms |
| Cold owned-name index | 1.635 s | <5 s |
| Exact real-component three-op apply, backup + verify | 3.550 s | <15 s |
| Peak benchmark-process RSS (preparation + apply) | 104.75 MiB | <500 MB |

Real acceptance apply phase timings: preflight **31 ms**, inflate **0 ms**
(warm planning cache reused), rewrite **2,605 ms**, verify **797 ms**, commit
**65 ms**. Reported engine total is 3,529 ms; externally measured apply wall time
is 3,550 ms, including temporary-intermediate cleanup. Peak RSS was collected with
`resource.getrusage(RUSAGE_CHILDREN)` on macOS and covers the entire benchmark
child, including fixture preparation.

The benchmark retains the following artifacts in the caller-selected output directory:
`before.zip`, `after.zip`, `patch/`, the timestamped backup and patch log,
`plan.json`, `report.json`, `measurement.json`, and `validation.json`.
The component was extracted from the original release archive without modifying it.
Unlike ordinary temporary tests, these copies are intentionally retained for QA.
`bench_real` refuses to overwrite an existing artifact set.

All three target payloads differ from their originals (paths below are generic placeholders):

- `BOOT-INF/lib/owned-batch-1.0.0.jar!/com/example/Worker.class`:
  SourceFile debug-name constant extended with `.m1`; instruction bytes unchanged.
- `config/app-config.xml`:
  a trailing XML comment added.
- `BOOT-INF/lib/owned-common-1.0.0.jar`:
  same jar filename, with `META-INF/zipr-m1-acceptance.txt` added to its contents.

The boot jar path is `app.jar`.
Independent validation confirmed backup equality, source/output SHA-256 matches,
STORED nested jars, unchanged empty BOOT.SF, and clean `unzip -t` on all four
output archives. All untouched local spans and central records (except offsets)
matched: 48 component entries, 598 boot entries, and 253 batch entries.
There is no signature-stripping or marker-renaming benchmark workaround.

Windows all-target cross-compilation passed using the rustup toolchain.
Remaining platform coverage: Windows lock/ACL/atomic replacement requires
Windows runtime QA. macOS ACLs and extended attributes are copied with copyfile; other Unix
platforms copy extended attributes, including filesystem ACL attributes. Multi-disk ZIPs and encrypted replacement
entries are unsupported. ZIP64 count, forced local-size, and offset-promotion
fixtures are tested; a physical >4 GiB rewrite has not been benchmarked here.

Automated results: formatting, strict all-target Clippy, the full Cargo suite,
Windows all-target compile check, and both explicitly invoked generated Boot startup tests pass. Coverage includes metadata identity through three levels,
STORED nesting with unzip checks, additions/indexes, signed refusal/whole-jar
replacement, cancellation after writing begins, injected verify failure,
parallel raw DEFLATE, 65,536-entry ZIP64 (including a prepended launch script),
forced ZIP64 sizes/offset promotion, cache reuse/invalidation, GBK replacement,
Unix mode plus macOS ACL/xattr preservation, and CLI JSON/path regression tests.
The older Java 17 asset tests self-skip when Java 17 is not installed; the new
Java 8 and Java 17/Boot 3.5 tests were run explicitly and started the patched applications.
