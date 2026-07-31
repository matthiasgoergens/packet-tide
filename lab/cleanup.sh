#!/usr/bin/env bash
set -euo pipefail

for namespace in tsu-bench-s tsu-bench-r tsu-bench-d; do
  ip netns delete "$namespace" 2>/dev/null || true
done
