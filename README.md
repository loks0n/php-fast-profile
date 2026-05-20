# pfp — PHP Fast Profile

A modern sampling profiler for PHP 8.0+, written in Rust.

`pfp` attaches to a running PHP process and walks the Zend VM's call stack
without pausing the target. It captures ~100% of samples at 999 Hz with a
small (~3 MB) RSS footprint, and emits formats your tools already speak —
folded stacks, pprof, or a live in-terminal `top` view.

```sh
pfp -p $(pgrep -n php) -d 30 -H 999 -f folded -o stacks.txt
```

See [docs/benchmarks.md](docs/benchmarks.md) for a comparison against
existing alternatives.

## Why pfp

- **High fidelity at high sample rates.** 2 syscalls per frame, with
  `Arc<str>`-interned function/file names. At 999 Hz pfp captures 4996/5000
  samples on a 5-second window.
- **Multi-PID with no rediscovery overhead.** `pfp -P php-fpm` spawns one
  thread per worker, attaches once, persists symbol state. New workers
  picked up on a configurable rediscovery interval.
- **Container-aware.** `--this-container` scopes attach to processes in the
  current cgroup. No PID juggling between sidecar and target.
- **Live `top` mode.** `pfp -p PID -f top` opens a ratatui table showing
  per-function exclusive/inclusive percentages, updating in real time.
- **Native pprof v3 output.** `-f pprof` produces a gzipped protobuf
  consumable by `pprof.dev`, `flamegraph.com`, Grafana, and Pyroscope —
  with no intermediate stack-collapse step.
- **PHP 8.0 / 8.1 / 8.2 / 8.3 / 8.4 / 8.5.** Per-version struct offsets verified
  via `pahole` against Sury's debug builds. Drift between minors is handled
  by a single layout table.
- **Linux x86_64 and aarch64.** Same offsets, separate prologue decoders.
- **NTS and ZTS.** Both build flavours work; ZTS attach walks TSRM via a
  short ptrace read of the target thread's TLS base register.

## Install

Build from source (Rust 1.95+):

```sh
git clone https://github.com/loks0n/php-fast-profile
cd php-fast-profile
cargo build --release
# binary: target/release/pfp
```

For a smaller binary without the live TUI or pprof output:

```sh
cargo build --release --no-default-features  # ~1.8 MB instead of 2.2 MB
```

`process_vm_readv` requires `CAP_SYS_PTRACE` (or running as root, or the
target's UID with `/proc/sys/kernel/yama/ptrace_scope=0`). Inside Docker,
add `--cap-add=SYS_PTRACE`.

## Usage

```sh
# Single PID, 30 s, default 99 Hz, default stacks output to stdout.
pfp -p 1234

# All php-fpm workers, 60 s at 999 Hz, folded stacks for FlameGraph.
pfp -P php-fpm -d 60 -H 999 -f folded -o profile.folded

# Just the workers in this container; pprof for Pyroscope/Grafana ingest.
pfp -P php-fpm --this-container -d 60 -H 99 -f pprof -o profile.pb.gz

# Live top view.
pfp -P php-fpm -f top
```

### CLI flags

| Flag | Description |
|------|-------------|
| `-p, --pid <PID>` | Attach to a single process |
| `-P, --pgrep <STR>` | Attach to all processes whose `comm` contains STR |
| `--cmdline <STR>` | Match against `cmdline` instead of `comm` |
| `--this-container` | Restrict discovery to this profiler's cgroup |
| `--rediscover-secs <N>` | Re-scan for new PIDs every N seconds (default 5) |
| `-H, --rate-hz <N>` | Sampling rate (default 99) |
| `-d, --duration-secs <N>` | Stop after N seconds (0 = forever) |
| `-s, --max-depth <N>` | Cap stack depth (default 256) |
| `-f, --format <FMT>` | `stacks`, `folded`, `pprof`, or `top` |
| `-o, --output <PATH>` | Output file (default stdout) |
| `--request-info` | Capture `$_SERVER` URI/method per sample |
| `--php-version <V>` | Force version (e.g. `8.4`) on stripped binaries |
| `--executor-globals <ADDR>` | Override EG address on stripped binaries |

## How it works

1. Find the target's principal binary via `/proc/PID/maps` (no reliance on
   `/proc/PID/exe`, which lies under Rosetta and after upgrades).
2. Resolve `executor_globals` and `php_version` from the ELF symbol tables
   using the `object` crate. Fall back to `--executor-globals` / `--php-version`
   for fully stripped binaries.
3. Decode the `php_version` accessor in the target's text segment to find
   the version string. (x86_64: `[endbr64] lea rip+disp32, ret`; arm64:
   `[bti c] adrp + add + ret`.)
4. Pick the matching struct-offset layout for that PHP minor version.
5. Read each frame with two bulk `process_vm_readv` calls (the
   `zend_execute_data` and the `zend_function` header), then look up
   `zend_string` data through an `Arc<str>` cache.
6. Format and write samples through the chosen sink (or aggregate them in
   the live TUI).

Multi-PID mode spawns one thread per discovered PID; samples flow into a
single `mpsc` and are written by the main thread to keep output ordered.
A discovery thread re-runs every `--rediscover-secs` seconds to pick up
new fpm workers.

## Status

- [x] PHP 8.0, 8.1, 8.2, 8.3, 8.4, 8.5 — NTS and ZTS, offsets verified via
      `offsetof()` against the official `php:X.Y[-zts]` images
- [x] Linux x86_64 + aarch64
- [x] Single-PID + multi-PID + auto-discovery
- [x] `stacks` / `folded` / `pprof` / `top` output
- [x] Container-aware attach
- [x] Stripped-binary support (with manual EG override)
- [ ] macOS (requires Apple-signed binary; tracked as future work)
- [ ] Pyroscope / OTLP push mode (planned as a sidecar binary)

## Documentation

- [docs/development.md](docs/development.md) — building, regenerating
  struct offsets via `pahole`, ZTS notes
- [docs/benchmarks.md](docs/benchmarks.md) — methodology, full results
  vs alternatives, caveats

## License

MIT.
