#!/usr/bin/env bash
# End-to-end test of continuous Pyroscope export: run pfp in push mode against a
# live PHP and a Pyroscope server.
#
# Usage: scripts/ci/smoke-pyroscope.sh <pfp-binary> [pyroscope-url]
set -euo pipefail

PFP=${1:?usage: smoke-pyroscope.sh <pfp-binary> [pyroscope-url]}
URL=${2:-http://localhost:4040}
here=$(cd "$(dirname "$0")" && pwd)

echo 0 | sudo tee /proc/sys/kernel/yama/ptrace_scope >/dev/null || true

php8.3 "$here/spin.php" &
PHP_PID=$!
# Run pfp continuously in sidecar/push mode; keep pushing while we poll.
RUST_LOG=pfp=debug "$PFP" -p "$PHP_PID" --pyroscope-url "$URL" \
  --pyroscope-app pfp-smoke --push-interval-secs 2 -H 99 \
  > /tmp/pfp.log 2>&1 &
PFP_PID=$!
trap 'kill "$PFP_PID" "$PHP_PID" 2>/dev/null || true' EXIT

# 1) Hard gate: pfp attached and pushed without errors. Pyroscope answers
#    /ingest with 422 on a malformed pprof (which the sink logs as "push
#    failed"), so a clean push proves the profile was parsed and accepted
#    end-to-end by the server.
sleep 12
cat /tmp/pfp.log
grep -q 'attached pid=' /tmp/pfp.log
grep -q 'pushed profile to pyroscope' /tmp/pfp.log
if grep -q 'push failed' /tmp/pfp.log; then
  echo "pfp reported push failures" >&2
  exit 1
fi

# 2) Best-effort: confirm the profile is queryable with our PHP frames.
#    Pyroscope's ingest->queryable lag is timing-dependent, so this is logged
#    but does not gate the job (the push gate above is the deterministic
#    e2e signal).
for _ in $(seq 1 30); do
  now=$(date +%s%3N)
  from=$((now - 600000))
  body=$(printf '{"profile_typeID":"process_cpu:samples:count::","label_selector":"{service_name=\\"pfp-smoke\\"}","start":%s,"end":%s}' "$from" "$now")
  names=$(curl -s -X POST "$URL/querier.v1.QuerierService/SelectMergeStacktraces" \
    -H 'Content-Type: application/json' -d "$body" \
    | python3 -c 'import sys,json; print("\n".join(json.load(sys.stdin).get("flamegraph",{}).get("names",[])))' 2>/dev/null || true)
  if echo "$names" | grep -q 'W::a' && echo "$names" | grep -q 'W::b'; then
    echo "queried PHP frames back from pyroscope:"
    echo "$names"
    break
  fi
  sleep 3
done
