#!/usr/bin/env bash
set -euo pipefail

if [[ $# -lt 2 || $# -gt 4 ]]; then
  echo "usage: $0 MATRIX_FILE OUTPUT_DIR [RANDOMIZATION_SEED] [TREATMENTS_CSV]" >&2
  exit 2
fi

MATRIX_FILE=$1
OUTPUT_DIR=$2
RANDOMIZATION_SEED=${3:-20260731}
TREATMENTS_CSV=${4:-udp,tcp,rsync}
ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
RUN_ONE="$ROOT/lab/run-one.sh"
WORK_RESULTS=/tmp/tsunami-udp-lab/results
DESIGN="$OUTPUT_DIR/design.tsv"

mkdir -p "$OUTPUT_DIR"
shopt -s nullglob
EXISTING_RESULTS=("$OUTPUT_DIR"/result-*.json)
if (( ${#EXISTING_RESULTS[@]} > 0 )); then
  echo "output directory already contains result JSON files: $OUTPUT_DIR" >&2
  exit 2
fi
python3 "$ROOT/lab/randomize-matrix.py" \
  "$MATRIX_FILE" "$RANDOMIZATION_SEED" "$TREATMENTS_CSV" >"$DESIGN"
python3 "$ROOT/lab/capture-provenance.py" "$OUTPUT_DIR/provenance.json"

CURRENT_BLOCK=''
handle_exit() {
  status=$?
  trap - EXIT
  if (( status != 0 )); then
    if [[ -n $CURRENT_BLOCK ]]; then
      python3 "$ROOT/lab/evaluate-block.py" \
        "$OUTPUT_DIR" "$CURRENT_BLOCK" "$TREATMENTS_CSV" "campaign interrupted"
    fi
    "$ROOT/lab/cleanup.sh"
  fi
  exit "$status"
}
trap handle_exit EXIT

while IFS=$'\t' read -r BLOCK_ORDER BLOCK_ID FILE_BYTES RATE_MBIT RTT_MS LOSS_PERCENT SEED TRANSPORT_ORDER; do
  [[ $BLOCK_ORDER == block_order ]] && continue
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
