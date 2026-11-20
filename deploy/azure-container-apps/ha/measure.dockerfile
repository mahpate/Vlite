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
COPY benchmarks/ha_meas002.rs ./benchmarks/ha_meas002.rs
COPY benchmarks/ha_meas004.rs ./benchmarks/ha_meas004.rs
COPY benchmarks/ha_meas005.rs ./benchmarks/ha_meas005.rs
COPY benchmarks/ha_gateway_support.rs ./benchmarks/ha_gateway_support.rs
COPY deploy/azure-container-apps/ha/meas006-collect.sh ./deploy/azure-container-apps/ha/meas006-collect.sh
COPY deploy/azure-container-apps/ha/meas006-collect.sh /usr/local/bin/meas006-collect.sh
COPY bindings/c ./bindings/c
COPY crates ./crates
ARG VLITE_RESOURCE_SOURCE_REVISION
ARG VLITE_RESOURCE_SOURCE_DIRTY_PATHS=""
ENV VLITE_RESOURCE_SOURCE_REVISION=${VLITE_RESOURCE_SOURCE_REVISION} \
    VLITE_RESOURCE_SOURCE_DIRTY_PATHS=${VLITE_RESOURCE_SOURCE_DIRTY_PATHS}
RUN test "${#VLITE_RESOURCE_SOURCE_REVISION}" -eq 40 \
    && printf '%s\n' "$VLITE_RESOURCE_SOURCE_REVISION" | grep -Eq '^[0-9a-f]{40}$' \
    && cargo build --locked --release --features json --example ha-reader-apply \
    && cargo build --locked --release --features server --bin vlite-server --example ha-meas002
RUN cargo test --locked --release --features server --lib --no-run --message-format=json > /tmp/meas006-artifacts.json \
    && sed -n '/"reason":"compiler-artifact"/{ /"kind":\["lib"\]/{ /"name":"vlite"/{ /"test":true/s/.*"executable":"\(\/app\/target\/release\/deps\/vlite-[0-9a-f]*\)".*/\1/p; }; }; }' /tmp/meas006-artifacts.json > /tmp/meas006-executable \
    && test "$(wc -l < /tmp/meas006-executable)" -eq 1 \
    && test -x "$(cat /tmp/meas006-executable)" \
    && cp "$(cat /tmp/meas006-executable)" /usr/local/bin/vlite-meas006-tests \
    && chmod 755 /usr/local/bin/meas006-collect.sh \
    && rm /tmp/meas006-artifacts.json /tmp/meas006-executable
RUN printf '%s\n' \
    '#!/bin/sh' \
    'set -eu' \
    'work=$(mktemp -d)' \
    'trap '\''rm -rf "$work"'\'' EXIT HUP INT TERM' \
    'case "${VLITE_MEAS_KIND:-MEAS-007}" in' \
    '  MEAS-007) set -- /app/target/release/examples/ha-reader-apply --resource-evidence ;;' \
    '  MEAS-006) set -- /bin/sh /usr/local/bin/meas006-collect.sh /usr/local/bin/vlite-meas006-tests ;;' \
    '  MEAS-002)' \
    '    if ! printf "%s" "${VLITE_MEAS002_TARGET_B64:-}" | base64 -d >"$work/target.json"; then' \
    '      printf "VLITE-MEAS FAILED 2\n"; exit 2' \
    '    fi' \
    '    set -- /app/target/release/examples/ha-meas002 --server /app/target/release/vlite-server --target-file "$work/target.json"' \
    '    [ -z "${VLITE_MEAS002_DURATION_SECS:-}" ] || set -- "$@" --duration-secs "$VLITE_MEAS002_DURATION_SECS"' \
    '    [ -z "${VLITE_MEAS002_RATE:-}" ] || set -- "$@" --rate "$VLITE_MEAS002_RATE"' \
    '    [ -z "${VLITE_MEAS002_ARCHIVE_INTERVAL_SECS:-}" ] || set -- "$@" --archive-interval-secs "$VLITE_MEAS002_ARCHIVE_INTERVAL_SECS"' \
    '    [ -z "${VLITE_MEAS002_STALENESS_BOUND_MS:-}" ] || set -- "$@" --staleness-bound-ms "$VLITE_MEAS002_STALENESS_BOUND_MS"' \
    '    [ -z "${VLITE_MEAS002_READER_POLL_MS:-}" ] || set -- "$@" --reader-poll-ms "$VLITE_MEAS002_READER_POLL_MS"' \
    '    [ -z "${VLITE_MEAS002_OBSERVE_MS:-}" ] || set -- "$@" --observe-ms "$VLITE_MEAS002_OBSERVE_MS"' \
    '    ;;' \
    '  *) printf "VLITE-MEAS FAILED 2\n"; exit 2 ;;' \
    'esac' \
    'if "$@" >"$work/out" 2>"$work/err"; then' \
    '  gzip -n -c "$work/out" >"$work/out.gz"' \
    '  hash=$(sha256sum "$work/out.gz"); hash=${hash%% *}' \
    '  bytes=$(wc -c <"$work/out.gz" | tr -d "[:space:]")' \
    '  base64 -w 76 "$work/out.gz" >"$work/payload"' \
    '  printf "VLITE-MEAS BEGIN %s %s\n" "$hash" "$bytes"' \
    '  awk '\''{ printf "VLITE-MEAS %d %s\n", NR, $0 } END { printf "VLITE-MEAS END %d\n", NR }'\'' "$work/payload"' \
    'else' \
    '  code=$?' \
    '  printf "VLITE-MEAS FAILED %s\n" "$code"' \
    '  if [ "${VLITE_MEAS_KIND:-MEAS-007}" != MEAS-006 ]; then tail -n 50 "$work/err" | sed "s/^/VLITE-MEAS-ERR /"; fi' \
    '  exit "$code"' \
    'fi' \
    > /usr/local/bin/vlite-meas \
    && chmod 755 /usr/local/bin/vlite-meas \
    && useradd --create-home --uid 10001 vlite
USER vlite
ENTRYPOINT ["/usr/local/bin/vlite-meas"]
