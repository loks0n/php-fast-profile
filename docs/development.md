# Development

## Building

`pfp` only links and runs on Linux (it uses `process_vm_readv` and `/proc`).
On a non-Linux dev host, cross-check builds with:

```sh
cargo check --target x86_64-unknown-linux-gnu
```

Or run the full toolchain inside Docker:

```sh
docker run --rm -v "$PWD":/src --platform linux/amd64 -w /src \
  rust:1.75-bookworm \
  sh -c 'apt-get update && apt-get install -y protobuf-compiler && cargo build --release'
```

The `protobuf-compiler` package is required because the build script invokes
`protoc` on `proto/profile.proto` (pprof v3 schema).

## Verifying / regenerating struct offsets

`src/offsets.rs` contains a `VersionLayout` per supported PHP minor version
(8.3, 8.4, 8.5). Most field offsets are stable across versions, but
`zend_executor_globals` regularly grows new fields, which shifts the offsets
for `current_execute_data` and `symbol_table` between minors.

Whenever a new PHP minor releases — or whenever someone reports an attach
failure on an unusual build — re-run the verification:

### From a release tarball

```sh
docker run --rm \
  -v "$PWD":/src \
  --platform linux/amd64 \
  debian:bookworm-slim \
  /src/scripts/dump-offsets.sh 8.5.0
```

This downloads php-8.5.0, builds it with `-O0 -g`, then runs `pahole` to
print every struct field offset we care about. Update `LAYOUT_8_5` in
`src/offsets.rs` if any of the EG offsets have changed.

The expected runtime under x86_64 emulation on Apple Silicon is ~10 minutes
per version (configure + `make -j` of a minimal PHP build).

### From an existing binary

If you already have a PHP binary with DWARF debug info (e.g. via
`apt-get install php8.3-dbg` on Debian), skip the build:

```sh
./scripts/dump-offsets.sh /usr/lib/debug/.build-id/.../php8.3
```

### What to copy into `offsets.rs`

For each struct, read the `/* offset size */` comment in the pahole output
and update the matching `pub const` or `VersionLayout` field.

The fields the profiler actually reads:

| Struct                    | Field                  | Where in code                         |
| ------------------------- | ---------------------- | ------------------------------------- |
| `zend_executor_globals`   | `current_execute_data` | `VersionLayout.eg_current_execute_data` |
| `zend_executor_globals`   | `symbol_table`         | `VersionLayout.eg_symbol_table`       |
| `zend_execute_data`       | `opline`               | `VersionLayout.ex_opline`             |
| `zend_execute_data`       | `func`                 | `VersionLayout.ex_func`               |
| `zend_execute_data`       | `prev_execute_data`    | `VersionLayout.ex_prev_execute_data`  |
| `zend_execute_data`       | `symbol_table`         | `VersionLayout.ex_symbol_table`       |
| `zend_op`                 | `lineno`               | `offsets::op::LINENO`                 |
| `zend_string`             | `len`, `val`           | `offsets::zstr::*`                    |
| `zend_class_entry`        | `name`                 | `offsets::ce::NAME`                   |
| `_zend_array`             | `nNumUsed`, `arData`   | `offsets::ht::*`                      |
| `_Bucket`                 | `val`, `h`, `key`      | `offsets::bucket::*`                  |
| `_zval_struct`            | `value`, `u1`          | `offsets::zval::*`                    |
| `zend_op_array`           | `function_name`, `scope`, `filename`, `line_start`, `line_end` | `offsets::func::*` |

### Runtime overrides

If a user reports an unusual build (custom patches, exotic configure flags),
they can override the most-likely-to-shift offsets at runtime without
recompiling:

```sh
PFP_EG_CURRENT_EXECUTE_DATA=512 \
PFP_EG_SYMBOL_TABLE=1296 \
pfp -p 1234
```

## Stripped binaries

Most distros ship PHP with the `.symtab`/DWARF stripped but `.dynsym` intact —
that's enough: `executor_globals` and `php_version` are exported, so `pfp`
finds them automatically. Verify:

```sh
nm -D /usr/bin/php8.3 | grep -E 'executor_globals|php_version'
```

If a build is **fully** stripped (no `.dynsym` exports), supply the addresses
yourself:

```sh
# Find executor_globals via /proc/PID/maps + the binary's section addresses,
# or by attaching gdb to a running process; e.g.:
gdb -batch -p $PID -ex 'p &executor_globals' -ex quit

pfp -p $PID \
  --executor-globals 0x598fa0 \
  --php-version 8.3
```

`--executor-globals` accepts either an absolute runtime address or the
ELF-relative one (anything below the load base from `/proc/PID/maps` is
treated as relative and rebiased automatically).

`--php-version` is required when neither `php_version` nor any version string
is reachable, since `pfp` selects struct offsets from the version.

## ZTS (thread-safe) support

`pfp` currently supports **NTS** (non-thread-safe) PHP builds only — by far
the common case for CLI / FPM. ZTS detection works (we recognize the build
and produce a clear error), but per-thread `executor_globals` resolution is
not yet implemented.

If you need ZTS attach, the path is:

1. Build a ZTS PHP from source with debug symbols:
   ```sh
   ./configure --enable-zts CFLAGS="-O0 -g"
   make -j
   ```
2. Verify struct offsets (some shift between NTS and ZTS due to TSRM):
   ```sh
   ./scripts/dump-offsets.sh /path/to/zts/sapi/cli/php
   ```
3. Walk TSRM to find the calling thread's `executor_globals`:
   - Read `tsrm_get_ls_cache` (a function returning `void**`).
   - Disassemble its prologue to extract the TLS access pattern; or read the
     thread-local cache directly via `/proc/PID/task/TID/...`.
   - At offset `executor_globals_offset` in the cache, read the per-thread
     EG pointer.

PRs welcome — file an issue with your `pahole` output to seed the layout.

## Testing against a live PHP

```sh
docker run --rm -it --cap-add SYS_PTRACE --pid=host php:8.3-cli \
  php -r 'while (true) { usleep(1000); }' &
PID=$(pgrep -n php)
target/release/pfp -p "$PID" -d 5
```

`SYS_PTRACE` (or running as root, or matching uid + `/proc/sys/kernel/yama/ptrace_scope=0`)
is required for `process_vm_readv`.
