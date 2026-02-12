use std::env;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use walkdir::WalkDir;
use zip::CompressionMethod;
use zip::write::SimpleFileOptions;

fn main() -> Result<()> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    env::set_current_dir(&root)?;

    let bin_name = "zipr";
    let version = package_version(&root)?;
    let target_triple = host_target()?;
    let git_rev = git_revision();
    let package_name = format!("{bin_name}-v{version}-{target_triple}-{git_rev}");
    let dist_root = root.join("dist");
    let out_dir = dist_root.join(&package_name);

    println!("[1/4] build release binary");
    run_checked(
        Command::new("cargo")
            .arg("build")
            .arg("--release")
            .arg("--bin")
            .arg(bin_name)
            .env("GIT_HEAD_REV", &git_rev),
    )?;

    println!("[2/4] prepare package layout");
    if out_dir.exists() {
        fs::remove_dir_all(&out_dir)?;
    }
    fs::create_dir_all(&out_dir)?;

    let exe_name = if cfg!(windows) {
        format!("{bin_name}.exe")
    } else {
        bin_name.to_string()
    };
    copy_file(
        root.join("target/release").join(&exe_name),
        out_dir.join(&exe_name),
    )?;
    copy_file(root.join("README.md"), out_dir.join("README.md"))?;
    copy_file(
        root.join("docs/USER_MANUAL_ZH.md"),
        out_dir.join("USER_MANUAL_ZH.md"),
    )?;
    copy_file(
        root.join("tests/assets/patch.sbpkg.toml"),
        out_dir.join("patch.example.toml"),
    )?;

    println!("[3/4] write build metadata");
    let built_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mut f = fs::File::create(out_dir.join("BUILD_INFO.txt"))?;
    writeln!(f, "name={bin_name}")?;
    writeln!(f, "version={version}")?;
    writeln!(f, "git_revision={git_rev}")?;
    writeln!(f, "target={target_triple}")?;
    writeln!(f, "built_at_unix={built_at}")?;

    println!("[4/4] create zip package");
    fs::create_dir_all(&dist_root)?;
    let zip_path = dist_root.join(format!("{package_name}.zip"));
    create_zip(&out_dir, &zip_path)?;

    println!("done");
    println!("package dir: {}", out_dir.display());
    println!("package zip: {}", zip_path.display());
    Ok(())
}

fn package_version(root: &Path) -> Result<String> {
    let raw = fs::read_to_string(root.join("Cargo.toml"))?;
    for line in raw.lines() {
        let t = line.trim();
        if t.starts_with("version = ") {
            return Ok(t
                .trim_start_matches("version = ")
                .trim_matches('"')
                .to_string());
        }
    }
    bail!("failed to read version from Cargo.toml")
}

fn host_target() -> Result<String> {
    let out = Command::new("rustc").arg("-vV").output()?;
    if !out.status.success() {
        bail!("failed to run `rustc -vV`");
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    for line in stdout.lines() {
        if let Some(host) = line.strip_prefix("host: ") {
            return Ok(host.to_string());
        }
    }
    bail!("failed to parse host target from rustc output")
}

fn git_revision() -> String {
    let out = Command::new("git")
        .args(["rev-parse", "--short=12", "HEAD"])
        .output();
    if let Ok(out) = out {
        if out.status.success() {
            let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !s.is_empty() {
                return s;
            }
        }
    }
    "unknown".to_string()
}

fn run_checked(cmd: &mut Command) -> Result<()> {
    let status = cmd.status()?;
    if status.success() {
        Ok(())
    } else {
        bail!("command failed with status {status}")
    }
}

fn copy_file(src: PathBuf, dst: PathBuf) -> Result<()> {
    fs::copy(&src, &dst)
        .with_context(|| format!("failed to copy {} -> {}", src.display(), dst.display()))?;
    Ok(())
}

fn create_zip(src_dir: &Path, zip_path: &Path) -> Result<()> {
    let file = fs::File::create(zip_path)?;
    let mut writer = zip::ZipWriter::new(file);
    let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);

    for e in WalkDir::new(src_dir).into_iter().filter_map(Result::ok) {
        let path = e.path();
        if path.is_dir() {
            continue;
        }
        let rel = path
            .strip_prefix(src_dir)
            .with_context(|| format!("failed to strip prefix for {}", path.display()))?;
        let zip_name = rel.to_string_lossy().replace('\\', "/");
        writer.start_file(&zip_name, options)?;
        let bytes = fs::read(path)?;
        writer.write_all(&bytes)?;
    }
    writer.finish()?;
    Ok(())
}
