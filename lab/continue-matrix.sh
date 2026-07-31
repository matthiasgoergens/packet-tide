#!/usr/bin/env bash
set -euo pipefail

if [[ $# -lt 2 || $# -gt 3 ]]; then
  echo "usage: $0 OUTPUT_DIR RANDOMIZATION_SEED [TREATMENTS_CSV]" >&2
  exit 2
fi

OUTPUT_DIR=$1
RANDOMIZATION_SEED=$2
TREATMENTS_CSV=${3:-udp,tcp-cubic,tcp-bbr,tcp4-cubic}
ROOT=${TSU_PROJECT_ROOT:-$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)}
RUN_ONE="$ROOT/lab/run-one.sh"
WORK_RESULTS=/tmp/tsunami-udp-lab/results
DESIGN="$OUTPUT_DIR/design.tsv"
QUALITY="$OUTPUT_DIR/block-quality.jsonl"

[[ -f $DESIGN ]] || { echo "missing realized design: $DESIGN" >&2; exit 2; }
[[ -f $OUTPUT_DIR/provenance.json ]] || {
  echo "missing original provenance: $OUTPUT_DIR/provenance.json" >&2
  exit 2
}
touch "$QUALITY"

CURRENT_BLOCK=''
handle_exit() {
  status=$?
  trap - EXIT
  if (( status != 0 )); then
    if [[ -n $CURRENT_BLOCK ]]; then
      python3 "$ROOT/lab/evaluate-block.py" \
        "$OUTPUT_DIR" "$CURRENT_BLOCK" "$TREATMENTS_CSV" \
        "continued campaign interrupted"
    fi
    "$ROOT/lab/cleanup.sh"
  fi
  exit "$status"
}
trap handle_exit EXIT

while IFS=$'\t' read -r BLOCK_ORDER BLOCK_ID FILE_BYTES RATE_MBIT RTT_MS LOSS_PERCENT SEED TRANSPORT_ORDER; do
  [[ $BLOCK_ORDER == block_order ]] && continue

  if jq -e --arg block_id "$BLOCK_ID" \
      'select(.block_id == $block_id)' "$QUALITY" >/dev/null; then
    echo "skip recorded block=$BLOCK_ID" >&2
    continue
  fi

  mapfile -t EXISTING < <(
    jq -r --arg block_id "$BLOCK_ID" \
      'select(.design.block_id == $block_id) | .transport' \
      "$OUTPUT_DIR"/result-*.json 2>/dev/null || true
  )
  if (( ${#EXISTING[@]} > 0 )); then
    echo "unassessed partial block=$BLOCK_ID has results: ${EXISTING[*]}" >&2
    exit 2
  fi

  CURRENT_BLOCK=$BLOCK_ID
  python3 "$ROOT/lab/wait-idle.py"
  IFS=',' read -r -a TRANSPORTS <<<"$TRANSPORT_ORDER"
  TREATMENT_ORDER=0
  for TRANSPORT in "${TRANSPORTS[@]}"; do
    python3 "$ROOT/lab/wait-idle.py" --samples 2 --interval 5
    TREATMENT_ORDER=$((TREATMENT_ORDER + 1))
    echo "run transport=$TRANSPORT bytes=$FILE_BYTES rate=$RATE_MBIT rtt=$RTT_MS loss=$LOSS_PERCENT seed=$SEED" >&2
    TSU_BLOCK_ID=$BLOCK_ID \
    TSU_BLOCK_ORDER=$BLOCK_ORDER \
    TSU_TREATMENT_ORDER=$TREATMENT_ORDER \
    TSU_RANDOMIZATION_SEED=$RANDOMIZATION_SEED \
    TSU_EXPECTED_TREATMENTS=$TREATMENTS_CSV \
      "$RUN_ONE" "$TRANSPORT" "$FILE_BYTES" "$RATE_MBIT" "$RTT_MS" "$LOSS_PERCENT" "$SEED"
    cp "$WORK_RESULTS/result-$BLOCK_ID-$TRANSPORT.json" "$OUTPUT_DIR/"
  done
  python3 "$ROOT/lab/evaluate-block.py" \
    "$OUTPUT_DIR" "$BLOCK_ID" "$TREATMENTS_CSV"
  CURRENT_BLOCK=''
done <"$DESIGN"

python3 "$ROOT/lab/summarize.py" "$OUTPUT_DIR"
python3 "$ROOT/lab/analyze-rbd.py" "$OUTPUT_DIR"
"$ROOT/lab/cleanup.sh"
trap - EXIT
