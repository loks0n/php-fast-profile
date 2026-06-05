# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- Continuous **Pyroscope** export (sidecar mode). `--pyroscope-url` makes pfp
  run continuously and push a gzipped pprof profile to a Grafana Pyroscope
  server every `--push-interval-secs` (default 10) instead of writing a file.
  Configurable via `--pyroscope-app`, repeatable `--pyroscope-label`, and auth
  via `--pyroscope-auth-token` / `--pyroscope-tenant-id`, plus arbitrary
  ingest headers via repeatable `--pyroscope-header "Name: value"`. Push failures are
  logged and never interrupt sampling. Pulls in a small synchronous HTTP
  client behind the default-on `pyroscope` feature.
- Distroless multi-arch (amd64/arm64) container image, published to
  `ghcr.io/loks0n/php-fast-profile` on each release, for running pfp as a
  profiling sidecar.

### Changed
- When the target binary's path (from its `/proc/PID/maps`) is not present in
  pfp's own mount namespace, fall back to `/proc/PID/root/<path>`. This lets a
  sidecar in a separate container resolve the profiled binary's symbols.

## [0.1.1] - 2026-06-02

### Added
- ZTS (thread-safe) attach support. pfp now resolves the per-thread
  `executor_globals` by decoding `tsrm_get_ls_cache_tcb_offset` and reading
  the target thread's TLS-base register via a brief `ptrace` attach
  (`FS_BASE` on x86_64, `TPIDR_EL0` on aarch64). Tested against the official
  `php:X.Y-zts` images for 8.0–8.4 on both architectures.

### Fixed
- Skip shared and deleted executable mappings when picking the principal
  binary, so the PHP binary is resolved correctly when the address space
  contains memfd/deleted exec regions.

## [0.1.0] - 2026-05-18

Initial release.

### Added
- Sampling profiler for PHP 8.0, 8.1, 8.2, 8.3, 8.4, and 8.5 (NTS).
  ZTS support landed post-0.1.0; see Unreleased.
- Linux x86_64 and aarch64 support.
- Single-PID attach (`-p PID`) and multi-PID auto-discovery
  (`-P pgrep`, `--cmdline`, `--this-container`) with periodic rediscovery.
- Output formats: `stacks`, `folded`, `pprof` (gzipped v3 protobuf), `top`
  (live ratatui display).
- Container-aware attach via cgroup matching.
- Stripped-binary support via `--executor-globals` / `--php-version`.
- ZTS detection (with a clear "not yet implemented" error; see
  `docs/development.md`).
- Per-version struct offsets verified via `pahole` against Sury debug builds.
- Reproducible benchmark harness comparing pfp against alternatives.
- `tui` and `pprof` cargo features (default-on); `--no-default-features`
  builds a smaller binary.

### Performance
- 2 syscalls per stack frame via batched `process_vm_readv` reads.
- `Arc<str>` interning for function/file name strings.
- mmap'd ELF for symbol resolution to avoid per-attach heap pressure.
- 256 KB worker thread stacks.
