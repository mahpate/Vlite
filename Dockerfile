# syntax=docker/dockerfile:1
FROM rust:1.89-bookworm AS build
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY build.rs ./
COPY proto ./proto
COPY src ./src
COPY site/assets/theme.css ./site/assets/theme.css
COPY tests ./tests
COPY examples ./examples
COPY bindings/c ./bindings/c
COPY crates ./crates
# Release image publishing sets VLITE_IMAGE_TESTS=0 because the same suite has
# already passed natively on each published architecture, and
# VLITE_IMAGE_BINS=shipped to build only the binaries the runtime and api
# images copy (same features, no vlite-ffi). The default "all" builds every
# binary plus the graph-enabled C library.
ARG VLITE_IMAGE_TESTS=1
ARG VLITE_IMAGE_BINS=all
RUN if [ "$VLITE_IMAGE_TESTS" = "1" ]; then cargo test --locked --workspace --all-targets --all-features; fi \
    && case "$VLITE_IMAGE_BINS" in \
         all) shipped="" ;; \
         shipped) shipped="--bin vlite-smoke --bin vlite-maintain --bin vlite-bench --bin vlite-server" ;; \
         *) echo "VLITE_IMAGE_BINS must be all or shipped" >&2; exit 1 ;; \
       esac \
    && cargo build --locked --release ${shipped:---bins} --features http-embedder,otel,tools,import-formats,json,mcp,server,grpc,gpu,mcp-proxy \
    && if [ "$VLITE_IMAGE_BINS" = "all" ]; then cargo build --locked --release -p vlite-ffi; fi

FROM build AS test
CMD ["cargo", "test", "--locked", "--workspace", "--all-targets", "--all-features"]

FROM debian:bookworm-slim AS runtime
RUN groupadd --gid 10001 vlite \
    && useradd --uid 10001 --gid vlite --no-create-home --shell /usr/sbin/nologin vlite \
    && mkdir /data \
    && mkdir /admin \
    && chown vlite:vlite /data /admin \
    && chmod 700 /admin
COPY --from=build /app/target/release/vlite-smoke /usr/local/bin/vlite-smoke
COPY --from=build /app/target/release/vlite-maintain /usr/local/bin/vlite-maintain
COPY --from=build /app/target/release/vlite-bench /usr/local/bin/vlite-bench
COPY --from=build /app/target/release/vlite-server /usr/local/bin/vlite-server
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
    /usr/share/nginx/html/index.html
USER 101
HEALTHCHECK --interval=30s --timeout=3s --start-period=5s --retries=3 \
    CMD wget -q -O /dev/null http://127.0.0.1:8080/ || exit 1
