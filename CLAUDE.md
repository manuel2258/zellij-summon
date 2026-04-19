# CLAUDE.md

## Commit Message Format

Every commit to this repository triggers an automated release via `auto-release.yml`.
The commit type determines the semver version bump, so using the correct prefix matters.

| Prefix | Semver bump | When to use |
|--------|-------------|-------------|
| `feat:` | minor (0.**X**.0) | New user-visible capability |
| `fix:` | patch (0.0.**X**) | Bug fix |
| `chore:` | patch | Tooling, deps, CI, refactor with no behaviour change |
| `test:` | patch | Adding or fixing tests only |
| `docs:` | patch | Documentation only |
| `feat!:` or body line `BREAKING CHANGE: ...` | major (**X**.0.0) | Incompatible API or behaviour change |

**Never use a bare or non-conventional commit message** (e.g. `update stuff`) —
it will still trigger a patch release but makes the changelog unreadable.

Keep the subject line under 72 characters. No trailing period.

## Before Committing

CI enforces formatting and linting. Run these two commands and fix any issues before committing:

```bash
cargo fmt
cargo clippy --target wasm32-wasip1 -- -D warnings
```

`cargo fmt` rewrites files in place. `cargo clippy` must exit with no warnings (they are treated as errors). Do not commit code that fails either check.
