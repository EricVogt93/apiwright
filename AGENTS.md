# Repository Guidelines

## Project Structure & Module Organization

ApiWright is a Rust 2021 Cargo workspace. Shared application and domain logic belongs in `crates/forge-core`; `crates/forge-cli` (`apiwright`, including MCP) and `crates/forge-gui` (`apiwright-ide`) remain thin adapters. Integration tests live in each crate's `tests/` directory; core fixtures are in `crates/forge-core/tests/fixtures/`. Documentation is in `docs/` and `docs/wiki/`, schemas in `schemas/`, demo projects in `examples/demo-workspace`, and GUI assets in `crates/forge-gui/assets`. The Codex plugin lives in `plugins/apiwright`, with marketplace metadata in `.agents/plugins/`.

## Build, Test, and Development Commands

- `cargo build --workspace` builds all crates in debug mode.
- `cargo build --release --locked --workspace` creates `target/release/apiwright` and `target/release/apiwright-ide`.
- `cargo run -p forge-gui --bin apiwright-ide` starts the desktop IDE.
- `cargo test --workspace --locked` runs workspace tests.
- `cargo fmt --all -- --check` verifies formatting.
- `cargo clippy --workspace --all-targets -- -D warnings` treats lint warnings as failures.
- `cargo check --release --locked -p forge-gui --bin apiwright-ide` checks release-only paths in CI.

Run the request-v1 demo offline with its reviewed project scripts:

```sh
cargo run -p forge-cli -- ci requests --root examples/demo-workspace --env demo --mock --allow-project-code
```

Linux GUI builds require the windowing packages listed in `README.md`.

## Coding Style & Naming Conventions

Use `rustfmt` defaults and four-space indentation. Follow Rust conventions: `snake_case` for modules, functions, and files; `CamelCase` for structs, enums, and traits; `SCREAMING_SNAKE_CASE` for constants. Prefer focused modules and existing workspace dependencies. Keep protocol, storage, and UI concerns outside domain models where practical.

## Testing Guidelines

Use `<feature>_test.rs`, `#[tokio::test]` for async paths, local servers such as `wiremock`, and `tempfile` for persistence. Run focused tests first, e.g. `cargo test -p forge-core --test reqv1_test`, then workspace checks. No coverage threshold is configured. Plugin changes also require `python3 -m unittest discover -s plugins/apiwright/scripts -p 'test_*.py'`; CI checks that plugin and workspace versions match.

## Format Compatibility & Configuration

For persisted-format changes, update models, matching schemas, `docs/architecture/request-format-v1.md`, and compatibility fixtures together. See `docs/wiki/mcp.md` for MCP trust and revision rules. Never commit `.forge-local/`, `.forge/`, `.env.local`, or `*.secrets.json`.

## Commit & Pull Request Guidelines

Follow recent history: concise, imperative messages using `fix(release): …`, `feat: …`, or `refactor: …`. Keep commits narrowly scoped. PRs should explain behavior changes, list executed checks, link relevant issues, and include screenshots for visible GUI changes.
