#!/usr/bin/env bash
set -euo pipefail

binary=${1:-target/debug/packet-tide}
work=$(mktemp -d "${TMPDIR:-/tmp}/packet-tide-directory.XXXXXX")
receiver=
sender=

cleanup() {
  status=$?
  for pid in "$sender" "$receiver"; do
    if [[ -n $pid ]]; then
      kill -KILL "$pid" 2>/dev/null || true
      wait "$pid" 2>/dev/null || true
    fi
  done
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
mkdir -p "$work/source/a/b" "$work/source/empty-dir"
printf 'alpha\n' >"$work/source/a/alpha"
printf 'beta\n' >"$work/source/a/b/beta"
: >"$work/source/empty-file"
chmod 0750 "$work/source" "$work/source/a" "$work/source/a/b"
chmod 0700 "$work/source/empty-dir"
chmod 0640 "$work/source/a/alpha" "$work/source/a/b/beta"
chmod 0600 "$work/source/empty-file"
python3 - "$work/source" <<'PY'
import os
import pathlib
import sys

root = pathlib.Path(sys.argv[1])
for index, path in enumerate(sorted([root, *root.rglob("*")])):
    timestamp = 1_600_000_000_000_000_000 + index * 123_456_789
    os.utime(path, ns=(timestamp, timestamp), follow_symlinks=False)
PY

base_port=$((30000 + ($$ % 10000)))
"$binary" receive-dir \
  --listen "127.0.0.1:$base_port" \
  --data-listen "127.0.0.1:$((base_port + 1))" \
  --udp "127.0.0.1:$((base_port + 2))" \
  --out "$work/output" --key-file "$work/key" \
  >"$work/receiver.json" 2>"$work/receiver.err" &
receiver=$!
sleep 0.2
"$binary" send-dir \
  --connect "127.0.0.1:$base_port" \
  --data-connect "127.0.0.1:$((base_port + 1))" \
  --udp-target "127.0.0.1:$((base_port + 2))" \
  --root "$work/source" --rate-mbps 50 --key-file "$work/key" \
  >"$work/sender.json"
wait "$receiver"
receiver=

python3 - "$work/source" "$work/output" "$work/sender.json" "$work/receiver.json" <<'PY'
import hashlib
import json
import os
import pathlib
import stat
import sys

source, output = map(pathlib.Path, sys.argv[1:3])
for relative in [pathlib.Path("."), *sorted(path.relative_to(source) for path in source.rglob("*"))]:
    left = source / relative
    right = output / relative
    assert right.exists()
    left_stat = left.stat()
    right_stat = right.stat()
    assert stat.S_IMODE(left_stat.st_mode) == stat.S_IMODE(right_stat.st_mode), relative
    assert left_stat.st_mtime_ns == right_stat.st_mtime_ns, relative
    if left.is_file():
        assert hashlib.sha256(left.read_bytes()).digest() == hashlib.sha256(right.read_bytes()).digest()

def summary(path, role):
    records = [json.loads(line) for line in open(path, encoding="utf-8") if line.strip()]
    return next(record for record in records if record.get("role") == role)

sender = summary(sys.argv[3], "directory-sender")
receiver = summary(sys.argv[4], "directory-receiver")
assert sender["manifest_sha256"] == receiver["manifest_sha256"]
assert sender["entries"] == receiver["entries"] == 6
assert sender["files"] == receiver["files"] == 3
PY

# A stopped multi-file session leaves a manifest-bound UDP receipt map. The same
# manifest resumes it; no partially populated destination tree becomes visible.
mkdir "$work/resume-source"
dd if=/dev/zero of="$work/resume-source/large" bs=1M count=8 status=none
base_port=$((base_port + 10))
"$binary" receive-dir \
  --listen "127.0.0.1:$base_port" \
  --data-listen "127.0.0.1:$((base_port + 1))" \
  --udp "127.0.0.1:$((base_port + 2))" \
  --out "$work/resumed-output" --key-file "$work/key" \
  >"$work/interrupted-receiver.json" 2>"$work/interrupted-receiver.err" &
receiver=$!
sleep 0.2
"$binary" send-dir \
  --connect "127.0.0.1:$base_port" \
  --data-connect "127.0.0.1:$((base_port + 1))" \
  --udp-target "127.0.0.1:$((base_port + 2))" \
  --root "$work/resume-source" --rate-mbps 1 --key-file "$work/key" \
  >"$work/interrupted-sender.json" 2>"$work/interrupted-sender.err" &
sender=$!
sleep 2
kill -KILL "$sender" "$receiver" 2>/dev/null || true
wait "$sender" 2>/dev/null || true
wait "$receiver" 2>/dev/null || true
sender=
receiver=
test ! -e "$work/resumed-output"

"$binary" receive-dir \
  --listen "127.0.0.1:$base_port" \
  --data-listen "127.0.0.1:$((base_port + 1))" \
  --udp "127.0.0.1:$((base_port + 2))" \
  --out "$work/resumed-output" --key-file "$work/key" \
  >"$work/resumed-receiver.json" 2>"$work/resumed-receiver.err" &
receiver=$!
sleep 0.2
"$binary" send-dir \
  --connect "127.0.0.1:$base_port" \
  --data-connect "127.0.0.1:$((base_port + 1))" \
  --udp-target "127.0.0.1:$((base_port + 2))" \
  --root "$work/resume-source" --rate-mbps 50 --key-file "$work/key" \
  >"$work/resumed-sender.json"
wait "$receiver"
receiver=
cmp "$work/resume-source/large" "$work/resumed-output/large"
python3 - "$work/resumed-sender.json" <<'PY'
import json
import sys

records = [json.loads(line) for line in open(sys.argv[1], encoding="utf-8") if line.strip()]
file_result = next(record for record in records if record.get("role") == "sender")
assert file_result["resumed_chunks"] > 0
assert records[-1]["role"] == "directory-sender"
PY

echo "authenticated directory transfer and resume checks passed"
