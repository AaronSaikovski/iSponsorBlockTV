# iSponsorBlockTV — Native Rust

A standalone, self-hosted companion for YouTube TV apps. It uses SponsorBlock to
skip sponsored segments and supports ad muting/skipping, autoplay control, and
terminal-based device configuration. No Python runtime is required.

This directory contains the native Rust implementation. The [parent README](../README.md)
primarily documents the Python implementation; its tooling and compatibility claims
should not be treated as native Rust certification.

## Requirements

- Rust **1.99.1** and Cargo, selected by [rust-toolchain.toml](rust-toolchain.toml).
- Native build tools; Linux builds use vendored OpenSSL and need a C toolchain,
  make, and Perl.
- A YouTube TV device to pair with and internet access to YouTube and SponsorBlock.
- An interactive terminal for the full-screen setup interface.

Automatic discovery requires local-network access. Ordinary playback control uses
YouTube Lounge and does not require the service to share the device's LAN.
Active discovery can launch YouTube on nearby devices.

## Quick start

New device? Follow the [getting-started and pairing guide](GETTING_STARTED.md)
for TV-code pairing, setup prompts, saving configuration, and first playback.

Run these commands from this directory (`rust/`):

```sh
cargo build --locked --release
./target/release/iSponsorBlockTV --data ./data setup
./target/release/iSponsorBlockTV --data ./data start
```

On Windows, the executable has an `.exe` suffix. Use `setup-cli` instead of `setup`
for terminal prompts or piped input:

```sh
cargo run --locked -- --data ./data setup-cli
```

Save configuration explicitly during setup before starting the service. Keep the
service running while watching TV, and use only one controller per device.
Omitting the subcommand defaults to `start`.

### Configuration

`--data` selects a **directory containing `config.json`**, not a filename. The
selection order is:

1. `--data <directory>`
2. `iSPBTV_data_dir`
3. The platform's default application-data directory

Generate configuration through setup. When trying the Rust implementation with
existing Python configuration, use a separate copy first. Setup preserves unknown
JSON fields; startup separately validates the configuration. A nonempty channel
whitelist requires a YouTube API key. Device offsets in JSON are milliseconds.

Treat configuration as sensitive: it contains device identifiers and may contain
API credentials. Do not commit your data directory.

### Diagnostics

```sh
cargo run --locked -- --help
cargo run --locked -- --data ./data --debug start
cargo run --locked -- --data ./data --debug --http-tracing start
```

Logs go to stderr. HTTP tracing requires debug logging for visibility and uses
redacted transport records. Review logs before sharing them.

## Docker

Build the native source image **from the parent repository root**, not this directory:

```sh
docker build -f rust/Dockerfile -t isponsorblocktv-rust .
```

Then, from `rust/`, create persistent storage and configure the service interactively:

```sh
mkdir -p data
docker run --rm -it -v "$PWD/data:/app/data" isponsorblocktv-rust setup-cli
ISPBTV_RUST_IMAGE=isponsorblocktv-rust docker compose up -d
```

The [Compose file](compose.yml) mounts `./data` at `/app/data`. Set
`ISPBTV_RUST_DATA_DIR` to change the host directory; use that same directory for the
setup container's bind mount. Without `ISPBTV_RUST_IMAGE`, Compose selects its
configured published image reference rather than your local build.

The image contains a stripped, static musl executable and CA certificates, with
no shell. Symbol stripping reduces image size without changing runtime settings;
symbolized debugging requires an unstripped build. Cargo artifacts are cached per
architecture on the same BuildKit builder, keeping compiler outputs out of the
final image. Fresh hosted runners still need to populate these cache mounts.
The supplied Compose configuration does not enable host networking; automatic LAN
discovery may require deployment-specific multicast/network configuration.

[Dockerfile.artifacts](Dockerfile.artifacts) is a separate packaging path: it uses
this directory as its build context and expects prebuilt binaries, a CA bundle,
and a license under `dist/`. It does not compile the application.

## Development

```sh
cargo build --locked
cargo fmt --all -- --check
cargo clippy --locked --all-targets
cargo test --locked
```

Run a focused contract or the synthetic end-to-end scenario:

```sh
cargo test --locked --test runtime_contract
cargo test --locked --test native_e2e
cargo run --locked --example native_scenario
```

The last two commands exercise the same fixture-backed application scenario.
Tests use Rust's built-in harness, Tokio, local HTTP/UDP/TLS services, temporary
configuration directories, and Ratatui's test backend. They need local sockets
and child-process support, but not paired TVs or production credentials.

