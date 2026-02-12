# Repository Guidelines

## Project Structure & Module Organization
This repository is a small Rust crate with a single binary entrypoint.

- `Cargo.toml`: package metadata, edition, and dependencies.
- `src/main.rs`: application entrypoint (`fn main()`).
- `target/`: Cargo build artifacts (generated; do not edit or commit).

As the project grows, place reusable logic in `src/lib.rs` and feature modules in `src/<module>.rs`. Keep integration tests in `tests/` and test helpers near the code they exercise.

## Build, Test, and Development Commands
Use Cargo for all local workflows:

- `cargo check`: fast compile check without producing a binary.
- `cargo run`: build and run the app locally.
- `cargo test`: run unit and integration tests.
- `cargo fmt`: format code using `rustfmt`.
- `cargo clippy -- -D warnings`: lint and treat warnings as errors.
- `cargo build --release`: produce optimized binaries.

Run `cargo fmt` and `cargo clippy -- -D warnings` before opening a PR.

## Coding Style & Naming Conventions
Follow idiomatic Rust conventions and keep code `rustfmt`-clean.

- Indentation: 4 spaces (managed by `rustfmt`).
- Naming: `snake_case` for functions/modules/files, `CamelCase` for types/traits, `SCREAMING_SNAKE_CASE` for constants.
- Keep modules focused and prefer small, composable functions.
- Avoid `unwrap()` in production paths; return `Result` and propagate errors with `?`.

## Testing Guidelines
Prefer fast, deterministic tests.

- Unit tests: colocate in the same file under `#[cfg(test)] mod tests`.
- Integration tests: place in `tests/` with descriptive names (for example, `tests/cli_smoke.rs`).
- Name tests by behavior, e.g., `parses_empty_input`.

Run `cargo test` locally before pushing. Add tests for each bug fix and new behavior.

## Commit & Pull Request Guidelines
There is no existing commit history yet, so use a clear default convention:

- Commit messages: imperative, concise subject (for example, `Add CLI argument parsing`), optional body for context.
- Keep commits focused and logically grouped.
- PRs should include: purpose, key changes, test evidence (`cargo test`, `cargo clippy`), and linked issue(s) when applicable.

For user-facing behavior changes, include sample command output in the PR description.
