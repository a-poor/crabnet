# Repository Guidelines

## Project Structure & Module Organization

Crabnet is a Rust 2024 embedded property-graph database backed by SQLite, using `open-cypher` for parsing.

- `src/lib.rs` defines the public API; `database.rs`, `value.rs`, and `error.rs` handle execution, typed values, and errors.
- `src/planner/` separates logical lowering, type checking, SQLite generation, physical planning, and explanations. `src/database/` contains traversal and comparison helpers.
- `tests/*.rs` holds integration tests; inline `#[cfg(test)]` modules cover internals. `tests/tck/` contains conformance helpers and outcome manifests.
- `benches/queries.rs` contains Criterion benchmarks. `docs/` records semantics, coverage, testing practices, and the roadmap.

## Build, Test, and Development Commands

This is a library; exercise it through tests and the README examples.

- `cargo build` — compile the library with bundled SQLite.
- `cargo test --test read_semantics` — run one integration suite; append `<test_name> -- --exact` for a single case.
- `cargo test --all-targets` — run unit/integration tests and benchmark smoke checks.
- `cargo test --doc` — verify documentation examples.
- `cargo fmt --all -- --check` — check formatting; use `cargo fmt --all` to apply it.
- `cargo clippy --all-targets -- -D warnings` — reject lint warnings.
- `cargo bench --bench queries` — measure planning, reads, traversal, and writes.
- `git diff --check` — catch whitespace errors before handoff.

## Coding Style & Naming Conventions

Follow rustfmt defaults: four-space indentation, `snake_case` modules/functions, `PascalCase` types, and `SCREAMING_SNAKE_CASE` constants. Document public APIs with `///`. Keep planner stages separate and preserve parameterized SQL and structured errors.

## Testing Guidelines

Use Rust's built-in test framework and descriptive behavior names. Follow `docs/testing.md`: demonstrate a focused failure, implement the behavior, then run adjacent suites and full checks. Cover nulls, types, multiplicity, parameters, and rollback for writes. Unordered results are bags: sorting is acceptable; deduplicating is not.

No numeric coverage threshold is configured. Maintain behavioral coverage in `docs/coverage.md`; avoid ignored speculative tests. The TCK requires `tests/tck/vendor/openCypher-2024.3/tck`; follow `tests/tck/README.md` and review every allowlist/exclusion update.

## Commit & Pull Request Guidelines

History uses short descriptive subjects, such as `Adds labels and xor`; recent work also uses `fix:`, `perf:`, and `bench:` prefixes. Keep commits focused. PRs should explain the behavior change, include validation commands/results, link relevant issues, and update semantics or coverage documentation when contracts change.
