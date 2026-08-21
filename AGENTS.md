# Code Style and Testing Guidelines

This project contains Membrane video decoding, encoding, and transcoding elements backed by Rust
NIFs. Display presentation and DRM/KMS sinks are outside this repository.

Check `native/h265_decoder/AGENTS.md` for Rust instructions.

## Code Style

- Use the Elixir version specified in `.tool-versions`.
- Run `mix format` before committing.
- Keep imports sorted and grouped.
- Use two spaces for Elixir indentation.

## Testing

- Run `mix test`.
- Run the Rust checks listed in the native crate instructions.
