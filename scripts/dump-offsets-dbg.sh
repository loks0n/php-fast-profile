#!/bin/sh
# Dump struct offsets from a Sury-packaged PHP build (with debug symbols).
#
# Far faster than building from source: Sury ships prebuilt binaries for
# every supported PHP minor (8.0 .. 8.5) with matching `-dbgsym` packages,
# for both x86_64 and aarch64.
#
# Usage (inside container, native arch):
#   docker run --rm -v "$PWD":/src debian:bookworm-slim \
#     /src/scripts/dump-offsets-dbg.sh 8.3
#
# To cross-check on a different arch, add --platform linux/amd64 etc.
set -eu

VER="$1"

export DEBIAN_FRONTEND=noninteractive
apt-get update >/dev/null
apt-get install -y curl ca-certificates gnupg lsb-release apt-transport-https \
    pahole >/dev/null

curl -sSLo /usr/share/keyrings/deb.sury.org-php.gpg \
    https://packages.sury.org/php/apt.gpg
. /etc/os-release
echo "deb [signed-by=/usr/share/keyrings/deb.sury.org-php.gpg] \
https://packages.sury.org/php/ ${VERSION_CODENAME} main" \
    > /etc/apt/sources.list.d/sury-php.list

apt-get update >/dev/null
apt-get install -y --no-install-recommends \
    "php${VER}-cli" "php${VER}-cli-dbgsym" >/dev/null

BIN="/usr/bin/php${VER}"
echo "### binary:    $BIN"
echo "### version:   $($BIN --version | head -1)"
echo "### pahole:    $(pahole --version 2>&1 | head -1)"

for s in \
    _zend_executor_globals \
    _zend_execute_data \
    _zend_op_array \
    _zend_op \
    _zend_string \
    _zend_class_entry \
    _zend_array \
    _Bucket \
    _zval_struct \
; do
    echo
    echo "###--- $s ---###"
    pahole -C "$s" "$BIN" 2>/dev/null || echo "(not found: $s)"
done
