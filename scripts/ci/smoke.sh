#!/usr/bin/env bash
# Smoke test against a live NTS PHP 8.3: profile a busy loop and assert the
# named frames show up.
#
# Usage: scripts/ci/smoke.sh <pfp-binary>
set -euo pipefail

PFP=${1:?usage: smoke.sh <pfp-binary>}
here=$(cd "$(dirname "$0")" && pwd)

# Same-UID process_vm_readv needs ptrace_scope relaxed on the runner.
echo 0 | sudo tee /proc/sys/kernel/yama/ptrace_scope >/dev/null || true

php8.3 "$here/spin.php" &
PID=$!
trap 'kill "$PID" 2>/dev/null || true' EXIT
sleep 1

"$PFP" -p "$PID" -d 2 -H 99 -o /tmp/out.txt

test "$(grep -c '^0 ' /tmp/out.txt)" -ge 100
grep -q 'W::a' /tmp/out.txt
grep -q 'W::b' /tmp/out.txt
