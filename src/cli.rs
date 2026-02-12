use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;
use clap::{Parser, Subcommand};

use crate::archive::{self, DiffKind, ReplaceOptions};
use crate::config::Config;
use crate::patch_spec::{LevelName, LevelPolicy, MethodPolicy, MtimePolicy};
use crate::path_expr::parse_zip_expr;

const LONG_VERSION: &str = concat!(
    env!("CARGO_PKG_VERSION"),
    "\nrevision: ",
    env!("ZIPR_GIT_REV")
);

#[derive(Debug, Parser)]
#[command(name = "zipr")]
#[command(about = "Recursive zip/jar/war inspector and patch tool")]
#[command(version = env!("CARGO_PKG_VERSION"), long_version = LONG_VERSION)]
pub struct Cli {
    #[arg(
        long,
        global = true,
        help = "Override archive extensions, comma separated"
    )]
    archive_ext: Option<String>,
    #[command(subcommand)]
    cmd: Option<Cmd>,
    #[arg(help = "Archive path for default list mode")]
    archive: Option<PathBuf>,
}

#[derive(Debug, Subcommand)]
enum Cmd {
    List {
        archive: PathBuf,
    },
    Get {
        zip_expr: String,
        #[arg(long)]
        out: Option<PathBuf>,
    },
    Delete {
        zip_expr: String,
    },
    Replace {
        zip_expr: String,
        source: PathBuf,
    },
    Diff {
        left: PathBuf,
        right: PathBuf,
    },
    Patch {
        #[command(subcommand)]
        cmd: PatchCmd,
    },
    Version,
}

#[derive(Debug, Subcommand)]
enum PatchCmd {
    Draft {
        archive: PathBuf,
        #[arg(long)]
        from_dir: PathBuf,
        #[arg(short, long, default_value = "patch.draft.toml")]
        output: PathBuf,
    },
    Apply {
        archive: PathBuf,
        #[arg(long)]
        spec: PathBuf,
        #[arg(long)]
        dry_run: bool,
    },
}

pub fn run() -> Result<()> {
    let cli = Cli::parse();
    let config = Config::load(cli.archive_ext.as_deref())?;

    match cli.cmd {
        Some(Cmd::List { archive }) => run_list(&archive, &config)?,
        Some(Cmd::Get { zip_expr, out }) => run_get(&zip_expr, out.as_deref(), &config)?,
        Some(Cmd::Delete { zip_expr }) => run_delete(&zip_expr, &config)?,
        Some(Cmd::Replace { zip_expr, source }) => run_replace(&zip_expr, &source, &config)?,
        Some(Cmd::Diff { left, right }) => run_diff(&left, &right, &config)?,
        Some(Cmd::Patch { cmd }) => run_patch(cmd, &config)?,
        Some(Cmd::Version) => run_version(),
        None => {
            if let Some(archive) = cli.archive {
                run_list(&archive, &config)?;
            }
        }
    }
    Ok(())
}

fn run_version() {
    println!("zipr {}", env!("CARGO_PKG_VERSION"));
    println!("revision {}", env!("ZIPR_GIT_REV"));
}

fn run_list(archive: &Path, config: &Config) -> Result<()> {
    let items = archive::list_recursive(archive, config)?;
    println!("{:>12} {:>12}  Name", "Length", "Compressed");
    for item in items {
        println!(
            "{:>12} {:>12}  {}",
            item.size, item.compressed_size, item.expr
        );
    }
    Ok(())
}

fn run_get(expr: &str, out: Option<&Path>, config: &Config) -> Result<()> {
    let parsed = parse_zip_expr(expr)?;
    let bytes = archive::get(&parsed, config)?;
    if let Some(path) = out {
        fs::write(path, &bytes)?;
    } else {
        use std::io::Write as _;
        std::io::stdout().write_all(&bytes)?;
    }
    Ok(())
}

fn run_delete(expr: &str, config: &Config) -> Result<()> {
    let parsed = parse_zip_expr(expr)?;
    archive::delete(&parsed, config)
}

fn run_replace(expr: &str, source: &Path, config: &Config) -> Result<()> {
    let parsed = parse_zip_expr(expr)?;
    let opts = ReplaceOptions {
        method: MethodPolicy::Inherit,
        level: LevelPolicy::Name(LevelName::Inherit),
        mtime: MtimePolicy::Source,
    };
    archive::replace(&parsed, source, config, opts)
}

fn run_diff(left: &Path, right: &Path, config: &Config) -> Result<()> {
    let items = archive::diff_recursive(left, right, config)?;
    if items.is_empty() {
        println!("No differences.");
        return Ok(());
    }

    let mut added = 0usize;
    let mut removed = 0usize;
    let mut modified = 0usize;

    for item in &items {
        let tag = match item.kind {
            DiffKind::Added => {
                added += 1;
                "A"
            }
            DiffKind::Removed => {
                removed += 1;
                "D"
            }
            DiffKind::Modified => {
                modified += 1;
                "M"
            }
        };
        println!("{tag} {}", item.path);
        if item.kind == DiffKind::Modified {
            if item.content_changed {
                println!("  content: changed");
            }
            for c in &item.metadata_changes {
                println!("  meta: {c}");
            }
        }
    }
    println!(
        "Summary: added={}, removed={}, modified={}",
        added, removed, modified
    );
    Ok(())
}

fn run_patch(cmd: PatchCmd, config: &Config) -> Result<()> {
    match cmd {
        PatchCmd::Draft {
            archive,
            from_dir,
            output,
        } => {
            let summary = archive::patch_draft(&archive, &from_dir, &output, config)?;
            println!(
                "draft written: {} (matched={}, unresolved={})",
                output.display(),
                summary.matched,
                summary.unresolved
            );
        }
        PatchCmd::Apply {
            archive,
            spec,
            dry_run,
        } => {
            let summary = archive::patch_apply(&archive, &spec, dry_run, config)?;
            println!(
                "patch {}: replaced={}, deleted={}",
                if dry_run { "dry-run" } else { "applied" },
                summary.replaced,
                summary.deleted
            );
        }
    }
    Ok(())
}
