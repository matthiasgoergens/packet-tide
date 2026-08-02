#!/usr/bin/env bash
set -euo pipefail

binary=${1:-target/debug/packet-tide}
work=$(mktemp -d "${TMPDIR:-/tmp}/packet-tide-reuse.XXXXXX")
receiver=

cleanup() {
  status=$?
  if [[ -n $receiver ]]; then
    kill -KILL "$receiver" 2>/dev/null || true
    wait "$receiver" 2>/dev/null || true
  fi
  if (( status != 0 )); then
    for log in "$work"/*.json "$work"/*.err; do
      [[ -f $log ]] || continue
      printf '==> %s <==\n' "$log" >&2
      sed -n '1,200p' "$log" >&2
    done
  fi
  rm -rf "$work"
  exit "$status"
}
trap cleanup EXIT INT TERM

"$binary" keygen --out "$work/key"
python3 "$(dirname "$0")/generate-data.py" "$work/original" 4194304 8675309
cp "$work/original" "$work/unchanged"
cp "$work/original" "$work/edited"
cp "$work/original" "$work/truncated"
python3 "$(dirname "$0")/generate-data.py" "$work/wrong" 4194304 424242
python3 - "$work/original" "$work/inserted" "$work/edited" "$work/truncated" <<'PY'
import pathlib
import sys

original, inserted, edited, truncated = map(pathlib.Path, sys.argv[1:])
data = original.read_bytes()
inserted.write_bytes(data[:1_000_000] + b"packet-tide insertion\n" * 100 + data[1_000_000:])
with edited.open("r+b") as stream:
    stream.seek(2_000_000)
    stream.write(b"a bounded local edit" * 100)
with truncated.open("r+b") as stream:
    stream.truncate(3_500_000)
PY

base_port=$((32000 + ($$ % 8000)))
run_transfer() {
  label=$1
  source=$2
  candidate=$3
  enable_reuse=$4
  output="$work/output-$label"
  port=$base_port
  base_port=$((base_port + 2))
  receive_extra=()
  send_extra=()
  if [[ $enable_reuse == true ]]; then
    receive_extra=(--reuse-from "$candidate")
    send_extra=(--reuse-chunks true)
  fi
  "$binary" receive \
    --listen "127.0.0.1:$port" --udp "127.0.0.1:$((port + 1))" \
    --out "$output" --key-file "$work/key" "${receive_extra[@]}" \
    >"$work/receiver-$label.json" 2>"$work/receiver-$label.err" &
  receiver=$!
  sleep 0.2
  "$binary" send \
    --connect "127.0.0.1:$port" --udp-target "127.0.0.1:$((port + 1))" \
    --file "$source" --transport udp --rate-mbps 200 \
    --key-file "$work/key" "${send_extra[@]}" \
    >"$work/sender-$label.json"
  wait "$receiver"
  receiver=
  cmp "$source" "$output"
}

run_transfer unchanged "$work/unchanged" "$work/original" true
run_transfer edited "$work/edited" "$work/original" true
run_transfer inserted "$work/inserted" "$work/original" true
run_transfer truncated "$work/truncated" "$work/original" true
run_transfer wrong "$work/edited" "$work/wrong" true
run_transfer fallback "$work/edited" "$work/original" false

python3 - "$work" <<'PY'
import json
import pathlib
import sys

work = pathlib.Path(sys.argv[1])

def sender(label):
    records = [json.loads(line) for line in (work / f"sender-{label}.json").read_text().splitlines()]
    return next(record for record in records if record.get("role") == "sender")

for label in ("unchanged", "edited", "inserted", "truncated"):
    result = sender(label)
    assert result["reused_bytes"] > 0, (label, result)
    assert result["reused_chunks"] > 0, (label, result)
    assert result["udp_ip_bytes_offered"] < result["bytes"], (label, result)

unchanged = sender("unchanged")
assert unchanged["reused_bytes"] == unchanged["bytes"]
assert unchanged["datagrams"] == 0

wrong = sender("wrong")
assert wrong["reused_bytes"] == wrong["reused_chunks"] == 0
assert wrong["datagrams"] > 0

fallback = sender("fallback")
assert fallback["reused_bytes"] == fallback["reused_chunks"] == 0
assert fallback["datagrams"] > 0
PY

echo "content-defined reuse and full-transfer fallback checks passed"
