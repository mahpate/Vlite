# syntax=docker/dockerfile:1
FROM rust:1.89-bookworm AS source
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY build.rs ./
COPY proto ./proto
COPY src ./src
COPY site/assets/theme.css ./site/assets/theme.css
COPY site/assets/fonts ./site/assets/fonts
COPY site/assets/vendor/three ./site/assets/vendor/three
COPY tests ./tests
COPY examples ./examples
COPY bindings/c ./bindings/c
COPY crates ./crates

FROM source AS test
ARG TARGETARCH
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,target=/app/target/debug,sharing=locked \
    case "$TARGETARCH" in \
      amd64) nextest_arch=x86_64; nextest_sha=b0959bc64232974224cf9fb22c915845398c0e8668fa7831eb5b43fb1eef781f ;; \
      arm64) nextest_arch=aarch64; nextest_sha=9164568e9840c6509177086828ac7d3178c2d8feb0658b4d2bff62fa620e98c8 ;; \
      *) echo "Unsupported Nextest build architecture: $TARGETARCH" >&2; exit 1 ;; \
    esac \
    && curl -fsSL "https://github.com/nextest-rs/nextest/releases/download/cargo-nextest-0.9.150/cargo-nextest-0.9.150-${nextest_arch}-unknown-linux-gnu.tar.gz" -o /tmp/cargo-nextest.tar.gz \
    && echo "$nextest_sha  /tmp/cargo-nextest.tar.gz" | sha256sum -c - \
    && tar -xzf /tmp/cargo-nextest.tar.gz -C /usr/local/bin cargo-nextest \
    && rm /tmp/cargo-nextest.tar.gz \
    && cargo nextest run --locked --workspace --all-targets --all-features --test-threads 2
# The build fails if any test fails; running this image does not repeat the suite.
CMD ["true"]

FROM source AS build
# Release image publishing sets VLITE_IMAGE_TESTS=0 because the same suite has
# already passed natively on each published architecture, and
# VLITE_IMAGE_BINS=shipped to build only the binaries the runtime and api
# images copy (same features, no vlite-ffi). The default "all" builds every
# binary plus the graph-enabled C library.
ARG VLITE_IMAGE_TESTS=1
ARG VLITE_IMAGE_BINS=all
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,target=/app/target/debug,sharing=locked \
    --mount=type=cache,target=/app/target/release,sharing=locked \
    if [ "$VLITE_IMAGE_TESTS" = "1" ]; then cargo test --locked --workspace --all-targets --all-features; \
       elif [ "$VLITE_IMAGE_TESTS" != "0" ]; then echo "VLITE_IMAGE_TESTS must be 0 or 1" >&2; exit 1; fi \
    && case "$VLITE_IMAGE_BINS" in \
         all) shipped="" ;; \
         shipped) shipped="--bin vlite-smoke --bin vlite-maintain --bin vlite-bench --bin vlite-server" ;; \
         *) echo "VLITE_IMAGE_BINS must be all or shipped" >&2; exit 1 ;; \
       esac \
    && cargo build --locked --release ${shipped:---bins} --features http-embedder,otel,tools,import-formats,json,mcp,server,grpc,gpu,mcp-proxy \
    && if [ "$VLITE_IMAGE_BINS" = "all" ]; then cargo build --locked --release -p vlite-ffi; fi \
    && mkdir /out \
    && cp target/release/vlite-smoke target/release/vlite-maintain target/release/vlite-bench target/release/vlite-server /out/

FROM debian:bookworm-slim AS runtime
RUN groupadd --gid 10001 vlite \
    && useradd --uid 10001 --gid vlite --no-create-home --shell /usr/sbin/nologin vlite \
    && mkdir /data \
    && mkdir /admin \
    && chown vlite:vlite /data /admin \
    && chmod 700 /admin
COPY --from=build /out/vlite-smoke /usr/local/bin/vlite-smoke
COPY --from=build /out/vlite-maintain /usr/local/bin/vlite-maintain
COPY --from=build /out/vlite-bench /usr/local/bin/vlite-bench
COPY --from=build /out/vlite-server /usr/local/bin/vlite-server
USER 10001:10001
WORKDIR /data
ENTRYPOINT ["/usr/local/bin/vlite-smoke"]
CMD ["run", "/data/smoke.vlite"]

FROM runtime AS api
ENTRYPOINT ["/usr/local/bin/vlite-server"]
CMD ["/data/vectors.vlite", "--create-if-missing", "3", "--bind", "0.0.0.0:8080", "--allow-remote"]

FROM nginxinc/nginx-unprivileged:1.30.6-alpine AS dashboard
COPY deploy/dashboard.conf /etc/nginx/conf.d/default.conf
COPY src/dashboard/ /usr/share/nginx/html/
COPY site/assets/theme.css /usr/share/nginx/html/theme.css
COPY site/assets/fonts /usr/share/nginx/html/fonts
COPY site/assets/vendor/three/three.min.js /usr/share/nginx/html/three.min.js
COPY site/assets/vendor/three/LICENSE /usr/share/nginx/html/three.LICENSE
USER root
RUN sed -i \
    -e 's|name="vlite-api-base" content=""|name="vlite-api-base" content="/api"|' \
    -e 's|href="/dashboard/style.css"|href="/style.css"|' \
    -e 's|href="/dashboard/graph.css"|href="/graph.css"|' \
    -e 's|src="/dashboard/app.js"|src="/app.js"|' \
    -e 's|src="/dashboard/console-tools.js"|src="/console-tools.js"|' \
    -e 's|src="/dashboard/admin-tools.js"|src="/admin-tools.js"|' \
    -e 's|src="/dashboard/observability.js"|src="/observability.js"|' \
    -e 's|src="/dashboard/navigation.js"|src="/navigation.js"|' \
    -e 's|src="/dashboard/graph.js"|src="/graph.js"|' \
    -e 's|src="/dashboard/ambience.js"|src="/ambience.js"|' \
    /usr/share/nginx/html/index.html
USER 101
HEALTHCHECK --interval=30s --timeout=3s --start-period=5s --retries=3 \
    CMD wget -q -O /dev/null http://127.0.0.1:8080/ || exit 1
