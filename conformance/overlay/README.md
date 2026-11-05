# Overlay conformance cases

`cases.json` encodes the 42 cases of SPEC.md "Conformance Cases". Every SDK runner executes all of them.

- `base`: a POSIX shell script that builds the shared base tree in an empty directory (`umask 022`; `d` has mode 750). Each case gets a fresh copy and a new overlay on it.
- `cases[]`: `id` (the spec number), `steps`, optional `xfail`, optional `timeout` (seconds, default 30). A case that exceeds its timeout fails and its worker must be killed.
- A step is `[var = ]op args... [=> expected]`. A binding is recognized only when `var` is one identifier word. Arguments are space-separated. A step without `=>` must succeed.
- With `=>`, the result (or the POSIX error name, such as `ENOENT`) must equal `expected` exactly. `$var` in `expected` is a result bound by an earlier step. A leading `!` requires a successful step with a different result.
- Path ops follow symlinks as the SDK path API does: `readdir P` (names joined by spaces), `stat|lstat P [type|mode|size|ino]` (default `type`: `f d l ?`; `mode`: octal permission bits), `plus DIR NAME FIELD` (readdir-plus entry), `read P`, `write P S`, `readlink P`, `mkdir|rmdir|unlink P`, `chmod P MODE`, `symlink TARGET P`, `link S D`, `rename S D`, `open P` (the read-write handle) and `hread`, `hostRead P` (the base file), `reopen`.
- `diff` returns the text form, changes joined by `; `.
- Management steps: `createOverlay DB BASE`, `openMerged DB [BASE]`, `deleteOverlay DB`, `rebindOverlay DB BASE`, `listOverlays DIR [BASE]` (paths joined by spaces), `close`, `plainDb DB`, `legacyRust DB BASE`, `sql DB STATEMENT` (rows as `a|b; c|d`, `NULL` for NULL), `hostWrite|hostRm|hostMkdir P ...` (change the base).
- `db` is the case's overlay database, `base` its base, `other` a different base, `dir` an empty directory for other databases. `openMerged` makes its overlay current for later path steps.
- `legacyRust` copies `sdk/rust/tests/fixtures/legacy_rust_overlay.db` and sets its `base_path` to BASE.
- A runner answers an op it cannot serve with `ENOSYS`, so the step fails.
- `xfail`: the issues that must land before the case passes. List every issue whose bug the case exercises; `9` is the management API and `diff`.
- `xfail_ts`: like `xfail`, for the TypeScript runner only. The TypeScript runner reads only `xfail_ts` and the Rust runner reads only `xfail`, so a case that one SDK passes does not break the other.
- A failing case without `xfail` is an error. A passing case with `xfail` is an error: remove the marker when its last issue lands. The same holds for `xfail_ts`. A timeout, a signal, or a runner error fails the case even with a marker.
