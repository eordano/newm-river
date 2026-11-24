# AGENTS.md — newm-river

Shared guide for coding agents and contributors; a local, untracked `CLAUDE.md` may include it.

Maintainer machine rules live outside this repository in `~/.config/newm-dev/AGENTS.md`; if that
file exists, read it before building or testing.

newm-river is river (a fork of codeberg river main, tracked as the `upstream` remote) plus a
Rust window manager in `newm/newm-river/` that brings newm's strict tile canvas to river.
`newm/README.md` describes the window manager, the river patch table and the tests.

## Layout

- The repository root is river (Zig). Changes to river's code are kept small and listed in the
  river patch table in `newm/README.md`; merge river `upstream/main` when it moves.
- `newm/` is a Cargo workspace: `newm-river` (the window manager), `newm-canvas` (the canvas
  model), `niri-config` and `niri-ipc` (the KDL configuration and IPC, from niri),
  `newm-river-lock` (the screen locker), `newm-kde` and `newm-synth`. `newm/compat` is the
  application compatibility VM suite, `newm/e2e` the headless and VM test scripts.

## Build and test

Build and test on a machine with room for a full Rust and Zig build, one build at a time,
inside the dev shell (`nix develop`). Never start the compositor on a real seat while testing:
the e2e scripts run it headless, and the VM tests run it in a NixOS VM.
- Unit gate, zero warnings and every test green:
  `cd newm && cargo fmt --check -p newm-river && cargo build --release -p newm-river --bins
  --examples && cargo clippy --workspace --all-targets -- -D warnings && cargo test -p
  newm-river && cargo clippy -p newm-river --all-targets --features telemetry -- -D warnings &&
  cargo test -p newm-river --features telemetry`. The usage log and its tests build only with
  the `telemetry` feature, which no default build has.
- The whole workspace's tests: `cargo test --workspace --exclude niri-config && cargo test -p
  niri-config --lib`.
- river: `zig build -Doptimize=ReleaseSafe -Dxwayland`.
- Headless e2e: `newm/e2e/headless.sh`, `session.sh`, `session-socket.sh` and `screencast.sh`
  (each takes `OUT=<dir>` for its results; see the script's header for its tools).
- VMs (KVM): `newm/e2e/vm.sh [vm-input|vm-session|vm-lock|vm-reattach|vm-browser]`; the
  compat suite: `nix build .#checks.x86_64-linux.compat-newm-river -L` (see
  `newm/compat/README.md`).
- A newm-canvas change keeps its Rocq proof's gate green (`newm/newm-canvas/proof/README.md`).
- Format newm-river with `cd newm && cargo fmt -p newm-river`; `cargo fmt --check` covers only
  that crate.
- Never use npm, npx or pip.

## Commits

- One coherent change per commit, so each one reverts alone.
- The subject names the area and what changed, `area: what changed` (for example
  `newm-river: Escape cancels a drag`); the body says why.
- No attribution trailers.
