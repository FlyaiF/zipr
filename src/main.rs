mod archive;
mod cli;
mod config;
mod patch_spec;
mod path_expr;

use anyhow::Result;

fn main() -> Result<()> {
    cli::run()
}
