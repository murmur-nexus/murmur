#!/usr/bin/env bash
#
# alloc-bench.sh — measure `mur`'s two allocation bursts under each candidate global
# allocator, on this host, in one run.
#
# Why this exists rather than a number quoted from somewhere:
#
#   * The allocator `murmur-cli` ships is chosen by the table this prints, and a table is only
#     worth anything if the person reading it can regenerate it. The system-allocator arm is
#     the "before" column and is built from the same toolchain in the same run, so the
#     comparison stays honest on a host nobody has seen yet.
#   * Cranelift compiles on the calling thread — `capsule-runtime` does not enable wasmtime's
#     `parallel-compilation` — which is what makes pinning to one core a measurement of the
#     allocator rather than of this machine's scheduler.
#   * The three arms are three separate binaries, so interleaving them means alternating
#     processes: one round of each per pass, so a thermal or frequency drift moves all three
#     arms together instead of whichever one ran last.
#
# Each arm is built into its own --target-dir. Flipping a feature in the shared tree would
# rebuild wasmtime on every switch and leave the tree rebuilt afterwards.
#
#   scripts/alloc-bench.sh [--cpu <n>] [--rounds <n>] [--compiles <n>] [--stores <n>]
#
# Takes roughly ten minutes with the defaults. Output is a markdown table plus the host facts
# the decision record has to carry.
set -euo pipefail

cd "$(dirname "$0")/.."

CPU=""
ROUNDS=15
COMPILES=8
STORES=40000

while [ $# -gt 0 ]; do
  case "$1" in
    --cpu) CPU="$2"; shift 2 ;;
    --rounds) ROUNDS="$2"; shift 2 ;;
    --compiles) COMPILES="$2"; shift 2 ;;
    --stores) STORES="$2"; shift 2 ;;
    -h|--help) sed -n '2,26p' "$0"; exit 0 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done

# The last CPU, on the assumption that a desktop's foreground work sits on the low-numbered
# ones. Any single core will do; what matters is that all three arms get the same one.
[ -n "$CPU" ] || CPU=$(( $(nproc) - 1 ))

command -v taskset >/dev/null || { echo "taskset is required" >&2; exit 2; }

BENCH_DIR=target/alloc-bench
OUT=$(mktemp -d)
trap 'rm -rf "$OUT"' EXIT

build_arm() {
  arm="$1"; features="$2"
  echo "building $arm ..." >&2
  cargo build --release -q \
    -p murmur-cli --bin mur-alloc-bench \
    --no-default-features --features "$features" \
    --target-dir "$BENCH_DIR/$arm"
}

build_arm system   "alloc-bench"
build_arm jemalloc "alloc-bench,jemalloc"
build_arm mimalloc "alloc-bench,mimalloc"

run_round() {
  arm="$1"
  taskset -c "$CPU" "$BENCH_DIR/$arm/release/mur-alloc-bench" \
    --rounds 1 --compiles "$COMPILES" --stores "$STORES"
}

# One discarded round per arm. The first component compile in a fresh process pays for page
# faults and lazy relocation that no later round repeats, and it would otherwise land in the
# min column of whichever arm ran first.
for arm in system jemalloc mimalloc; do
  echo "warming $arm ..." >&2
  run_round "$arm" > "$OUT/warmup-$arm.tsv"
done

for pass in $(seq 1 "$ROUNDS"); do
  echo "pass $pass/$ROUNDS ..." >&2
  for arm in system jemalloc mimalloc; do
    run_round "$arm" | awk -v pass="$pass" -F'\t' '
      $2 ~ /^[0-9]+$/ { print $1 "\t" pass "\t" $3 "\t" $4; next }
      { print }
    ' >> "$OUT/all.tsv"
  done
done

median_col() {
  awk -F'\t' -v arm="$1" -v col="$2" '$1 == arm && $2 ~ /^[0-9]+$/ { print $col }' "$OUT/all.tsv" \
    | sort -n | awk '{ v[NR] = $1 } END { print (NR % 2) ? v[(NR+1)/2] : int((v[NR/2] + v[NR/2+1]) / 2) }'
}
minmax_col() {
  awk -F'\t' -v arm="$1" -v col="$2" '$1 == arm && $2 ~ /^[0-9]+$/ { print $col }' "$OUT/all.tsv" \
    | sort -n | awk 'NR == 1 { min = $1 } { max = $1 } END { print min "\t" max }'
}
# How many passes this arm beat the system arm on, comparing the two rounds that ran in the
# same pass rather than the two sorted distributions.
wins_col() {
  awk -F'\t' -v arm="$1" -v col="$2" '
    $2 ~ /^[0-9]+$/ && $1 == "system" { sys[$2] = $col }
    $2 ~ /^[0-9]+$/ && $1 == arm      { mine[$2] = $col }
    END { for (p in mine) if (mine[p] < sys[p]) wins++; print wins + 0 }
  ' "$OUT/all.tsv"
}

