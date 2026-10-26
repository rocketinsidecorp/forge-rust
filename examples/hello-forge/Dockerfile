# syntax=docker/dockerfile:1

# Production image for hello-forge.
#
#   docker build -t hello-forge .
#   docker run --rm -p 3000:3000 hello-forge
#
# The runtime stage is distroless: no shell, no package manager and a numeric
# non-root user. Both base images are pinned by the digest of their
# multi-architecture index. Docker builds the digest, not the tag: when
# RUST_VERSION changes (it must match rust-toolchain.toml, see
# tests/container.rs), replace the digest with the new tag's:
#   docker buildx imagetools inspect rust:<version>-slim-trixie --format '{{ .Manifest.Digest }}'

ARG RUST_VERSION=1.98.1

FROM rust:${RUST_VERSION}-slim-trixie@sha256:4cd829461bd5c4d511c32e269da9cb8929223b666519d8004e35fc8d1d771ab7 AS build
WORKDIR /app
COPY . .
# Cargo.lock is required: the image is built from the reviewed dependency graph.
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,target=/app/target,sharing=locked \
    cargo build --release --locked --bin hello-forge \
    && install -D -m 0755 target/release/hello-forge /out/hello-forge
FROM gcr.io/distroless/cc-debian13:nonroot@sha256:b7e81cb1af38d9345680e68bcf564d388f96304e25222030fc8c051faf8565b3 AS runtime
COPY --from=build /out/hello-forge /usr/local/bin/hello-forge
ENV FORGE_ENV=production \
    FORGE_HTTP_ADDR=0.0.0.0:3000
USER 65532:65532
EXPOSE 3000
# The binary probes its own /health/live endpoint; no curl or shell is needed.
HEALTHCHECK --interval=10s --timeout=3s --start-period=10s --start-interval=1s --retries=3 \
    CMD ["/usr/local/bin/hello-forge", "healthcheck"]
# Exec form: the application is PID 1 and receives SIGTERM for graceful shutdown.
ENTRYPOINT ["/usr/local/bin/hello-forge"]
CMD ["serve"]
