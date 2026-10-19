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
