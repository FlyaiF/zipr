use std::fs;
use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Result, bail};
use wait_timeout::ChildExt;
use zip::CompressionMethod;
use zip::ZipArchive;

const FIRST_CLASS: &str = "BOOT-INF/classes/com/example/sbpkg/FirstPrinter.class";
const MODULE_A_JAR: &str = "BOOT-INF/lib/sb-modulea-0.0.1-SNAPSHOT.jar";
const MODULE_A_CLASS: &str = "com/example/sbpkg/modulea/PrintFromProperties.class";
const MODULE_A_PROP: &str = "print.properties";
const MODULE_B_JAR: &str = "BOOT-INF/lib/sb-moduleb-0.0.1-SNAPSHOT.jar";
const MODULE_B_CLASS: &str = "com/example/sbpkg/moduleb/PrintFromModule.class";

#[test]
fn sbpkg_replace_and_run_on_java17_plus() -> Result<()> {
    if !has_java_17_plus() {
        eprintln!("skip sbpkg_replace_and_run_on_java17_plus: Java 17+ not found");
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let jar = dir.path().join("sb-pkg.jar");
    fs::copy(asset("sb-pkg.jar"), &jar)?;

    let first_before = entry_method(&jar, &[], FIRST_CLASS)?;
    let mod_a_before = entry_method(&jar, &[MODULE_A_JAR], MODULE_A_CLASS)?;
    let mod_b_before = entry_method(&jar, &[MODULE_B_JAR], MODULE_B_CLASS)?;

    run_zipr_replace(
        dir.path(),
        &jar,
        FIRST_CLASS,
        &asset("FirstPrinter.class"),
    )?;
    run_zipr_replace(
        dir.path(),
        &jar,
        &format!("{MODULE_A_JAR}!/{MODULE_A_CLASS}"),
        &asset("PrintFromProperties.class"),
    )?;
    run_zipr_replace(
        dir.path(),
        &jar,
        &format!("{MODULE_B_JAR}!/{MODULE_B_CLASS}"),
        &asset("PrintFromModule.class"),
    )?;
    run_zipr_replace(
        dir.path(),
        &jar,
        &format!("{MODULE_A_JAR}!/{MODULE_A_PROP}"),
        &asset("print.properties"),
    )?;

    assert_eq!(
        run_zipr_get(dir.path(), &expr(&jar, FIRST_CLASS))?,
        fs::read(asset("FirstPrinter.class"))?
    );
    assert_eq!(
        run_zipr_get(dir.path(), &expr(&jar, &format!("{MODULE_A_JAR}!/{MODULE_A_CLASS}")))?,
        fs::read(asset("PrintFromProperties.class"))?
    );
    assert_eq!(
        run_zipr_get(dir.path(), &expr(&jar, &format!("{MODULE_B_JAR}!/{MODULE_B_CLASS}")))?,
        fs::read(asset("PrintFromModule.class"))?
    );
    assert_eq!(
        run_zipr_get(dir.path(), &expr(&jar, &format!("{MODULE_A_JAR}!/{MODULE_A_PROP}")))?,
        fs::read(asset("print.properties"))?
    );

    assert_eq!(entry_method(&jar, &[], FIRST_CLASS)?, first_before);
    assert_eq!(entry_method(&jar, &[MODULE_A_JAR], MODULE_A_CLASS)?, mod_a_before);
    assert_eq!(entry_method(&jar, &[MODULE_B_JAR], MODULE_B_CLASS)?, mod_b_before);

    let run_output = run_java_jar_with_timeout(&jar, Duration::from_secs(25))?;
    assert!(run_output.contains(":: printSelf modified."));
    assert!(run_output.contains(":: module b modified."));
    assert!(run_output.contains("from print.properties modified"));
    Ok(())
}

#[test]
fn sbpkg_patch_apply_and_run_on_java17_plus() -> Result<()> {
    if !has_java_17_plus() {
        eprintln!("skip sbpkg_patch_apply_and_run_on_java17_plus: Java 17+ not found");
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let jar = dir.path().join("sb-pkg.jar");
    fs::copy(asset("sb-pkg.jar"), &jar)?;
    let patch_file = dir.path().join("patch.sbpkg.toml");
    let jar_norm = normalize(jar.clone());

    let spec = format!(
        r#"
version = 1

[[entry]]
target = "{jar_norm}!/{FIRST_CLASS}"
source = "{first_src}"
action = "replace"
method = "inherit"
level = "inherit"
mtime = "source"
comment = "inherit"

[[entry]]
target = "{jar_norm}!/{MODULE_A_JAR}!/{MODULE_A_CLASS}"
source = "{mod_a_src}"
action = "replace"
method = "inherit"
level = "inherit"
mtime = "source"
comment = "inherit"

[[entry]]
target = "{jar_norm}!/{MODULE_B_JAR}!/{MODULE_B_CLASS}"
source = "{mod_b_src}"
action = "replace"
method = "inherit"
level = "inherit"
mtime = "source"
comment = "inherit"

[[entry]]
target = "{jar_norm}!/{MODULE_A_JAR}!/{MODULE_A_PROP}"
source = "{prop_src}"
action = "replace"
method = "inherit"
level = "inherit"
mtime = "source"
comment = "inherit"
"#,
        first_src = normalize(asset("FirstPrinter.class")),
        mod_a_src = normalize(asset("PrintFromProperties.class")),
        mod_b_src = normalize(asset("PrintFromModule.class")),
        prop_src = normalize(asset("print.properties")),
    );
    fs::write(&patch_file, spec)?;

    run_zipr_patch_apply(dir.path(), &jar, &patch_file, true)?;
    run_zipr_patch_apply(dir.path(), &jar, &patch_file, false)?;

    let run_output = run_java_jar_with_timeout(&jar, Duration::from_secs(25))?;
    assert!(run_output.contains(":: printSelf modified."));
    assert!(run_output.contains(":: module b modified."));
    assert!(run_output.contains("from print.properties modified"));
    Ok(())
}

fn has_java_17_plus() -> bool {
    let out = Command::new("java").arg("-version").output();
    let Ok(out) = out else { return false };
    let text = format!(
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    parse_java_major(&text).is_some_and(|v| v >= 17)
}

fn parse_java_major(s: &str) -> Option<u32> {
    let marker = "version \"";
    let start = s.find(marker)? + marker.len();
    let rest = &s[start..];
    let end = rest.find('"')?;
    let ver = &rest[..end];
    let mut parts = ver.split('.');
    let first = parts.next()?.parse::<u32>().ok()?;
    if first == 1 {
        parts.next()?.parse::<u32>().ok()
    } else {
        Some(first)
    }
}

fn run_zipr_replace(cwd: &Path, jar: &Path, target: &str, src: &Path) -> Result<()> {
    let expr = expr(jar, target);
    let out = bin_cmd(cwd)
        .arg("replace")
        .arg(&expr)
        .arg(src)
        .output()?;
    if !out.status.success() {
        bail!("replace failed for {expr}: {}", String::from_utf8_lossy(&out.stderr));
    }
    Ok(())
}

fn run_zipr_get(cwd: &Path, expr: &str) -> Result<Vec<u8>> {
    let out = bin_cmd(cwd).arg("get").arg(expr).output()?;
    if !out.status.success() {
        bail!("get failed for {expr}: {}", String::from_utf8_lossy(&out.stderr));
    }
    Ok(out.stdout)
}

fn run_zipr_patch_apply(cwd: &Path, jar: &Path, spec: &Path, dry_run: bool) -> Result<()> {
    let mut cmd = bin_cmd(cwd);
    cmd.arg("patch")
        .arg("apply")
        .arg(jar)
        .arg("--spec")
        .arg(spec);
    if dry_run {
        cmd.arg("--dry-run");
    }
    let out = cmd.output()?;
    if !out.status.success() {
        bail!(
            "patch apply failed (dry_run={}): {}",
            dry_run,
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(())
}

fn bin_cmd(cwd: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_zipr"));
    cmd.current_dir(cwd);
    cmd
}

fn run_java_jar_with_timeout(jar: &Path, timeout: Duration) -> Result<String> {
    let mut child = Command::new("java")
        .arg("-jar")
        .arg(jar)
        .arg("--spring.main.web-application-type=none")
        .arg("--spring.main.banner-mode=off")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    let status = child.wait_timeout(timeout)?;
    if status.is_none() {
        let _ = child.kill();
        let _ = child.wait();
        bail!("java -jar timed out after {:?}", timeout);
    }
    let out = child.wait_with_output()?;
    let text = format!(
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    Ok(text)
}

fn entry_method(outer_jar: &Path, chain: &[&str], target: &str) -> Result<CompressionMethod> {
    let mut data = fs::read(outer_jar)?;
    for seg in chain {
        let bytes = {
            let mut arc = ZipArchive::new(Cursor::new(&data))?;
            let mut nested = arc.by_name(seg)?;
            let mut bytes = Vec::new();
            nested.read_to_end(&mut bytes)?;
            bytes
        };
        data = bytes;
    }
    let mut arc = ZipArchive::new(Cursor::new(&data))?;
    let f = arc.by_name(target)?;
    Ok(f.compression())
}

fn expr(jar: &Path, target: &str) -> String {
    format!("{}!/{target}", normalize(jar.to_path_buf()))
}

fn asset(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("assets")
        .join(name)
}

fn normalize(path: impl Into<PathBuf>) -> String {
    path.into().to_string_lossy().replace('\\', "/")
}
