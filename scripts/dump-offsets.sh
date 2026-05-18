#!/bin/sh
# Dump struct field offsets for a PHP build using pahole.
#
# Usage:
#   ./scripts/dump-offsets.sh <php-version>           # build from source
#   ./scripts/dump-offsets.sh /path/to/php            # use existing binary (must have DWARF)
#
# Examples:
#   docker run --rm -v $PWD:/src --platform linux/amd64 debian:bookworm-slim \
#     /src/scripts/dump-offsets.sh 8.3.14
#
# Inside the container; on a Debian/Ubuntu host with the binary already
# present and pahole installed you can also point it at any PHP build:
#   ./scripts/dump-offsets.sh /usr/bin/php8.3
set -eu

if [ -x "$1" ] || [ -f "$1" ]; then
    BIN="$1"
else
    VER="$1"
    if command -v apt-get >/dev/null 2>&1; then
        export DEBIAN_FRONTEND=noninteractive
        apt-get update >/dev/null
        apt-get install -y curl gcc make pkg-config libxml2-dev libsqlite3-dev \
            re2c bison file pahole >/dev/null
    fi
    cd /tmp
    [ -f "php-${VER}.tar.gz" ] || curl -sLO "https://www.php.net/distributions/php-${VER}.tar.gz"
    [ -d "php-${VER}" ] || tar xzf "php-${VER}.tar.gz"
    cd "php-${VER}"
    if [ ! -f Makefile ]; then
        CFLAGS="-O0 -g" ./configure --disable-all --without-pear --disable-cgi >/dev/null
    fi
    if [ ! -x sapi/cli/php ]; then
        make -j"$(nproc)" >/dev/null 2>&1
    fi
    BIN="$(pwd)/sapi/cli/php"
fi

echo "### binary: $BIN"
echo "### pahole: $(pahole --version 2>&1 | head -1)"

# pahole resolves struct names; PHP uses an underscore-prefixed convention.
# We list both names because some types are known by their typedef.
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
