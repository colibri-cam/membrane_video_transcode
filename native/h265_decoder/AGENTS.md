# Code Style and Testing Guidelines

This directory contains a Rust NIF library that exposes hardware-accelerated H.265 decoding using `ffmpeg` and VAAPI on AMD GPUs.

## Code Style
- Use Rust 2024 edition conventions.
- Format the code with `cargo fmt --all` before committing.
- Keep imports sorted and grouped as produced by `rustfmt`.
- Use 4 spaces for indentation and keep lines under 100 characters.
- Propagate errors with `?` and avoid unwraps in non-test code.

## Testing
- Run `cargo clippy --all-targets --all-features -- -D warnings`.
- Run `cargo build --release` so the library builds successfully.
