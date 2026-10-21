#!/bin/sh
# Check nestoa against the NestJS OpenAPI generation benchmark
# (https://github.com/algoan/nestjs-openapi-generation-benchmark): generate the document of its
# `nestoa-main` variant with a candidate and a baseline binary, then exit with
#   1 (regression, may be intended) when the candidate has more differences with the expected
#     document than the baseline in any category, or fails `--strict` where the baseline passes;
#   2 (error, never intended) when the candidate is not deterministic (two runs give different
#     bytes or exit statuses), a binary writes no document, or the benchmark cannot run.
#
#   scripts/benchmark.sh [candidate] [baseline]
#     candidate: binary to check (default: target/release/nestoa)
#     baseline:  binary to compare with (default: none, the differences are only reported)
#   BENCHMARK_DIR:   checkout of the benchmark, with `npm --prefix bench ci` done
#                    (default: ../nestjs-openapi-generation-benchmark)
#   BENCHMARK_SUMMARY: markdown file the report is appended to (e.g. $GITHUB_STEP_SUMMARY)
set -eu

CANDIDATE="${1:-target/release/nestoa}"
BASELINE="${2:-}"
BENCH="${BENCHMARK_DIR:-../nestjs-openapi-generation-benchmark}"
PROJECT="$BENCH/variants/nestoa-main"
OUT="$(mktemp -d)"
trap 'rm -rf "$OUT"' EXIT

if [ ! -f "$BENCH/bench/src/cli.ts" ]; then
  echo "benchmark not found in $BENCH: clone it or set BENCHMARK_DIR" >&2
  exit 2
fi

# generate <binary> <output>: prints the --strict exit status
generate() {
  status=0
  "$1" "$PROJECT" -o "$2" --strict >"$2.log" 2>&1 || status=$?
  echo "$status"
}

failed=0
summary() {
  printf '%s\n' "$1"
  if [ -n "${BENCHMARK_SUMMARY:-}" ]; then
    printf '%s\n' "$1" >>"$BENCHMARK_SUMMARY"
  fi
}

# require_document <name>: stop when a binary wrote no document (crash, wrong path)
require_document() {
  if [ ! -s "$OUT/$1.yaml" ]; then
    summary "❌ The $1 wrote no document:"
    summary "$(printf '\n```\n%s\n```\n' "$(cat "$OUT/$1.yaml.log")")"
    exit 2
  fi
}

candidate_strict=$(generate "$CANDIDATE" "$OUT/candidate.yaml")
require_document candidate
candidate_strict_2=$(generate "$CANDIDATE" "$OUT/candidate-2.yaml")
if [ "$candidate_strict" != "$candidate_strict_2" ] || ! cmp -s "$OUT/candidate.yaml" "$OUT/candidate-2.yaml"; then
  summary "❌ Two runs of the candidate gave different documents or exit statuses."
  exit 2
fi

if [ -n "$BASELINE" ]; then
  baseline_strict=$(generate "$BASELINE" "$OUT/baseline.yaml")
  require_document baseline
  if [ "$candidate_strict" != 0 ] && [ "$baseline_strict" = 0 ]; then
    summary "❌ \`--strict\` fails with the candidate and passes with the baseline:"
    summary "$(printf '\n```\n%s\n```\n' "$(cat "$OUT/candidate.yaml.log")")"
    failed=1
  fi
  status=0
  node "$BENCH/bench/src/cli.ts" check "$OUT/candidate.yaml" --baseline "$OUT/baseline.yaml" \
    ${BENCHMARK_SUMMARY:+--summary "$BENCHMARK_SUMMARY"} >"$OUT/check.md" || status=$?
  cat "$OUT/check.md"
  # `check` exits 1 on a regression, after printing its verdict; anything else is a crash
  if [ "$status" = 1 ] && grep -q '^More differences' "$OUT/check.md"; then
    failed=1
  elif [ "$status" != 0 ]; then
    summary "❌ The benchmark comparison failed (exit $status)."
    exit 2
  fi
else
  node "$BENCH/bench/src/cli.ts" check "$OUT/candidate.yaml" ${BENCHMARK_SUMMARY:+--summary "$BENCHMARK_SUMMARY"}
fi

exit "$failed"
