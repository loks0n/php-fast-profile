#!/usr/bin/env bash
# Smoke test against a ZTS PHP. Runs the ZTS PHP inside the official php:X.Y-zts
# image (the only convenient source of prebuilt ZTS binaries — Sury doesn't ship
# them); pfp runs inside the same container so there's no pid-namespace plumbing
# between host and container.
#
# Usage: scripts/ci/smoke-zts.sh <pfp-binary> <php-zts-image>
set -euo pipefail

PFP=${1:?usage: smoke-zts.sh <pfp-binary> <php-zts-image>}
IMAGE=${2:?usage: smoke-zts.sh <pfp-binary> <php-zts-image>}
spin=$(cd "$(dirname "$0")" && pwd)/spin.php

docker run -d --name php-zts --cap-add=SYS_PTRACE \
  -v "$spin:/spin.php:ro" \
  -v "$(realpath "$PFP"):/pfp:ro" \
  "$IMAGE" php /spin.php
trap 'docker rm -f php-zts >/dev/null 2>&1 || true' EXIT
sleep 1

docker exec php-zts php -i 2>/dev/null | grep -i 'thread safety' | grep -qi enabled
# php is the container's PID 1 — the docker-php-entrypoint execs the command —
# and the php:*-zts image ships no pgrep (no procps).
docker exec php-zts /pfp -p 1 -d 2 -H 99 -o /tmp/out.txt
OUT=$(docker exec php-zts cat /tmp/out.txt)

echo "$OUT" | head -20
test "$(echo "$OUT" | grep -c '^0 ')" -ge 100
echo "$OUT" | grep -q 'W::a'
echo "$OUT" | grep -q 'W::b'
