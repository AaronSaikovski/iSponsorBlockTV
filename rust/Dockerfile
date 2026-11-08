# syntax=docker/dockerfile:1
# Build from the repository root: docker build -f rust/Dockerfile -t isponsorblocktv-rust .
FROM rust:1.99.1-alpine AS compiler
ARG TARGETARCH

RUN apk add --no-cache build-base perl ca-certificates binutils
WORKDIR /build
COPY rust/Cargo.toml rust/Cargo.lock rust/rust-toolchain.toml ./
COPY rust/src ./src

# Select the native musl target explicitly so static flags do not affect host proc macros.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,id=isponsorblocktv-target-${TARGETARCH},target=/build/target,sharing=locked \
    set -eu; \
    target="$(rustc -vV | sed -n 's/^host: //p')"; \
    RUSTFLAGS="-C target-feature=+crt-static" \
        cargo build --locked --release --target "$target" --bin iSponsorBlockTV; \
    mkdir -p /out; \
    cp "target/$target/release/iSponsorBlockTV" /out/iSponsorBlockTV; \
    strip --strip-all /out/iSponsorBlockTV; \
    readelf -l -d /out/iSponsorBlockTV > /out/elf.txt; \
    if grep -Eq 'INTERP|NEEDED' /out/elf.txt; then \
        cat /out/elf.txt; exit 1; \
    fi; \
    /out/iSponsorBlockTV --version

FROM scratch
COPY --from=compiler /out/iSponsorBlockTV /app/iSponsorBlockTV
COPY --from=compiler /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/ca-certificates.crt
COPY LICENSE.md /app/LICENSE.md
COPY rust/THIRD_PARTY_NOTICES.md /app/THIRD_PARTY_NOTICES.md
ENV SSL_CERT_FILE=/etc/ssl/certs/ca-certificates.crt \
    iSPBTV_data_dir=/app/data \
    iSPBTV_docker=True \
    TERM=xterm-256color \
    COLORTERM=truecolor
WORKDIR /app/data
WORKDIR /app
ENTRYPOINT ["/app/iSponsorBlockTV"]
