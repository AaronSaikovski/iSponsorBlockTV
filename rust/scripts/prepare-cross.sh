#!/usr/bin/env bash
# Prepare exactly one Linux target. Requires Docker/Buildx with Linux containers
# and linux/amd64 build support (native or emulated), Git, rustup with 1.99.1,
# and the Cargo installation bin directory on PATH. Network access is required
# for source, crates, and Ubuntu build inputs.
# Capture stdout only after checking this command's exit status, then evaluate
# its two exports. Work directories are retained for provenance and CROSS_CONFIG.
set -euo pipefail
umask 077

# Nothing except the final exports may reach the caller's stdout.
exec 3>&1 1>&2
readonly revision=a9301e91f72bba3850460512685f6b16062015ff
readonly toolchain=1.99.1
work_dir=

fail() {
    printf 'prepare-cross: %s\n' "$*" >&2
    exit 1
}

finish() {
    local status=$?
    if [[ $status -ne 0 ]]; then
        printf 'prepare-cross: preparation failed; no exports emitted.\n' >&2
        if [[ -n "$work_dir" ]]; then
            printf 'prepare-cross: retained work directory: %s\n' "$work_dir" >&2
        fi
    fi
}
trap finish EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

[[ $# -eq 1 ]] || fail 'usage: prepare-cross.sh <Linux target triple>'
target=$1
case "$target" in
    x86_64-unknown-linux-musl|aarch64-unknown-linux-musl|armv7-unknown-linux-musleabihf|i686-unknown-linux-musl|arm-unknown-linux-musleabihf) ;;
    *) fail "unsupported target: $target" ;;
esac

for program in git rustup docker mktemp tr; do
    command -v "$program" >/dev/null || fail "missing required program: $program"
