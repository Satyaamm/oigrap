# Contributing to oigrap

Thank you for your interest in contributing. This document covers everything you need to get started.

## Prerequisites

- Rust 1.78 or later (`rustup update stable`)
- `cargo` (included with Rust)
- A PostgreSQL-compatible client (psql, psycopg2, etc.) for integration testing

## Getting started

```bash
git clone https://github.com/Satyaamm/oigrap.git
cd oigrap
cargo build
cargo test
```

To run oigrap locally:

```bash
cargo run --bin oigrap -- 0.0.0.0:7432
psql -h localhost -p 7432 -U postgres
```

## Branch model

```
main        production — always releasable, protected
  └── develop     integration — all work merges here first, protected
        └── feature/short-description
        └── fix/short-description
        └── docs/short-description
        └── chore/short-description
```

- Branch off `develop`, not `main`.
- Keep branches short-lived. One logical change per branch.
- Delete your branch after the PR is merged.

## Pull request process

1. Open your PR against `develop`.
2. Fill in the PR description: what changed and why. Link a related issue if one exists.
3. One maintainer approval is required before merge.
4. Stale reviews are dismissed if you push new commits — re-approval is needed.
5. The maintainer merges `develop` → `main` when a batch of changes is ready to ship.

If you are proposing a significant change (new engine, protocol change, storage format change), open an issue first to discuss the approach before writing code.

## Code standards

All of the following must pass before a PR can be merged:

```bash
cargo test                        # all 207+ tests must pass
cargo clippy -- -D warnings       # zero warnings
```

Additional rules:

- No `unsafe` code in `crates/storage`. Correctness over performance in the storage layer.
- No external database crates. oigrap has no dependency on existing database engines.
- Do not break the PostgreSQL wire protocol compatibility. Existing clients must continue to work.
- New public functions in library crates should have a doc comment explaining what they do and any invariants the caller must uphold.

## Commit message format

Use [Conventional Commits](https://www.conventionalcommits.org/):

```
<type>: <short imperative subject>

[optional body — explain why, not what]
```

Types:

| Type | When to use |
|---|---|
| `feat` | New user-visible capability |
| `fix` | Bug fix |
| `perf` | Performance improvement |
| `refactor` | Internal restructuring, no behaviour change |
| `test` | Adding or fixing tests |
| `docs` | Documentation only |
| `chore` | Build, CI, dependency, or tooling change |

Subject line: imperative mood, lowercase, no trailing period, under 72 characters.

Examples:

```
feat: add FULL OUTER JOIN to executor
fix: correct HeapEntry ordering in HNSW beam search
perf: short-circuit zone map scan when predicate matches all values
docs: document spill wire format byte layout
chore: pin rust toolchain to 1.78 in Dockerfile
```

## Testing

Unit tests live alongside source files in the same module:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_my_change() { ... }
}
```

Integration tests go in `crates/<name>/tests/`.

To run the full test suite including end-to-end scripts:

```bash
cargo test
./scripts/run_all_tests.sh     # requires oigrap to be built and port 7432 free
```

Write tests for any new behaviour. Bug fixes should include a regression test.

## Filing issues

**Bug reports** — include:
- Steps to reproduce (minimal SQL or code that triggers the bug)
- Expected behaviour
- Actual behaviour (error message, wrong output, panic)
- oigrap version or commit hash (`git rev-parse --short HEAD`)
- OS and Rust version

**Feature requests** — include:
- The problem you are trying to solve
- Your proposed API or behaviour (example SQL or function signature)
- Any alternatives you considered

## Areas that welcome contributions

- SQL functions: additional aggregate functions, string functions, date/time functions
- Column encoding: bit-packing, ZSTD compression, run-length encoding improvements
- Query optimizer: cost model improvements, statistics collection, index selectivity
- Client compatibility: testing and fixing compatibility with specific drivers or ORMs
- Documentation: corrections, examples, tutorials
- Test coverage: edge cases in transaction isolation, large-scale load tests

## Code of conduct

oigrap is a professional project. Contributors are expected to communicate respectfully and constructively in all project spaces — issues, pull requests, and discussions. Disagreements about technical direction are welcome; personal attacks are not. Maintainers reserve the right to remove content or restrict participation for behaviour that falls below this standard.
