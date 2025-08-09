# Code Style and Testing Guidelines

This directory contains a small Rust example for atomic modesetting using the `drm` crate.
Follow these guidelines when modifying any files under this directory.

## Code Style
- Use Rust 2021 edition conventions.
- Format the code with `cargo fmt --all` before committing.
- Keep imports sorted and grouped as produced by `rustfmt`.
- Use 4 spaces for indentation and try to keep lines under 100 characters.
- Propagate errors with `?` and avoid unwraps in non-test code.
- Prefer constructs that keep the binary small: avoid unnecessary allocations or
  dependencies.

## Testing
- Run `cargo clippy --all-targets --all-features -- -D warnings` and ensure it
  passes.
- Run `cargo build --release` (or `cargo test` once tests are added) so the
  size-optimized release profile builds successfully.

## Notes
- Optional feature `verbose` enables additional logging for debugging. Keep it
  off in normal builds unless logs are required.
- The release profile in `Cargo.toml` is tuned for minimal binaries; use
  `--release` for production builds.
