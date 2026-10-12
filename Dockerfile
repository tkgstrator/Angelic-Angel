# syntax=docker/dockerfile:1.7
FROM rust:1.99.0-slim-bookworm AS builder
RUN apt-get update \
    && apt-get install -y --no-install-recommends build-essential pkg-config libssl-dev cmake \
    && rm -rf /var/lib/apt/lists/*
ARG TARGETARCH
WORKDIR /src
# Keep sources out of image layers and reuse downloads/builds per architecture.
RUN --mount=type=bind,source=.,target=/src,readonly \
    --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,id=angelic-angel-target-${TARGETARCH},target=/cargo-target,sharing=locked \
    RUSTUP_TOOLCHAIN=1.99.0 CARGO_TARGET_DIR=/cargo-target \
    CARGO_PROFILE_RELEASE_STRIP=symbols cargo build --release --locked \
    && install -D /cargo-target/release/angelic-angel /out/angelic-angel \
    && install -d -m 0700 -o 65532 -g 65532 /out/rootfs/config

# Debian 12 matches the builder's glibc and includes OpenSSL 3 and CA roots.
# The runtime contains no shell, package manager, compiler or application source.
FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=builder /out/angelic-angel /usr/local/bin/angelic-angel
# Copy the config directory itself; copying an empty directory's contents creates
# a new destination with Docker's default 0755 mode instead of preserving 0700.
COPY --from=builder /out/rootfs/ /
COPY LICENSE /usr/share/licenses/angelic-angel/LICENSE
COPY LICENSE.x-transaction /usr/share/licenses/angelic-angel/LICENSE.x-transaction
ENV HOME=/config
# The config path comes from the environment; `--config` on the command line still wins.
ENV ANGELIC_ANGEL_CONFIG=/config/angelic-angel.toml
USER 65532:65532
WORKDIR /config
STOPSIGNAL SIGINT
ENTRYPOINT ["/usr/local/bin/angelic-angel"]
CMD ["listen"]
