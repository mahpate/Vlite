# syntax=docker/dockerfile:1
FROM rust:1.89-bookworm AS build
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY tests ./tests
COPY examples ./examples
COPY bindings/c ./bindings/c
RUN cargo test --locked --workspace --all-targets --all-features \
    && cargo build --locked --release --bins --all-features \
    && cargo build --locked --release -p vlite-ffi

FROM build AS test
CMD ["cargo", "test", "--locked", "--workspace", "--all-targets", "--all-features"]

FROM debian:bookworm-slim AS runtime
RUN groupadd --gid 10001 vlite \
    && useradd --uid 10001 --gid vlite --no-create-home --shell /usr/sbin/nologin vlite \
    && mkdir /data \
    && chown vlite:vlite /data
COPY --from=build /app/target/release/vlite-smoke /usr/local/bin/vlite-smoke
COPY --from=build /app/target/release/vlite-maintain /usr/local/bin/vlite-maintain
COPY --from=build /app/target/release/vlite-bench /usr/local/bin/vlite-bench
COPY --from=build /app/target/release/vlite-server /usr/local/bin/vlite-server
USER 10001:10001
WORKDIR /data
ENTRYPOINT ["/usr/local/bin/vlite-smoke"]
CMD ["run", "/data/smoke.vlite"]

FROM nginxinc/nginx-unprivileged:1.30.6-alpine AS dashboard
COPY deploy/dashboard.conf /etc/nginx/conf.d/default.conf
COPY src/dashboard/ /usr/share/nginx/html/
USER root
RUN sed -i \
    -e 's|name="vlite-api-base" content=""|name="vlite-api-base" content="/api"|' \
    -e 's|href="/dashboard/style.css"|href="/style.css"|' \
    -e 's|src="/dashboard/app.js"|src="/app.js"|' \
    /usr/share/nginx/html/index.html
USER 101