# A round's line carries the total for all the compiles or all the stores in it, so both
# columns are divided down to one operation. The two live six orders of magnitude apart — a
# component compile is seconds, a Store create-and-drop is under a microsecond — so they get
# different units rather than a shared one that rounds the Store column to nothing.
ms() { awk -v ns="$1" -v n="$2" 'BEGIN { printf "%.1f", ns / n / 1000000 }'; }
us() { awk -v ns="$1" -v n="$2" 'BEGIN { printf "%.3f", ns / n / 1000 }'; }
pct() { awk -v a="$1" -v b="$2" 'BEGIN { printf "%+.1f%%", (a - b) * 100 / b }'; }

SYS_COMPILE=$(median_col system 3)
SYS_STORE=$(median_col system 4)

# `awk NR == 1` and not `head -1`: under `pipefail`, a `head` that exits early can deliver
# SIGPIPE to the writer and take the whole script down after the measurement has already run.
GLIBC=$(ldd --version 2>/dev/null | awk 'NR == 1 { print $NF }')
FIXTURE=crates/murmur-cli/tests/fixtures/drivers/anthropic/driver/murmur-driver-anthropic.wasm

echo
echo "Host:      $(uname -srm), glibc $GLIBC"
echo "Pinned to: CPU $CPU (taskset), $(nproc) CPUs online"
echo "Rounds:    $ROUNDS measured (1 discarded warmup per arm), $COMPILES compiles and $STORES stores per round"
echo "Fixture:   $FIXTURE ($(stat -c %s "$FIXTURE") bytes)"
echo
echo "| Allocator | Median compile | Δ compile | Compile min/max | Median Store | Δ Store | Store min/max | Compile rounds faster | Store rounds faster |"
echo "|---|---|---|---|---|---|---|---|---|"
for arm in system jemalloc mimalloc; do
  cmed=$(median_col "$arm" 3); smed=$(median_col "$arm" 4)
  IFS=$'\t' read -r cmin cmax <<< "$(minmax_col "$arm" 3)"
  IFS=$'\t' read -r smin smax <<< "$(minmax_col "$arm" 4)"
  if [ "$arm" = system ]; then
    cdelta="—"; sdelta="—"; cwins="—"; swins="—"
  else
    cdelta=$(pct "$cmed" "$SYS_COMPILE"); sdelta=$(pct "$smed" "$SYS_STORE")
    cwins="$(wins_col "$arm" 3)/$ROUNDS"; swins="$(wins_col "$arm" 4)/$ROUNDS"
  fi
  echo "| $arm | $(ms "$cmed" "$COMPILES") ms | $cdelta | $(ms "$cmin" "$COMPILES") / $(ms "$cmax" "$COMPILES") ms | $(us "$smed" "$STORES") µs | $sdelta | $(us "$smin" "$STORES") / $(us "$smax" "$STORES") µs | $cwins | $swins |"
done
echo
echo "Compile figures are per component compile; Store figures are per create-and-drop pair."
echo "Load average during the run: $(uptime | sed 's/.*load average: //')"
echo "Threads sampled by each arm:"
grep -h -E 'threads_(before|after)' "$OUT/all.tsv" | sort -u | sed 's/^/  /'
echo
# The bar is 12 of 15 rounds. Scaled rather than hardcoded so that overriding --rounds states a
# threshold that means the same thing instead of one the run cannot reach.
WINS_NEEDED=$(( (ROUNDS * 12 + 14) / 15 ))
echo "Rule: a candidate reproduces its gain when its median compile is at least 3% below the"
echo "system arm's and it is the faster arm in at least $WINS_NEEDED of the $ROUNDS rounds."

cp "$OUT/all.tsv" "$BENCH_DIR/rounds.tsv"
echo "Raw rounds: $BENCH_DIR/rounds.tsv"
