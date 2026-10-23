# syntax=docker/dockerfile:1
# forge-proxy image. Build from the repository root:
#
#   docker build -f crates/forge-proxy/Dockerfile -t registry.example.com/acme/forge-proxy:0.1.0 .
#
# Push it to a registry the deploy hosts can pull from and reference it by
# digest in config/deploy.yml (proxy.image). See docs/deployment.md and
# ADR 0042. Crates/forge-proxy/Dockerfile.dockerignore keeps target/, .git
# and the docs out of the build context.
#
# Both base images are pinned by the digest of their multi-architecture index
# (docs/supply-chain.md says how to refresh them); the release workflow signs
# the image built from this file. The Rust tag matches rust-toolchain.toml, and
# crates/forge/tests/supply_chain.rs fails when it or a digest is missing.

FROM rust:1.99.0-slim-trixie@sha256:2752b332db73fdbb7dc576f06c82ed1f312005784ef913d7e04a28f5f55dc581 AS build
WORKDIR /src
COPY . .
# Cargo.lock is required: the image is built from the reviewed dependency graph.
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,target=/src/target,sharing=locked \
    cargo build --release --locked -p forge-proxy --bin forge-proxy \
    && install -D -m 0755 target/release/forge-proxy /out/forge-proxy
# Directories the nonroot user (65532) owns: routing state and ACME cache
# (mount a volume at /var/lib/forge-proxy) and the control socket.
RUN mkdir -p /out/var/lib/forge-proxy/acme /out/run/forge-proxy \
    && chmod 0700 /out/var/lib/forge-proxy /out/var/lib/forge-proxy/acme /out/run/forge-proxy

FROM gcr.io/distroless/cc-debian13:nonroot@sha256:b7e81cb1af38d9345680e68bcf564d388f96304e25222030fc8c051faf8565b3 AS runtime
COPY --from=build /out/forge-proxy /usr/local/bin/forge-proxy
COPY --from=build --chown=65532:65532 /out/var/lib/forge-proxy /var/lib/forge-proxy
COPY --from=build --chown=65532:65532 /out/run/forge-proxy /run/forge-proxy
USER 65532:65532
# Unprivileged ports; publish them as 80 and 443 on the host.
EXPOSE 8080 8443
VOLUME ["/var/lib/forge-proxy"]
HEALTHCHECK --interval=10s --timeout=3s --start-period=5s --retries=3 \
    CMD ["/usr/local/bin/forge-proxy", "healthcheck"]
ENTRYPOINT ["/usr/local/bin/forge-proxy"]
CMD ["run"]
