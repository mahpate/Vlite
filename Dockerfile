# syntax=docker/dockerfile:1
FROM rust:1.89-bookworm AS build
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY tests ./tests
COPY examples ./examples
RUN cargo test --locked --all-targets \
    && cargo build --locked --release --bins

FROM build AS test
CMD ["cargo", "test", "--locked", "--all-targets"]

FROM debian:bookworm-slim AS runtime
RUN groupadd --gid 10001 vlite \
    && useradd --uid 10001 --gid vlite --no-create-home --shell /usr/sbin/nologin vlite \
    && mkdir /data \
    && chown vlite:vlite /data
COPY --from=build /app/target/release/vlite-smoke /usr/local/bin/vlite-smoke
COPY --from=build /app/target/release/vlite-maintain /usr/local/bin/vlite-maintain
USER 10001:10001
WORKDIR /data
ENTRYPOINT ["/usr/local/bin/vlite-smoke"]
CMD ["run", "/data/smoke.vlite"]
