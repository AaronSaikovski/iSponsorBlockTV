# isponsorblocktvrust Repository Guidelines

## Project Overview

This directory is the standalone native Rust implementation of iSponsorBlockTV:
a self-hosted companion for YouTube TV that skips SponsorBlock segments, controls
ads/autoplay, and provides terminal device setup. It does not invoke Python.
Parent Python tooling and its lack of automated tests do not apply here.

The executable is `isponsorblocktvrust` (`isponsorblocktvrust.exe` on Windows);
the Cargo package is `isponsorblocktvrust`, and the library import remains
`isponsorblocktv`. Preserve existing config
locations, environment variable names, Lounge `join_name`, repository URLs, and
Docker image/service names when changing executable references.

## Architecture & Data Flow

- `main.rs` parses Clap arguments and captures signal cancellation, then loads one
  configuration snapshot through `PreparedApp::load`. It initializes tracing from
  the effective log level before calling `lib.rs::run_app(PreparedApp)`.
  `AppContext` injects environment, endpoints, shutdown, and optional prompt I/O;
  reusable application code does not install a global subscriber.
- `config.rs` separates lossless editable `ConfigDocument` from validated
  `RuntimeConfig`. Setup edits drafts and saves explicitly through `ConfigStore`;
  runtime validation must not rewrite configuration. Saves use a same-directory
  temporary file and atomic replacement, preserving existing permissions.
- `runtime/mod.rs` constructs transports and supervises one actor per device.
  Each actor owns playback/protocol state; shared API services provide cached
  metadata and SponsorBlock segments. Lounge events update snapshots, the pure
  scheduler computes seek deadlines, and serialized commands control the TV.
- Setup TUI follows events → reducer → effects. Screen/request generations reject
  stale completions. Discovery coordinates bounded DIAL/Cast probes separately
  from ordinary cloud-mediated playback control.

For protocol, platform, logging, or ownership changes, consult
[`ARCHITECTURE.md`](ARCHITECTURE.md) for native Rust invariants and verification boundaries.

## Key Directories

- `src/runtime/`: device actors, supervision, playback policy, pure scheduling.
- `src/lounge/`: session commands, typed events, incremental BrowserChannel framing.
- `src/api/`: SponsorBlock/YouTube APIs and bounded caches.
- `src/discovery/`: DIAL/SSDP and Cast/mDNS discovery and pairing.
- `src/setup/`: prompt setup, TUI state/effects, rendering and terminal lifecycle.
- `tests/`: integration contracts; `tests/fixtures/` supplies loopback services.
- `scripts/`: cross-toolchain preparation; `examples/`: executable native scenario.

## Development Commands

Run from this directory using the pinned toolchain:

```sh
cargo build --locked --release
cargo test --locked
cargo test --locked --test runtime_contract
cargo fmt --all -- --check
cargo clippy --locked --all-targets
cargo run --locked -- --data ./data setup
cargo run --locked -- --data ./data setup-cli
cargo run --locked -- --data ./data --debug start
```

Omitting the subcommand defaults to `start`. `setup` requires an interactive terminal;
use `setup-cli` for piped input. For verbosity and safe config-editing procedures,
see `README.md#configuration` and `GETTING_STARTED.md#configure-log-verbosity`.
Use a dedicated data directory when testing setup or migrating Python config.

Build the source container **from the parent repository root**:

```sh
docker build -f rust/Dockerfile -t isponsorblocktv-rust .
```

From this directory, `ISPBTV_RUST_IMAGE=isponsorblocktv-rust docker compose up -d`
selects that local image; otherwise Compose selects the published image reference.

## Code Conventions & Common Patterns

- Use rustfmt, snake_case functions/modules, PascalCase types, typed `thiserror`
  errors, and `tracing`. Preserve upstream wire field names and redact credentials,
  URLs and payloads in errors/logs; use the centralized `http.rs` transports.
- Logging accepts optional top-level `log_level`: lowercase `off`, `error`,
  `warn`, `info`, `debug`, or `trace`, defaulting to `info` when absent. Reject
  invalid types/values with redacted field-path errors. `--debug` overrides the
  configured level for one run. Keep third-party logs disabled using
  `off,isponsorblocktv=LEVEL`; HTTP traces require `--http-tracing` plus an
  effective `debug`/`trace` level.
- Attribute runtime logs by device number in configuration order. Log native ad
  detection once per observed episode and log its observed end; repeated ad
  events must not flood notifications. Describe accepted seeks as request
  acceptance, not proof of completed TV action.
- Keep async work cancellation-aware with `CancellationToken`, tracked tasks,
  bounded channels, and explicit shutdown. Shared services use `Arc` and bounded
  mutex-protected caches; release cache locks before network awaits.
  Same-video segment lookups share in-flight work; cancelling one consumer must
  not cancel another, and dropping all consumers must release the HTTP request.
  Setup reuses API clients only while credential and proxy options match.
- Keep playback mutation inside its device actor. Validate connection/video
  identity and revisions before applying async completions. Keep scheduler logic
  pure with explicit time inputs, rather than performing network effects there.
- Serialize Lounge mutations; never blindly replay ambiguous failed commands.
  Failed seeks retain suppression until playback observation resolves it.
  Restore volume only when the actor owns the mute.
