# Getting started with native Rust iSponsorBlockTV

Pair your TV's YouTube app, save its configuration, then leave the companion
running while you watch. Choose **local Rust** or **Docker** below; both use the
same pairing prompts and configuration format. Use only one controller per TV.

A YouTube API key is not needed for pairing or ordinary SponsorBlock skipping.
It is needed for the channel whitelist. Internet access to YouTube and
SponsorBlock is required; manual TV-code pairing does not require LAN discovery.

Before deployment, read the [dependency security warning](README.md#dependency-security).
It records an unresolved vendored OpenSSL advisory affecting Linux builds,
including Docker. Passing tests or rebuilding the same lockfile does not resolve it.

## Option A: Run locally

### Prerequisites

- Install Rust through rustup, including Cargo. The source checkout selects
  Rust **1.99.1** through `rust/rust-toolchain.toml`.
- Linux: install a C compiler, make and Perl for vendored OpenSSL builds, plus
  your distribution's CA certificate bundle (usually `ca-certificates`) for HTTPS.
- macOS: install Xcode Command Line Tools (`xcode-select --install`). If Homebrew
  Rust is also installed, use rustup first: `export PATH="$HOME/.cargo/bin:$PATH"`.
- Windows: install Visual Studio Build Tools with the C++ build tools and Windows
  SDK. Run Cargo from a developer terminal with the MSVC toolchain available.

Commands below assume your terminal is in the checkout's **`rust/` directory**.
Use a separate data directory or backup when trying existing Python configuration.

### Build and pair

```sh
cargo build --locked --release
cargo run --locked --release -- --data ./data setup-cli
```

Follow [Pair and save](#pair-and-save) below. The configuration will be written to
`rust/data/config.json` when you run from `rust/`.

For a full-screen terminal interface instead of prompts:

```sh
cargo run --locked --release -- --data ./data setup
```

Choose **Add device**, enter the **Twelve-digit TV code**, select
**Validate / retry**, then **Add paired device**. Configure preferences and save
explicitly. Opening Add device starts active LAN discovery, which can launch
YouTube on nearby devices even while the pairing-code tab is selected. Use
`setup-cli` to pair manually without this discovery flow.

### Start, inspect, and stop

```sh
cargo run --locked --release -- --data ./data start
```

Or run the already-built executable without Cargo:

```sh
./target/release/iSponsorBlockTV --data ./data start
```

In Windows PowerShell:

```powershell
.\target\release\iSponsorBlockTV.exe --data .\data start
```

The service runs in the foreground; leave it running while watching TV. Info-level
logs go to stderr by default and show startup/shutdown, device connections/retries,
and accepted SponsorBlock seek requests. Device numbers follow configuration order;
these messages omit screen IDs, video IDs, keys and URLs.
Add `--debug` before `start` for detailed diagnostics:

```sh
cargo run --locked --release -- --data ./data --debug start
```

For HTTP diagnostics, use `--debug --http-tracing start`. Both flags are needed
to display redacted HTTP records; review the output before sharing it.

Use **Ctrl-C** to stop. Setup does not start a background playback service, and
these commands do not install an operating-system startup service.

To update a source installation, stop the process, back up the data directory,
update your checkout to the desired release, rebuild with `--locked --release`,
and start with the same `--data` directory. Stop before re-running setup to avoid
editing configuration underneath a running controller; restart to load changes.

### Use a release binary instead

After a native release is published, download the matching
`iSponsorBlockTV-rust-<version>-<target>` archive and `SHA256SUMS-rust.txt` from
this repository's GitHub release. Match both OS and CPU architecture. Unix
archives are `.tar.gz`; Windows archives are `.zip`. They include the executable,
license and third-party notices; keep these together when redistributing.
No Python or Rust compiler is required to run them.

Verify the downloaded archive against its entry in the checksum file, then
extract it. Linux can use `sha256sum <archive>`; macOS can use
`shasum -a 256 <archive>`; PowerShell can use `Get-FileHash <archive> -Algorithm SHA256`.
Run the extracted executable with `--data ./data setup-cli`, then with
`--data ./data start`. Host TLS trust certificates are still required on Linux.
Availability depends on a successful release workflow; a configured workflow is
not evidence that a downloadable release already exists.

## Option B: Run in Docker

Install Docker Engine or Docker Desktop with Linux containers and Docker Compose.
The native image supports **Linux AMD64 and ARM64**. Docker commands below use a
POSIX shell; on Windows, use WSL or adapt bind-mount paths to your shell.

### Choose an image

**Build locally:** from the checkout's **parent repository root**, run:

```sh
docker build -f rust/Dockerfile -t isponsorblocktv-rust:local .
```

Then change into `rust/` and select it:

```sh
export ISPBTV_RUST_IMAGE=isponsorblocktv-rust:local
```

**Use a published image:** after a successful native release, select its version
tag. For example, if `v2.11.0` has been published by the upstream repository:

```sh
export ISPBTV_RUST_IMAGE=ghcr.io/dmunozv04/isponsorblocktv-rust:v2.11.0
docker pull "$ISPBTV_RUST_IMAGE"
```

For a fork, use `ghcr.io/<owner>/<repository>-rust:<tag>`. Stable releases also
publish `latest`; version tags make your deployment choice explicit. A private
GHCR package requires `docker login ghcr.io` with credentials that can read it.
The Rust workflow publishes to GHCR, not Docker Hub.

### Create persistent storage and pair

Run from **`rust/`**. Select one absolute host path and use it for both setup and
the running service:

```sh
export ISPBTV_RUST_DATA_DIR="$PWD/data"
mkdir -p "$ISPBTV_RUST_DATA_DIR"
docker run --rm -it \
  --mount "type=bind,src=$ISPBTV_RUST_DATA_DIR,dst=/app/data" \
  "$ISPBTV_RUST_IMAGE" setup-cli
```

Follow [Pair and save](#pair-and-save). The `-it` flags allow interactive input;
`--rm` removes the setup container afterwards, **not** the bind-mounted config.
The resulting host file is `$ISPBTV_RUST_DATA_DIR/config.json`.

The image has no shell: pass `setup-cli`, `setup`, or `start` directly after the
image name. Do not try `docker exec ... sh`. Manual pairing avoids multicast
requirements; automatic LAN discovery may require additional container network
configuration. The supplied Compose file does not enable host networking.

### Start, inspect, and stop

In the same shell, with both variables still exported:

```sh
docker compose up -d
docker compose ps
docker compose logs --tail=100 -f isponsorblocktv-rust
```

Ctrl-C stops following logs, **not** the container. The Compose service uses
`restart: unless-stopped`. To stop it or remove the container:

```sh
docker compose stop
docker compose down
```

Your bind-mounted data survives both commands. Keep a backup; do not delete the
host directory. Preserve `ISPBTV_RUST_IMAGE` and `ISPBTV_RUST_DATA_DIR` for future
Compose invocations, either by exporting them again or setting them in a local
`rust/.env` file. Use an absolute storage path there; keep `.env` and data private.

Compose reads `.env` itself; the direct `docker run` commands above still need
exported shell variables. Exported values override `.env`, so update or unset
them when changing the image or storage path.

To change settings, use Compose so the setup container receives the same image
and mount, whether they come from `.env` or your shell:

```sh
docker compose stop
docker compose run --rm --no-deps isponsorblocktv-rust setup-cli
docker compose up -d
```

Save explicitly before exiting setup. Read the prompt-default warning under
[Pair and save](#pair-and-save) before reconfiguring an existing installation.

### Debug in the foreground

Stop the background controller first to avoid controlling the same TV twice:

```sh
docker compose stop
docker compose run --rm --no-deps isponsorblocktv-rust --debug --http-tracing start
```

Here, Ctrl-C stops the one-off service, not just log following. Afterwards, use
`docker compose up -d` to resume normal background operation. Omit `--http-tracing`
when ordinary debug logs are sufficient; the image does not need a shell for either.

### Update the container

Back up the data directory. For a published image, set the desired new version
in `ISPBTV_RUST_IMAGE` (or `.env`), then run:

```sh
docker compose pull
docker compose up -d
docker compose logs --tail=100 isponsorblocktv-rust
```

For a local image, rebuild from the parent root, then run
`docker compose up -d --force-recreate` from `rust/`; do not pull a local-only tag.
If a new version fails, select the prior image and recreate the service using
compatible backed-up configuration as needed. Never run both versions against
the same TV at once.

## Pair and save

1. On the TV or streaming device, open **YouTube → Settings → Link with TV code**.
2. Leave the **12-digit TV code** visible. This is not an account sign-in or
   activation code. Spaces and hyphens are accepted by prompt setup.
3. Start `setup-cli` using your chosen local or Docker command above.
4. Follow the prompts below. Pressing Enter accepts the displayed default, which
   is **no** for both adding a device and saving.

| Prompt | First-device choice |
| --- | --- |
| Use system-wide proxy? | `n`, unless you need a configured proxy |
| Paired with 0 Device(s). Add more? | **`y`** |
| Enter pairing code | The TV's 12-digit code |
| Paired with 1 Device(s). Add more? | `n`, or `y` for another TV |
| Add an API key? | `n` unless using the channel whitelist |
| Enter skip categories | **`sponsor`** to skip sponsored segments |
| Whitelist channels? | `n` for basic setup |
| Specify a minimum segment length? | `n` to leave it unchanged |
| Report skipped segments? | Defaults to `y`; choose `n` to disable reporting segment UUIDs to SponsorBlock |
| Mute / skip native YouTube ads? | Choose each option; both prompts default to no |
| Enable autoplay? | Defaults to `y`; choose your preference |
| SponsorBlock API URL | Enter for the default service |
| Save this configuration? | **`y`** |

**Blank categories disable SponsorBlock skipping.** Enter `sponsor` if you want
sponsor segments skipped. Existing configurations may first ask whether to change
categories. Native YouTube ad controls are separate from SponsorBlock skipping.

Successful pairing prints `Paired device: ...`; continue until the final save.
Look for `Config saved to ...` or `Config unchanged at ...`. Pairing alone, EOF,
or interruption does not implicitly save the draft.

**When rerunning `setup-cli`, defaults are not your saved preferences.** Proxy,
reporting, ad controls, autoplay and SponsorBlock URL prompts use fixed defaults.
Pressing Enter through them and saving can reset earlier choices. Select each
desired value explicitly; blank at the URL prompt selects the default service,
not your previous custom URL.

`--data` takes a **directory**, not a JSON filename. Keep configuration private:
it contains device identifiers and may contain API keys. Do not commit it.

## Verify first playback

1. Stop other Python or Rust controllers for the device, then start your local
   service or Compose container. Check for a device connection in the logs;
   process startup or a running container alone does not prove a connection.
2. Play a video on the paired TV with a known SponsorBlock segment in a selected
   category. Matching community segment data must exist; not every video has it.
3. Watch the TV confirm the skip. An accepted-seek log means YouTube accepted the
   request, not proof that the TV completed it. If nothing happens, inspect logs
   and the settings below before changing pairing.

## Troubleshooting

| Symptom | Next step |
| --- | --- |
| Code rejected | Check the digits; reopen the TV-code screen or restart YouTube and retry |
| Pairing failed | Check internet/proxy connectivity; pairing needs no API key |
| No configured devices | Answer `y` to Add more, pair successfully, and confirm the final save |
| Config not found | Use the same data directory; local relative paths depend on your working directory |
| Container keeps restarting | Inspect `docker compose logs`; check the mount, saved devices, and config validation errors |
| Container runs but nothing connects | Open YouTube on the paired TV; inspect connection/retry logs and internet/proxy access |
| GHCR pull denied or tag missing | Confirm a successful release, correct owner/tag, and package visibility/login |
| Sponsor segments not skipped | Check categories, segment availability, minimum length, whitelist, and that the service is running |
| TUI refuses to start | Use an interactive terminal or switch to `setup-cli` |
| Discovery finds nothing | Pair manually; discovery needs LAN access and multicast support |
| E0514 compiler mismatch | Put rustup first on `PATH`; set `CARGO_TARGET_DIR=target/rustup-native` and rebuild; direct executables then live under that directory |
| Existing config fails validation | Back it up and correct it through setup; saved drafts can still be incomplete for startup |
| Malformed JSON cannot open in setup | Restore a valid backup; setup deliberately refuses to overwrite malformed configuration |
| Preferences change after rerunning prompt setup | Explicitly answer fixed-default prompts before saving; Enter does not always retain saved values |

Review logs before sharing them; do not share TV codes, screen IDs, API keys or
configuration files. See the [Rust README](README.md) for CI, release platforms,
architecture and [tests and QA](README.md#tests-and-qa). Automated loopback tests
do not replace this physical-device playback check.
