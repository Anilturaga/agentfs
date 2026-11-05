# AGENTS.md: overlay work on the Anilturaga/agentfs fork

Branch `overlay/main` is the integration trunk for bringing overlay management to the TypeScript and Python SDKs. `main` mirrors upstream and is never a PR target.

## Hard rules
- Work only in `Anilturaga/agentfs`, and pass `--repo Anilturaga/agentfs` to `gh`. Never interact with `tursodatabase/agentfs` in any way.
- There is no GitHub Actions. CI runs on Warp: the agent environment runs the commands below, and the Gatekeeper agent re-runs them before merge.
- Every issue states its acceptance criteria, its verification commands, its LOC budget and its `Base:` branch. Keep to them.

## Keep the diff small
- Make the smallest correct change. Delete before you add.
- Add no new abstraction, option, file or dependency without at least two real uses or an explicit spec requirement.
- Make no drive-by refactors or reformatting, and don't touch files outside the issue's scope.
- Write tests that would catch a real regression. Don't write tests that only restate the implementation.
- Report `git diff --numstat` against the base, split into source and tests/fixtures, in every PR.

## Toolchains (already installed in the Warp environment)
- Rust: `nightly-2026-03-01`. The latest nightly breaks the `branches` crate, so don't change the toolchain.
- Node 24 + npm, Python ≥3.10 + uv, Go (apt), pkg-config, libssl, libunwind. There's no FUSE, so mount tests can't run here.

## Checks to run for each component you touch (run the full suite, not just new tests)
| Component | Commands |
|---|---|
| `sdk/rust` | `cargo fmt --check && cargo clippy -- -D warnings && cargo test` |
| `cli` | `cargo fmt --check && cargo clippy -- -D warnings && cargo test` |
| `sdk/typescript` | `npm ci && npm run build && npm test` |
| `sdk/python` | `uv sync && uv run ruff check . && uv run ruff format --check . && uv run pytest` |
| `sdk/go` | `go build ./... && go test ./...` |

## Overlay references
- The current overlay semantics are in `SPEC.md` (overlay section) and `sdk/rust/src/filesystem/overlayfs.rs`. The Rust implementation is the reference, but it has known bugs. Where they differ, the spec issue decides which is right.
- Known defects are tracked as issues on this fork. Before changing overlay behavior, check the epic.

## PRs
- Branch name: `<stack>/<issue#>-<slug>`. The PR targets the issue's `Base:`. Stacked PRs target the branch below them, and we use `gh stack` when it's available.
- The PR description includes `## Proof`: the Warp run link, the exact commands, and the result counts. Post full logs as a PR comment, never committed.
