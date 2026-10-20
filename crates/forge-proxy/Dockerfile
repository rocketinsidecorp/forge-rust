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
# Pin both base images by digest (image@sha256:...) for release builds.
# RUST_VERSION matches rust-toolchain.toml.

ARG RUST_VERSION=1.98.1

FROM rust:${RUST_VERSION}-slim-trixie AS build
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

FROM gcr.io/distroless/cc-debian13:nonroot AS runtime
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
