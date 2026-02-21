# Repository Guidelines

## Project Structure & Module Organization
This repository is a Rust binary crate (`edition = 2024`).

- `src/main.rs`: current application entry point.
- `Cargo.toml`: package metadata and dependencies.
- `target/`: Cargo build artifacts (generated, do not edit).

As the project grows, keep runtime code in `src/` modules (for example, `src/bot/`, `src/llm/`) and add integration tests under `tests/`.

## Build, Test, and Development Commands
Use Cargo for all local workflows:

- `cargo check`: verify code compiles.
- `cargo test`: run unit and integration tests.
- `cargo fmt --all`: format all Rust code using `rustfmt`.
- `cargo clippy --all-targets --all-features`: lint.
- `cargo clippy --all-targets --all-features --fix --allow-dirty`: fix warnings automatically (prefer this to manual fixes).

Run `cargo fmt` and `cargo clippy` after making changes.

## Coding Style & Naming Conventions
Follow standard Rust style (4-space indentation; no tabs).

- `snake_case`: functions, variables, modules, file names.
- `CamelCase`: structs, enums, traits.
- `SCREAMING_SNAKE_CASE`: constants and static values.

Prefer small modules, explicit types at public boundaries, and `Result`-based error handling over panics in non-fatal paths.

Use a hard cutover approach and never add backwards compatibility unless approved by user.

After making changes, look at the code again and make sure it doesn't have unnecessary fallbacks, unnecessary complexity, redundancies, and overall code quality isn't horrible.

## Testing Guidelines
No coverage gate is configured yet; treat tests as required for new behavior.

- Unit tests: colocate with code using `#[cfg(test)] mod tests`.
- Integration tests: place in `tests/` with behavior-oriented names (for example, `tests/message_flow.rs`).
- Note: this agent may be unable to run integration tests in this environment due to sandboxing/permission restrictions.

At minimum, add tests for parsing, branching logic, and error paths introduced by your change.
