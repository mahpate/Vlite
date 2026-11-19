FROM rust:1.89-bookworm
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
COPY benchmarks/ha_reader_apply.rs ./benchmarks/ha_reader_apply.rs
COPY benchmarks/ha_reader_apply_tests.rs ./benchmarks/ha_reader_apply_tests.rs
COPY benchmarks/ha_reader_strategies.rs ./benchmarks/ha_reader_strategies.rs
COPY benchmarks/ha_reader_resource.rs ./benchmarks/ha_reader_resource.rs
COPY benchmarks/ha_reader_resource_tests.rs ./benchmarks/ha_reader_resource_tests.rs
COPY bindings/c ./bindings/c
COPY crates ./crates
ARG VLITE_RESOURCE_SOURCE_REVISION
ARG VLITE_RESOURCE_SOURCE_DIRTY_PATHS=""
ENV VLITE_RESOURCE_SOURCE_REVISION=${VLITE_RESOURCE_SOURCE_REVISION} \
    VLITE_RESOURCE_SOURCE_DIRTY_PATHS=${VLITE_RESOURCE_SOURCE_DIRTY_PATHS}
RUN test "${#VLITE_RESOURCE_SOURCE_REVISION}" -eq 40 \
    && printf '%s\n' "$VLITE_RESOURCE_SOURCE_REVISION" | grep -Eq '^[0-9a-f]{40}$' \
    && cargo build --locked --release --features json --example ha-reader-apply
RUN printf '%s\n' \
    '#!/bin/sh' \
    'set -eu' \
    'work=$(mktemp -d)' \
    'trap '\''rm -rf "$work"'\'' EXIT HUP INT TERM' \
    'if /app/target/release/examples/ha-reader-apply --resource-evidence >"$work/out" 2>"$work/err"; then' \
    '  gzip -n -c "$work/out" >"$work/out.gz"' \
    '  hash=$(sha256sum "$work/out.gz"); hash=${hash%% *}' \
    '  bytes=$(wc -c <"$work/out.gz" | tr -d "[:space:]")' \
    '  base64 -w 76 "$work/out.gz" >"$work/payload"' \
    '  printf "VLITE-MEAS BEGIN %s %s\n" "$hash" "$bytes"' \
    '  awk '\''{ printf "VLITE-MEAS %d %s\n", NR, $0 } END { printf "VLITE-MEAS END %d\n", NR }'\'' "$work/payload"' \
    'else' \
    '  code=$?' \
    '  printf "VLITE-MEAS FAILED %s\n" "$code"' \
    '  tail -n 50 "$work/err" | sed "s/^/VLITE-MEAS-ERR /"' \
    '  exit "$code"' \
    'fi' \
    > /usr/local/bin/vlite-meas \
    && chmod 755 /usr/local/bin/vlite-meas \
    && useradd --create-home --uid 10001 vlite
USER vlite
ENTRYPOINT ["/usr/local/bin/vlite-meas"]
