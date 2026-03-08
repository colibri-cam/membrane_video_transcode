# Code Style and Testing Guidelines

This project contains Elixir module. It is a membrane framework sink that uses
Rust NIF to implement low level atomic modesetting implementation.

Check `native/drm_prime_sink/AGENTS.md` for the Rust instructions.

## Code Style
- Use elixir specified in .tool-versions
- Format code with `cargo fmt -all` before committing.
- Keep imports sorted and grouped.
- Use 2 spaces for indentation and try to keep lines under 100 characters.

## Testing
- Run `mix test` to ensure tests are passing.
