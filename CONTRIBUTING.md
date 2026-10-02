# Contributing to ActiveGit

Thanks for your interest in improving ActiveGit! Bug reports, feature ideas,
docs fixes, and pull requests are all welcome.

## Reporting bugs and requesting features

Open an [issue](https://github.com/DevarshVasani/activegit/issues). For bugs,
please include:

- your OS, terminal, and `activegit --version`
- what you did, what you expected, and what happened
- a screenshot or recording for rendering problems

For larger features, open an issue first so we can agree on the approach
before you spend time on code.

## Project layout

| Crate           | Purpose                                                                       |
| --------------- | ----------------------------------------------------------------------------- |
| `git-tui-core`  | UI-independent git logic (libgit2, sync, LLM commit messages, job queue)      |
| `git-tui`       | The `activegit` binary: ratatui UI, key handling, config, session, themes      |

Keep git and LLM logic in `git-tui-core` and rendering/input in `git-tui`, so
the core stays testable without a terminal.

## Development setup

You need Rust 1.88+ and a `git` CLI on your `PATH`.

```sh
git clone https://github.com/DevarshVasani/activegit && cd activegit
cargo run -p activegit -- .    # run against this repo
```

Before opening a pull request, make sure the same checks CI runs pass:

```sh
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

## Making changes

1. Fork the repo and branch from `master` (`feat/fuzzy-preview`, `fix/commit-cursor`, ...).
2. Keep each pull request focused on one change.
3. Add or update tests for behavior changes. Core git operations should be
   tested against a temporary repo (see `git-tui-core/src/testutil.rs`).
4. Update the README if you change keys, flags, or configuration.
5. Never let the UI thread block on git or network work; use the job queue.

## Commit messages

We use [Conventional Commits](https://www.conventionalcommits.org/):

```text
<type>(<scope>): <summary in the imperative, lowercase>
```

Types: `feat`, `fix`, `refactor`, `docs`, `test`, `chore`, `build`, `ci`, `perf`.
Scopes are usually a crate or area, e.g. `feat(diff): add binary file detection`.

## Pull requests

- Describe what changed and why; link related issues.
- Include screenshots for visible UI changes.
- Make sure CI is green. A maintainer will review and may ask for changes.

## License

By contributing, you agree that your contributions are licensed under the
[MIT License](LICENSE).
