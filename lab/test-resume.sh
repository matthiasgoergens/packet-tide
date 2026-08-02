#!/usr/bin/env bash
set -euo pipefail

ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
BINARY=${PACKET_TIDE_BINARY:-${TSU_BINARY:-"$ROOT/target/release/packet-tide"}}
SOURCE_COMMIT=${TSU_SOURCE_COMMIT:-unknown}
FILE_BYTES=${TSU_RESUME_TEST_BYTES:-134217728}
INTERRUPT_SECONDS=${TSU_RESUME_INTERRUPT_SECONDS:-5}
WORK_DIR=${TSU_RESUME_TEST_DIR:-$(mktemp -d /tmp/tsu-resume-test.XXXXXX)}
CONTROL_PORT=$((22000 + $$ % 15000))
UDP_PORT=$((CONTROL_PORT + 1))
SOURCE="$WORK_DIR/source.bin"
OUTPUT="$WORK_DIR/output.bin"
AUTH_KEY="$WORK_DIR/auth.key"

mkdir -p "$WORK_DIR"
python3 "$ROOT/lab/generate-data.py" "$SOURCE" "$FILE_BYTES" 2718
"$BINARY" keygen --out "$AUTH_KEY"

receiver=''
sender=''
cleanup() {
  [[ -z $sender ]] || kill "$sender" 2>/dev/null || true
  [[ -z $receiver ]] || kill "$receiver" 2>/dev/null || true
}
trap cleanup EXIT INT TERM

"$BINARY" receive \
  --listen "127.0.0.1:$CONTROL_PORT" \
  --udp "127.0.0.1:$UDP_PORT" \
  --out "$OUTPUT" --key-file "$AUTH_KEY" >"$WORK_DIR/receiver-interrupted.log" 2>&1 &
receiver=$!
sleep 0.2
"$BINARY" send \
  --connect "127.0.0.1:$CONTROL_PORT" \
  --udp-target "127.0.0.1:$UDP_PORT" \
  --file "$SOURCE" --transport udp --rate-mbps 50 \
  --key-file "$AUTH_KEY" \
  >"$WORK_DIR/sender-interrupted.json" 2>"$WORK_DIR/sender-interrupted.log" &
sender=$!
sleep "$INTERRUPT_SECONDS"
kill -9 "$receiver" 2>/dev/null || true
kill -9 "$sender" 2>/dev/null || true
wait "$receiver" 2>/dev/null || true
wait "$sender" 2>/dev/null || true
receiver=''
sender=''

test -f "$OUTPUT.part"
test -s "$OUTPUT.part.map"
python3 - "$OUTPUT.part.map" "$WORK_DIR/checkpoint.json" <<'PY'
import json
import struct
import sys
from pathlib import Path

raw = Path(sys.argv[1]).read_bytes()
assert raw[:8] == b"TSUMAP3\0"
size, chunks = struct.unpack(">QQ", raw[8:24])
words = struct.iter_unpack(">Q", raw[56:])
received = sum(word[0].bit_count() for word in words)
assert 0 < received < chunks
Path(sys.argv[2]).write_text(
    json.dumps(
        {"file_bytes": size, "chunks": chunks, "durable_chunks_after_kill": received},
        indent=2,
    )
    + "\n"
)
PY

/usr/bin/time -v -o "$WORK_DIR/receiver-resumed.time" \
  "$BINARY" receive \
    --listen "127.0.0.1:$CONTROL_PORT" \
    --udp "127.0.0.1:$UDP_PORT" \
    --out "$OUTPUT" --key-file "$AUTH_KEY" >"$WORK_DIR/receiver-resumed.log" 2>&1 &
receiver=$!
sleep 0.2
"$BINARY" send \
  --connect "127.0.0.1:$CONTROL_PORT" \
  --udp-target "127.0.0.1:$UDP_PORT" \
  --file "$SOURCE" --transport udp --rate-mbps 200 \
  --key-file "$AUTH_KEY" \
  >"$WORK_DIR/sender-resumed.json"
wait "$receiver"
receiver=''
cmp "$SOURCE" "$OUTPUT"
test ! -e "$OUTPUT.part"
test ! -e "$OUTPUT.part.map"

"$BINARY" receive \
  --listen "127.0.0.1:$CONTROL_PORT" \
  --udp "127.0.0.1:$UDP_PORT" \
  --out "$OUTPUT" --key-file "$AUTH_KEY" >"$WORK_DIR/receiver-complete-retry.log" 2>&1 &
receiver=$!
sleep 0.2
"$BINARY" send \
  --connect "127.0.0.1:$CONTROL_PORT" \
  --udp-target "127.0.0.1:$UDP_PORT" \
  --file "$SOURCE" --transport udp --rate-mbps 200 \
  --key-file "$AUTH_KEY" \
  >"$WORK_DIR/sender-complete-retry.json"
wait "$receiver"
receiver=''

python3 - \
  "$WORK_DIR/checkpoint.json" \
  "$WORK_DIR/sender-resumed.json" \
  "$WORK_DIR/sender-complete-retry.json" \
  "$WORK_DIR/receiver-resumed.time" \
  "$SOURCE_COMMIT" \
  "$(sha256sum "$BINARY" | awk '{print $1}')" \
  "$WORK_DIR/result.json" <<'PY'
import json
import math
import re
import sys
from pathlib import Path

checkpoint = json.loads(Path(sys.argv[1]).read_text())
resumed = json.loads(Path(sys.argv[2]).read_text())
complete = json.loads(Path(sys.argv[3]).read_text())
time_output = Path(sys.argv[4]).read_text()
peak_rss = int(re.search(r"Maximum resident set size \(kbytes\): (\d+)", time_output).group(1))
fresh_ip_bytes = checkpoint["file_bytes"] + checkpoint["chunks"] * (28 + 28)
assert resumed["resumed_chunks"] == checkpoint["durable_chunks_after_kill"]
assert 0 < resumed["udp_ip_bytes_offered"] < fresh_ip_bytes
assert complete["resumed_chunks"] == checkpoint["chunks"]
assert complete["datagrams"] == 0
assert complete["udp_ip_bytes_offered"] == 0
Path(sys.argv[7]).write_text(
    json.dumps(
        {
            "verified": True,
            "source_commit": sys.argv[5],
            "binary_sha256": sys.argv[6],
            "receiver_peak_rss_kib": peak_rss,
            "checkpoint": checkpoint,
            "resumed_sender": resumed,
            "complete_retry_sender": complete,
        },
        indent=2,
    )
    + "\n"
)
PY

cat "$WORK_DIR/result.json"
echo "resume test artifacts: $WORK_DIR" >&2
trap - EXIT INT TERM