- Preserve unknown JSON fields, numeric precision, and missing/null/empty
  distinctions. Do not save by round-tripping through `RuntimeConfig`.
  Persisted device offsets are milliseconds; scheduling uses seconds.
- BrowserChannel frame lengths count UTF-16 units, not UTF-8 bytes. Keep framing
  errors distinct from malformed individual event payloads.

## Important Files

| File | Change here for |
| --- | --- |
| `src/main.rs`, `src/cli.rs`, `src/lib.rs` | Entry point, commands, application composition |
| `src/config.rs` | Data-directory resolution, lossless persistence, validation |
| `src/http.rs` | Client policies, deadlines, proxy opt-in, redacted traces |
| `src/runtime/device.rs`, `src/runtime/scheduler.rs` | Playback state, commands, timing |
| `src/setup/state.rs`, `src/setup/tui.rs` | Reducer/effects, stale results, terminal cleanup |
| `Cargo.toml`, `Cargo.lock`, `rust-toolchain.toml` | Dependencies and compiler pin |
| `Cross.toml`, `scripts/prepare-cross.sh` | Cross-build template and preparation |
| `Dockerfile`, `Dockerfile.artifacts`, `compose.yml` | Native deployment |

## Runtime/Tooling Preferences

- Rust **1.99.1**, edition **2024**, Cargo; rustfmt and Clippy are toolchain components.
  Retain `Cargo.lock` and use `--locked` for reproducible dependency resolution.
  Runtime uses Tokio, Reqwest/native TLS, and Ratatui/Crossterm—not Python/Textual.
- On macOS with both Homebrew Rust and rustup, put rustup's proxies first:
  `export PATH="$HOME/.cargo/bin:$PATH"`. `rustup run 1.99.1 cargo clippy` alone
  did not prevent Homebrew Clippy selection in this environment. Mixing compiler
  distributions can cause E0514 even when version and commit strings match.
  Keep their build caches separate rather than deleting the shared cache.
  Verified commands using the rustup toolchain:

  ```sh
  export PATH="$HOME/.cargo/bin:$PATH"
  cargo clippy --locked --all-targets --target-dir target/rustup-native
  cargo test --locked --target-dir target/rustup-native
  ```

- Linux TLS builds vendor OpenSSL; native build tools include a C toolchain, make,
  and Perl. Vendoring alone does not imply static linkage or bundled trust roots.
- `--data` is a directory containing `config.json`, not a filename. Resolution:
  CLI → `iSPBTV_data_dir` → platform default. Nonempty `iSPBTV_docker` requires
  an existing directory. Containers persist `/app/data`; Compose host storage is
  configurable through `ISPBTV_RUST_DATA_DIR` (default `./data`).
- `Cross.toml` is a substitution template. Run
  `bash scripts/prepare-cross.sh <target-triple>` and use its generated
  `CROSS_CONFIG` exports only after success.
  It installs pinned cross-rs and builds local images; requires Git, rustup,
  network access, and Docker/Buildx with the `docker` driver and linux/amd64 support.
  Keep its generated directory and local images available for subsequent builds.
- Source `Dockerfile` uses the parent-root context; `Dockerfile.artifacts` uses
  this directory and requires prebuilt `dist/` binaries, CA bundle, and license.
  Both distribution paths must include `THIRD_PARTY_NOTICES.md`; retain its source
  attribution notices when changing adapted discovery/protocol code.
- Active discovery can launch YouTube on real devices; LAN multicast is needed
  for discovery, not normal Lounge control. Use one controller per device.

## Testing & QA

- Before delivering Rust code changes, run Clippy across all targets and the test
  suite using one consistent toolchain. Report actual results and any unverified
  integration behavior; do not weaken contracts to make checks pass.

- Rust's built-in harness and `#[tokio::test]` cover configuration, CLI/setup/TUI,
  APIs, Lounge, discovery/Cast, and runtime behavior. There is no configured numeric
  coverage threshold. `../.github/workflows/rust-ci.yml` runs native checks on
  Linux, Windows and macOS plus a container smoke test.
- `../.github/workflows/rust-release.yml` gates native release assets and GHCR
  images on CI and exact tag/crate-version agreement. See `README.md` for release
  platforms and publishing steps; Python release automation remains separate.
- Focus regressions with `cargo test --locked --test <contract> <test_name>`.
  `cargo test --locked --test native_e2e` and
  `cargo run --locked --example native_scenario` run the same synthetic application
  scenario, not live-TV certification. Tests need local sockets and child processes.
- Reuse temporary config directories, injected `Environment`/`Endpoints`, loopback
  ephemeral ports, and synthetic identities. Avoid process-global environment
  mutation in parallel tests; use child-process environments where needed.
- Use explicit synchronization for races and cancel/join fixture tasks. Resume
  Tokio time before TCP requests: paused-time auto-advance can race HTTP deadlines.
- Prioritize lossless/atomic saves, stale results, cancellation, no mutation replay,
  cache expiry, protocol framing, and discovery side effects. TUI rendering tests
  use Ratatui `TestBackend`; terminal lifecycle changes also need an actual terminal.
  Report physical-device, live-API and LAN-discovery checks separately from fixtures.
