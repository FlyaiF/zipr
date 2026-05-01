use std::process::Command;

fn main() {
    println!("cargo:rerun-if-env-changed=GIT_HEAD_REV");
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/refs");

    let rev = std::env::var("GIT_HEAD_REV")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(git_head_revision);
    println!("cargo:rustc-env=ZIPR_GIT_REV={rev}");
}

fn git_head_revision() -> String {
    let out = Command::new("git")
        .args(["rev-parse", "--short=12", "HEAD"])
        .output();
    if let Ok(out) = out
        && out.status.success()
    {
        let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !s.is_empty() {
            return s;
        }
    }
    "unknown".to_string()
}
