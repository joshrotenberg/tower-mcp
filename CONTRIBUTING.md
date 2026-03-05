# Contributing to tower-mcp

## Getting Started

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --lib --all-features
cargo test --test '*' --all-features
cargo test --doc --all-features
```

All of these must pass before submitting a PR.

## Submitting Changes

- **Open an issue first** for anything beyond a straightforward bug fix. This lets us discuss scope and approach before you invest the effort.
- **Keep PRs focused.** One logical change per PR. Don't bundle unrelated changes together.
- **Squash commits** into a clean history before opening the PR.
- Use [conventional commits](https://www.conventionalcommits.org/): `feat:`, `fix:`, `docs:`, `refactor:`, `test:`, etc.

## What Makes a Good PR

- Solves a specific problem or implements a discussed feature
- Includes tests for new or changed behavior
- Updates relevant doc comments and examples
- Passes CI

## AI-Assisted Contributions

AI tools are fine. You're still responsible for what you submit.
