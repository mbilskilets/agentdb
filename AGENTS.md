# agentdb

Rust library crate. Toolchain is pinned in `rust-toolchain.toml`.

## Before you say a change is done

Run `./check.sh`. It must exit 0. It runs the formatter check, Clippy with every warning as an error, the tests, and `cargo deny`.

- `cargo fmt` fixes formatting failures.
- `cargo clippy --fix --allow-dirty` applies the mechanical lint fixes. Fix the rest by hand.

## Lint rules

The rules live in `Cargo.toml` under `[lints]`, with thresholds in `clippy.toml`.

- Fix the code a lint points at. Do not weaken the config: no removing lints, raising thresholds, or adding `allow` entries to make a failure go away. If a lint is wrong for this project, tell the user and let them decide.
- `#[allow(...)]` is banned. When a single site is a real exception, use `#[expect(clippy::the_lint, reason = "why this site is safe")]` on the smallest item possible.
- `unsafe` is forbidden outright.

## Writing code that passes

- Return errors, don't crash. No `.unwrap()`, `.expect()`, `panic!`, `todo!`, `unreachable!`, `vec[i]`, or `&s[a..b]` outside tests. Use `?`, `.get(i)`, `.ok_or(...)`, and `match`.
- Keep the source error: `.map_err(|e| MyError::Read(e))`, never `.map_err(|_| ...)`. Never discard a `Result` with `let _ =` or `.ok();`.
- Borrow before you clone. Take `&str` and `&[T]` parameters instead of `String` and `Vec<T>` when the function only reads.
- Convert numbers with `From`/`TryFrom`, not `as`.
- Keep functions short and flat: at most 100 lines, 4 levels of nesting, 6 arguments. Split a function when Clippy reports its complexity.
- Tests go in a `#[cfg(test)] mod tests` block, also in files under `tests/`. Test functions return nothing and use `.unwrap()`, which is allowed there; a test that returns `Result` and asserts is rejected.

## Dependencies

Add a dependency only when the task needs it, with `cargo add <crate>`, and confirm the crate exists on crates.io first. `cargo deny check` rejects known-vulnerable or yanked versions, licenses outside the list in `deny.toml`, and anything not from crates.io.
