#!/bin/sh
# MEAS-006 job-side collector for the measurement image's vlite-meas wrapper.
# Usage: meas006-collect.sh <vlite lib unit-test executable built with --features server>
# Runs the writer-fence document test once and prints only its single JSON
# document on stdout, so the wrapper's existing gzip/SHA-256 transport applies.
# Test-harness output goes to stderr. Fails closed: a failed, ignored or
# filtered-out test, or a missing or empty document, exits non-zero and prints
# nothing on stdout. Probe settings and storage come from VLITE_MEAS006_* (see
# src/server/writer_fence_bench.rs).
set -eu
if [ "$#" -ne 1 ]; then
  echo "usage: meas006-collect.sh <vlite-lib-test-executable>" >&2
  exit 2
fi
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT HUP INT TERM
VLITE_MEAS006_OUT="$work/meas006.json" "$1" --ignored --exact --test-threads=1 --nocapture \
  server::writer_fence_bench::writer_fence_meas006_document >&2
if [ ! -s "$work/meas006.json" ]; then
  echo "MEAS-006 test produced no document" >&2
  exit 1
fi
cat "$work/meas006.json"