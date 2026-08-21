# Code Style and Testing Guidelines

This Rust NIF exposes hardware-accelerated H.265 decoding through FFmpeg and canonical
VideoInterop DMA-BUF output.

## Code Style

- Use Rust 2024 edition conventions.
- Run `cargo fmt --all` before committing.
- Keep imports sorted and grouped as produced by rustfmt.
- Propagate errors with `?`; avoid unwraps in non-test code.
- Keep all ownership transfer and native release paths explicit and idempotent.

## Testing

- Run `cargo test`.
- Run `cargo clippy --all-targets --all-features -- -D warnings`.
- Run `cargo build --release`.