done
rustup run "$toolchain" rustc --version
rustup run "$toolchain" cargo --version
docker buildx version
daemon=$(docker info --format '{{.OSType}}/{{.Architecture}}')
case "$daemon" in
    linux/*) ;;
    *) fail "requires a Linux Docker daemon; found $daemon" ;;
esac
# Use Docker's daemon-backed builder so the locally built base image is visible
# to the subsequent derived build; an arbitrary buildx-container builder is not.
builder=$(docker context show)
inspection=$(docker buildx inspect "$builder")
driver=
while read -r key value; do
    if [[ "$key" == Driver: ]]; then
        driver=$value
        break
    fi
done <<< "$inspection"
[[ "$driver" == docker ]] || fail 'the current Docker context builder must use the docker driver'

if [[ -n ${CARGO_HOME:-} ]]; then
    install_root=$CARGO_HOME
else
    [[ -n ${HOME:-} ]] || fail 'HOME or CARGO_HOME is required for installing cross'
    install_root=$HOME/.cargo
fi
mkdir -p "$install_root/bin"
install_root=$(cd "$install_root" && pwd -P)
# No third export is needed or permitted: the installed command must already be
# reachable through PATH. Check normalized entries without running another cross.
install_on_path=false
IFS=: read -r -a path_entries <<< "$PATH"
for entry in "${path_entries[@]}"; do
    [[ -d ${entry:-.} ]] || continue
    normalized=$(cd "${entry:-.}" && pwd -P)
    if [[ "$normalized" == "$install_root/bin" ]]; then
        install_on_path=true
        break
    fi
done
[[ "$install_on_path" == true ]] || fail "add $install_root/bin to PATH before running this script"

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)
template=$script_dir/../Cross.toml
[[ -r "$template" ]] || fail "cannot read configuration template: $template"
if [[ -n ${RUNNER_TEMP:-} ]]; then
    [[ -d "$RUNNER_TEMP" ]] || fail 'RUNNER_TEMP must name an existing directory'
    temp_root=$(cd "$RUNNER_TEMP" && pwd -P)
    work_dir=$(mktemp -d "$temp_root/isponsorblocktv-cross.XXXXXXXX")
else
    work_dir=$(mktemp -d)
fi
work_dir=$(cd "$work_dir" && pwd -P)
[[ "$work_dir" != *$'\n'* && "$work_dir" != *$'\r'* ]] || fail 'temporary directory path must not contain line breaks'
checkout=$work_dir/cross
printf 'prepare-cross: source revision %s; work directory %s\n' "$revision" "$work_dir"
git init "$checkout"
git -C "$checkout" remote add origin https://github.com/cross-rs/cross.git
git -C "$checkout" fetch --depth 1 origin "$revision"
git -C "$checkout" checkout --detach FETCH_HEAD
actual_revision=$(git -C "$checkout" rev-parse HEAD)
[[ "$actual_revision" == "$revision" ]] || fail 'fetched source does not match the pinned revision'
[[ -f "$checkout/docker/Dockerfile.$target" ]] || fail 'pinned checkout is missing the target Dockerfile'

# Install from the same verified checkout used for the builder, not a registry
# release or a moving branch. The source's lockfile governs cross dependencies.
rustup run "$toolchain" cargo install --locked --force --path "$checkout" --bin cross --root "$install_root"
hash -r
resolved_cross=$(command -v cross) || fail 'installed cross is not reachable'
resolved_dir=$(cd "$(dirname "$resolved_cross")" && pwd -P)
[[ "$resolved_dir/$(basename "$resolved_cross")" == "$install_root/bin/cross" ]] || fail "another cross shadows $install_root/bin/cross in PATH"

# Upstream's pinned Dockerfiles build Ubuntu 24.04 host tools and target C
# toolchains. Their final scratch stage copies that complete Ubuntu filesystem.
docker build --builder "$builder" --platform linux/amd64 --pull=false \
    --iidfile "$work_dir/base.iid" \
    --file "$checkout/docker/Dockerfile.$target" "$checkout/docker"
IFS= read -r base_id < "$work_dir/base.iid" || [[ -n ${base_id:-} ]]
[[ "$base_id" =~ ^sha256:[0-9a-f]{64}$ ]] || fail 'Docker returned an invalid base image ID'
base_id=$(docker image inspect --format '{{.Id}}' "$base_id")
[[ "$base_id" =~ ^sha256:[0-9a-f]{64}$ ]] || fail 'could not inspect the base image ID'
base_tag=isponsorblocktv-cross-base-$target:${base_id#sha256:}
docker image tag "$base_id" "$base_tag"

mkdir "$work_dir/derived"
cat > "$work_dir/derived/Dockerfile" <<EOF
FROM $base_tag
LABEL org.opencontainers.image.source="https://github.com/cross-rs/cross" \\
      org.opencontainers.image.revision="$revision"
RUN apt-get update \\
    && DEBIAN_FRONTEND=noninteractive apt-get install --yes --no-install-recommends perl make ca-certificates \\
    && rm -rf /var/lib/apt/lists/*
EOF
# apt installs only missing packages (or available updates); no host package
# manager or unverified OpenSSL binary is needed. OpenSSL remains vendored by Cargo.
docker build --builder "$builder" --platform linux/amd64 --pull=false \
    --iidfile "$work_dir/derived.iid" "$work_dir/derived"
IFS= read -r image_id < "$work_dir/derived.iid" || [[ -n ${image_id:-} ]]
[[ "$image_id" =~ ^sha256:[0-9a-f]{64}$ ]] || fail 'Docker returned an invalid derived image ID'
image_id=$(docker image inspect --format '{{.Id}}' "$image_id")
[[ "$image_id" =~ ^sha256:[0-9a-f]{64}$ ]] || fail 'could not inspect the derived image ID'
image_tag=isponsorblocktv-cross-$target:${image_id#sha256:}
docker image tag "$image_id" "$image_tag"
printf 'prepare-cross: source %s; base %s; builder %s; local tag %s\n' \
    "$revision" "$base_id" "$image_id" "$image_tag"

upper_target=$(printf '%s' "$target" | tr '[:lower:]-' '[:upper:]_')
rustflags_env=CARGO_TARGET_${upper_target}_RUSTFLAGS
config=$work_dir/Cross.toml
while IFS= read -r line || [[ -n "$line" ]]; do
    line=${line//@TARGET@/$target}
    line=${line//@IMAGE@/$image_tag}
    line=${line//@IMAGE_ID@/$image_id}
    line=${line//@RUSTFLAGS_ENV@/$rustflags_env}
    printf '%s\n' "$line"
done < "$template" > "$config"

# Shell single-quote escaping makes paths containing spaces or apostrophes safe.
quoted_config=${config//\'/\'\\\'\'}
printf "export CROSS_CONFIG='%s'\nexport ISPBTV_CROSS_IMAGE='%s'\n" \
    "$quoted_config" "$image_tag" >&3