These tests do not certify physical devices, live API behavior, LAN multicast,
or real terminal lifecycle behavior. Verify those separately when changing the
corresponding integration. There is no configured numerical coverage threshold.

### Architecture

`main.rs` handles arguments, logging, and signals; `lib.rs` composes injected
configuration, endpoints, prompt I/O, and cancellation. Configuration has separate
lossless editable and validated runtime representations.

At runtime, one actor owns each device's playback/protocol state. Lounge events
update playback snapshots; shared API services fetch/cache segments; a pure
scheduler computes skip timing; serialized Lounge commands control the device.
The TUI uses an event/reducer/effect model with stale-result guards.

| Location | Responsibility |
| --- | --- |
| `src/config.rs` | Configuration validation and atomic persistence |
| `src/http.rs` | Transport policies, deadlines, proxies, redacted tracing |
| `src/runtime/` | Device actors, supervision, playback scheduling |
| `src/lounge/` | YouTube Lounge sessions, events, streaming framing |
| `src/api/` | SponsorBlock and YouTube API clients/caches |
| `src/discovery/` | DIAL/SSDP and Cast/mDNS discovery/pairing |
| `src/setup/` | Prompt setup and Ratatui interface |
| `tests/` | Behavioral contracts and local fixtures |

See [repository guidelines](AGENTS.md) for implementation invariants and the
native Rust section of [the architecture document](../ARCHITECTURE.md) for details.

### Linux cross-compilation

[Cross.toml](Cross.toml) is a template, not a directly usable configuration.
The preparation script installs pinned cross-rs and builds local toolchain images.
It requires Git, the pinned Rust toolchain, Cargo's binary directory on `PATH`,
network access, and Docker/Buildx with the `docker` driver and `linux/amd64` support.

From this directory, in Bash:

```sh
if exports=$(bash scripts/prepare-cross.sh x86_64-unknown-linux-musl); then
    eval "$exports"
    cross build --locked --release --target x86_64-unknown-linux-musl
fi
```

Only evaluate exports after successful preparation. Preserve the generated
`CROSS_CONFIG` directory and local Docker images for subsequent builds.
Supported targets are x86_64, aarch64, and i686 Linux musl, plus
`armv7-unknown-linux-musleabihf` and `arm-unknown-linux-musleabihf`.

## GitHub Actions and releases

[Rust CI](../.github/workflows/rust-ci.yml) runs on pull requests and pushes to
`main` affecting Rust files or Rust workflows, and can be started manually.
It checks formatting on Linux, runs Clippy/tests and builds release executables on
Linux, Windows and macOS, and builds/smoke-tests the AMD64 Docker image.

[Release native Rust](../.github/workflows/rust-release.yml) runs when a GitHub
release with a `v`-prefixed tag is published. It verifies that the tag is exactly
`v` plus the version in `rust/Cargo.toml`, then requires Rust CI before publishing:

- Archives named `iSponsorBlockTV-rust-<version>-<target>.tar.gz` (Unix) or `.zip`
  (Windows), containing the executable and license, plus `SHA256SUMS-rust.txt`.
- Linux musl targets: x86_64, aarch64, i686, ARMv7 and ARMv6; macOS targets:
  aarch64 and x86_64; Windows target: x86_64 MSVC. Cross-built targets are not
  runtime certification for those devices.
- A multi-platform AMD64/ARM64 image at `ghcr.io/<owner>/<repository>-rust`,
  tagged `v<version>`. Stable releases also update `latest`; prereleases do not.

To release:

1. Update `rust/Cargo.toml` and its package entry in `rust/Cargo.lock` together.
   Keep the CLI version contract in `rust/tests/cli_contract.rs` synchronized.
2. Merge the changes and confirm Rust CI succeeds.
3. Tag that commit `v<version>`, push the tag, and publish its GitHub release
   (mark previews as prereleases). A tag push alone does not publish Rust assets.
4. Check the workflow run and release assets; retry failed jobs if necessary.

This shares the existing Python release event without replacing its automation.
Publishing a GitHub release also invokes the existing Python workflows; it is
not a Rust-only release operation. Rust assets and GHCR image names are separate.
The built-in `GITHUB_TOKEN` supplies release/package permissions; no Docker Hub
credentials are required. Repository policy must allow those permissions, and
the GHCR package must grant this repository access. Make the package public if
anonymous pulls are intended. Forks publish under their own owner/repository.

See [Getting started](GETTING_STARTED.md) for local and Docker deployment.

## License

GPL-3.0-only; see the [project license](../LICENSE.md).
