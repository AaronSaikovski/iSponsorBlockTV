# Raspberry Pi: build on Mac, deploy on Pi

Requires Docker Desktop running on your Mac, and Docker Engine plus the Compose
plugin on the Pi. The Pi must run **64-bit Raspberry Pi OS**; on the Pi,
`dpkg --print-architecture` must print `arm64`, not `armhf`.

These commands use SSH login `wrpadm01@west-ryde-pi` and deploy to
`~/docker/isponsorblocktv-rust` on the Pi.

## 1. On your Mac: build and export

Open a **local Mac terminal**, not an SSH session on the Pi. Change into this
repository's `rust/` directory, then run:

```sh
docker buildx build --platform linux/arm64 --load \
  -f Dockerfile -t isponsorblocktv-rust:pi ..
docker run --rm --network=none isponsorblocktv-rust:pi --version
docker image save -o isponsorblocktv-rust-arm64.tar isponsorblocktv-rust:pi
```

The `..` build context is required. Apple Silicon builds ARM64 natively; Intel
Macs require Docker Desktop's ARM64 emulation and may build more slowly.

## 2. Still on your Mac: copy to the Pi

Run from the same `rust/` directory:

```sh
ssh wrpadm01@west-ryde-pi 'mkdir -p ~/docker/isponsorblocktv-rust'
scp isponsorblocktv-rust-arm64.tar compose.yml \
  wrpadm01@west-ryde-pi:~/docker/isponsorblocktv-rust/
```

Do **not** run this `scp` command at the Pi's prompt: its source files are on
 your Mac. If you are connected to the Pi over SSH, run `exit` first to return
 to your Mac's shell. If the hostname does not resolve, substitute the Pi's IP
 address for `west-ryde-pi`.

## 3. On the Pi: load the image

Connect from your Mac:

```sh
ssh wrpadm01@west-ryde-pi
```

All remaining commands run **on the Pi**:

```sh
cd ~/docker/isponsorblocktv-rust
docker image load -i isponsorblocktv-rust-arm64.tar
docker image inspect isponsorblocktv-rust:pi --format '{{.Os}}/{{.Architecture}}'
docker run --rm --network=none isponsorblocktv-rust:pi --version
```

The inspection should print `linux/arm64`. No repository checkout or Rust
installation is needed on the Pi.

## 4. On the Pi: configure and pair once

For a **new installation**, create the Compose settings and persistent storage:

```sh
cd ~/docker/isponsorblocktv-rust
cat > .env <<EOF
ISPBTV_RUST_IMAGE=isponsorblocktv-rust:pi
ISPBTV_RUST_DATA_DIR=$HOME/docker/isponsorblocktv-rust/data
EOF
mkdir -p data
docker compose run --rm --no-deps isponsorblocktv-rust setup-cli
```

If `.env` already exists, edit it instead of overwriting it and preserve your
existing data path. Exported `ISPBTV_RUST_IMAGE` or `ISPBTV_RUST_DATA_DIR` values
override `.env`; unset or update them if they point elsewhere.

Stop any other controller managing the same TV. On the TV, open
**YouTube → Settings → Link with TV code**, then follow setup:

- Choose **yes** to add a device and enter its 12-digit TV code.
- Enter **`sponsor`** for categories; blank disables segment skipping.
- Select your preferred ad and autoplay settings.
- Choose **yes** at the final save prompt. Pairing alone does not save.

Manual TV-code pairing needs no published ports or host networking. Automatic
LAN discovery has additional multicast requirements.

The transferred image does not include an existing configuration. Configuration
is stored separately in `~/docker/isponsorblocktv-rust/data/config.json`;
keep it private and back it up.

## 5. On the Pi: start and inspect

```sh
docker compose up -d
docker compose ps
docker compose logs --tail=100 -f
```

Ctrl-C stops following logs, not the service. To stop the service:

```sh
docker compose stop
```

## Updating later

Back up `data/`. Repeat steps 1 and 2 **on your Mac**, then load the image and
recreate the service **on the Pi**:

```sh
cd ~/docker/isponsorblocktv-rust
docker image load -i isponsorblocktv-rust-arm64.tar
docker compose up -d --force-recreate
```

Do not run `docker compose pull` for this local-only image tag. Do not recreate
`.env` or rerun pairing for an ordinary image update; retain `data/`.

For reconfiguration and prompt-default caveats, see
[the getting started guide](GETTING_STARTED.md#pair-and-save). The
[Linux dependency security warning](README.md#dependency-security) also applies.
