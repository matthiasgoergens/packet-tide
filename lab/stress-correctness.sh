#!/usr/bin/env bash
set -euo pipefail

if [[ $# -lt 1 || $# -gt 2 ]]; then
  echo "usage: $0 OUTPUT_DIR [SCHEDULE_SEED]" >&2
  exit 2
fi

OUTPUT_DIR=$1
SCHEDULE_SEED=${2:-8421}
ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
WORK_RESULTS=/tmp/tsunami-udp-lab/results
DESIGN="$OUTPUT_DIR/design.tsv"

mkdir -p "$OUTPUT_DIR"
shopt -s nullglob
existing=("$OUTPUT_DIR"/result-*.json)
if (( ${#existing[@]} > 0 )); then
  echo "output directory already contains results: $OUTPUT_DIR" >&2
  exit 2
fi

cleanup() {
  "$ROOT/lab/cleanup.sh"
}
trap cleanup EXIT INT TERM
"$ROOT/lab/setup.sh"
python3 "$ROOT/lab/capture-provenance.py" "$OUTPUT_DIR/provenance.json"

python3 - "$SCHEDULE_SEED" >"$DESIGN" <<'PY'
import csv
import random
import sys

seed = int(sys.argv[1])
cases = [
    # name, bytes, path rate, RTT, loss, netem seed, jitter, duplicate, reorder, queue, send rate
    ("loss3", 16_777_216, 100, 100, 3.0, 5101, 0, 0, 0, 10_000, 100),
    ("jitter", 16_777_216, 100, 100, 0.3, 5102, 20, 0, 0, 10_000, 100),
    ("duplicate", 16_777_216, 100, 100, 0.3, 5103, 0, 5, 0, 10_000, 100),
    ("reorder", 16_777_216, 100, 100, 0.3, 5104, 0, 0, 10, 10_000, 100),
    ("combined", 16_777_216, 100, 100, 3.0, 5105, 20, 2, 10, 10_000, 100),
    ("oversubscribed", 16_777_216, 100, 20, 0, 5106, 0, 0, 0, 128, 125),
]
rng = random.Random(seed)
writer = csv.writer(sys.stdout, delimiter="\t", lineterminator="\n")
writer.writerow(
    (
        "block",
        "order",
        "case",
        "file_bytes",
        "rate_mbit",
        "rtt_ms",
        "loss_percent",
        "seed",
        "jitter_ms",
        "duplicate_percent",
        "reorder_percent",
        "queue_packets",
        "send_rate_mbit",
    )
)
for block in (1, 2):
    order = cases.copy()
    rng.shuffle(order)
    for position, case in enumerate(order, 1):
        writer.writerow((block, position, *case))
PY

current_block=0
while IFS=$'\t' read -r BLOCK ORDER CASE FILE_BYTES RATE_MBIT RTT_MS LOSS_PERCENT SEED JITTER DUPLICATE REORDER QUEUE SEND_RATE; do
  [[ $BLOCK == block ]] && continue
  if [[ $BLOCK != "$current_block" ]]; then
    python3 "$ROOT/lab/wait-idle.py"
    current_block=$BLOCK
  fi
  python3 "$ROOT/lab/wait-idle.py" --samples 2 --interval 5
  BLOCK_ID="stress-b$BLOCK-$CASE-s$SEED"
  echo "stress block=$BLOCK order=$ORDER case=$CASE" >&2
  TSU_BLOCK_ID=$BLOCK_ID \
  TSU_BLOCK_ORDER=$BLOCK \
  TSU_TREATMENT_ORDER=$ORDER \
  TSU_RANDOMIZATION_SEED=$SCHEDULE_SEED \
  TSU_EXPECTED_TREATMENTS=udp \
  TSU_QUEUE_PACKETS=$QUEUE \
  TSU_SEND_RATE_MBIT=$SEND_RATE \
  TSU_FORWARD_JITTER_MS=$JITTER \
  TSU_FORWARD_DUPLICATE_PERCENT=$DUPLICATE \
  TSU_FORWARD_REORDER_PERCENT=$REORDER \
    "$ROOT/lab/run-one.sh" \
      udp "$FILE_BYTES" "$RATE_MBIT" "$RTT_MS" "$LOSS_PERCENT" "$SEED"
  cp "$WORK_RESULTS/result-$BLOCK_ID-udp.json" "$OUTPUT_DIR/"
done <"$DESIGN"

jq -s '{
  verified: all(.[]; .verified == true),
  runs: length,
  cases: (group_by(.design.block_id | split("-")[2]) | map({
    case: (.[0].design.block_id | split("-")[2]),
    runs: length,
    elapsed_ms: map(.elapsed_ms),
    repairs: map(.repairs),
    forward_drops: map(.network.forward_qdisc[0].drops)
  }))
}' "$OUTPUT_DIR"/result-*.json >"$OUTPUT_DIR/summary.json"
jq -e '.verified and .runs == 12 and all(.cases[]; .runs == 2)' \
  "$OUTPUT_DIR/summary.json" >/dev/null
cat "$OUTPUT_DIR/summary.json"
trap - EXIT INT TERM
cleanup
