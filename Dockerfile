# Multi-stage build producing a static musl binary on a distroless base.
# Builds natively for the requested platform (no cross-linker needed), so it
# works under `docker buildx --platform linux/amd64,linux/arm64`.
ARG RUST_VERSION=1.95

FROM rust:${RUST_VERSION}-bookworm AS builder
ARG TARGETARCH
RUN apt-get update \
    && apt-get install -y --no-install-recommends protobuf-compiler musl-tools \
    && rm -rf /var/lib/apt/lists/*
RUN case "${TARGETARCH}" in \
      amd64) echo x86_64-unknown-linux-musl ;; \
      arm64) echo aarch64-unknown-linux-musl ;; \
      *) echo "unsupported TARGETARCH=${TARGETARCH}" >&2; exit 1 ;; \
    esac > /target.txt \
    && rustup target add "$(cat /target.txt)"
WORKDIR /src
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked --target "$(cat /target.txt)" \
    && cp "target/$(cat /target.txt)/release/pfp" /pfp

# distroless/static ships CA certificates (needed for HTTPS to Grafana Cloud).
# Runs as root by default so cross-uid process_vm_readv works with CAP_SYS_PTRACE.
FROM gcr.io/distroless/static-debian12
COPY --from=builder /pfp /usr/local/bin/pfp
ENTRYPOINT ["/usr/local/bin/pfp"]
