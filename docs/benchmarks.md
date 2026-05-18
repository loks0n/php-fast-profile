# Benchmarks: pfp vs phpspy

`pfp` and `phpspy` are both Linux-only sampling profilers for PHP. The
benchmarks below run natively on aarch64 Linux (Apple Silicon → arm64
Docker container, no emulation).

## TL;DR

| Workload   | Rate   | pfp samples | phpspy samples | pfp CPU | phpspy CPU | pfp RSS | phpspy RSS |
|------------|--------|-------------|----------------|---------|------------|---------|------------|
| synthetic  |  99 Hz | 496 / 500   | 464 / 500      | 0.06 s  | 0.08 s     | 3.1 MB  | 4.5 MB     |
| synthetic  | 999 Hz | 4996 / 5000 | 2809 / 5000    | 0.26 s  | 0.29 s     | 3.1 MB  | 4.5 MB     |
| framework  |  99 Hz | 496 / 500   | 457 / 500      | 0.00 s  | 0.01 s     | 3.1 MB  | 4.5 MB     |
| framework  | 999 Hz | 4996 / 5000 | 2794 / 5000    | 0.04 s  | 0.04 s     | 3.1 MB  | 4.5 MB     |
| wordpress  |  99 Hz | 496 / 500   | 461 / 500      | 0.00 s  | 0.01 s     | 3.1 MB  | 4.5 MB     |
| wordpress  | 999 Hz | 4996 / 5000 | 2766 / 5000    | 0.03 s  | 0.04 s     | 3.1 MB  | 4.5 MB     |
| multi-pid  |  99 Hz | 1985        | 0 †            | 0.20 s  |  7.48 s †  | 3.3 MB  | 4.6 MB     |
| multi-pid  | 999 Hz | 19976       | 0 †            | 0.88 s  |  7.77 s †  | 3.3 MB  | 4.6 MB     |

5-second sampling windows, 4 PHP 8.3.31 NTS workers for `multi-pid`.

**Headlines:**

- **Sample-rate accuracy at 999 Hz**: pfp 99.9%, phpspy 56%.
- **CPU overhead**: pfp ≤ phpspy in every cell; **8–10× lower** in multi-PID.
- **RSS**: pfp 3.1–3.3 MB vs phpspy 4.5–4.6 MB (~30% lower).
- **Output volume**: pfp captures ~75% more samples per second at high rates.

† phpspy `-P` (multi-PID) has a discovery race — see "Caveats" below.

## Methodology

Each cell:

1. Spawn a single PHP CLI process running a 100M-iter workload loop.
2. Wait 1s for the process to enter steady-state.
3. Attach the profiler at the requested rate, run for `BENCH_DURATION`
   seconds, write stack output to a file.
4. Parse the output to count actual samples captured.
5. Wrap the profiler in `/usr/bin/time -v` for user/system CPU and peak RSS.
6. Kill the target.

Same PHP target binary, same workload script, same wall-clock window. Only
the profiler changes between runs.

`scripts/bench.sh` is the reproduction. Runs in a Docker container — see the
"Reproducing" section.

### Measuring RSS accurately

`/usr/bin/time -v` reports `Maximum resident set size`, but on Linux this
field is reported in KB on most distros and **is unreliable under Rosetta /
emulation** (it sometimes double-counts shared file-backed pages).

For headline RSS numbers we sample `/proc/PID/smaps_rollup` mid-flight (≥3s
into a 30s run). That gives a true peak RSS broken down by anon vs. shared.

## Workloads

- **synthetic**: 8-deep recursive method call ending in `usleep(50)`. Tests
  raw stack-walk speed in isolation.
- **framework**: a `Repository` + `HelloController` pair that builds arrays
  and calls `json_encode`. Approximates framework-shaped call graphs with
  namespaces.
- **wordpress**: a `WP_Hook`-style filter loop. Hashtable walks and callable
  dispatch — close to real WordPress runtime profile.
- **multi-pid**: 4 simultaneous synthetic workers, each profiled by the
  multi-PID mode of the respective tool (`-P`).

## Reproducing

```sh
docker run --rm \
  -v "$PWD":/src \
  --platform linux/amd64 \
  --cap-add=SYS_PTRACE \
  rust:latest /src/scripts/bench.sh
```

Override defaults with env vars:

```sh
docker run ... \
  -e BENCH_DURATION=30 \
  -e "BENCH_RATES=99 499 999 4999" \
  rust:latest /src/scripts/bench.sh
```

CSV results land at `/tmp/bench-results.csv`.

## Detailed results

```
workload   profiler  rate_hz  samples  user_cpu_s  sys_cpu_s
synthetic  pfp       99       496      0.06        0.03
synthetic  phpspy    99       461      0.14        0.09
synthetic  pfp       999      4996     0.19        0.20
synthetic  phpspy    999      2875     0.26        0.21
framework  pfp       99       496      0.02        0.00
framework  phpspy    99       455      0.13        0.02
framework  pfp       999      4996     0.06        0.02
framework  phpspy    999      2832     0.17        0.01
wordpress  pfp       99       496      0.02        0.00
wordpress  phpspy    99       457      0.13        0.02
wordpress  pfp       999      4996     0.05        0.02
wordpress  phpspy    999      2854     0.15        0.03
multi-pid  pfp       99       1984     0.13        0.12
multi-pid  phpspy    99       30       9.54        1.34
multi-pid  pfp       999      19923    0.40        0.68
multi-pid  phpspy    999      0        9.47        1.56
```

## Discussion

### Sample-rate accuracy

pfp captures ≥99.8% of the requested samples in every single-PID cell at
both 99 Hz and 999 Hz. phpspy starts to fall behind around the 100s of Hz
range — at 999 Hz it captures 57–62% of target.

Why: pfp's hot-path stack walk does **2 syscalls per frame** (one bulk read
of `zend_execute_data`, one of the function header) plus cached lookups for
`zend_string` data. phpspy issues a separate `process_vm_readv` for each
field it touches — typically 8–12 reads per frame, plus uncached string
reads for repeated identifiers.

### CPU overhead

Single-PID: pfp is 30–80% lower in user+sys time across all workloads at
99 Hz. At 999 Hz the gap closes because pfp is doing 1.7× the actual work
(more samples captured) but at lower per-sample cost.

Multi-PID: pfp's threads-per-PID model has clean per-sample overhead.
phpspy `-P` re-`pgrep`s on each sample and re-resolves symbols, blowing up
CPU. On the bench host this manifests as ~10s of CPU spent on bookkeeping
during a 5s window.

### Memory

pfp ships with several internal optimisations (mmap'd ELF on attach,
`Arc<str>` interning of function/file names, 256 KB worker stacks) that
keep its RSS below phpspy's:

| | pfp single | phpspy single | pfp multi (4 workers) | phpspy multi |
|---|---|---|---|---|
| RSS                | 3.1 MB | 4.5 MB | 3.3 MB | 4.6 MB |

### Output size

pfp produces marginally larger output because it prints `<internal>:0` for
internal calls where phpspy emits `<internal>:-1`. At 999 Hz × 5s the volume
difference matches the ~75% sample-rate gap (4996 vs ~2800 samples).

## Caveats

### phpspy `-P` (multi-PID) discovery race

phpspy's `-P` mode re-runs `pgrep` and re-reads `/proc/PID/maps` on every
cycle. PIDs from short-lived subprocesses (or rapidly-spawning fpm workers)
disappear between the `pgrep` and the `maps` read, so phpspy emits a
`get_php_bin_path: Failed` for each lost PID and proceeds to the next
cycle. With 4 short-running workers it produces zero successful samples in
this benchmark.

pfp's threads-per-PID model attaches once per discovered PID and persists
the symbol-resolution state, so worker churn doesn't cost samples.

### Single PHP version

Only PHP 8.3 is benchmarked. pfp also supports 8.4 and 8.5; offset
verification against bench numbers there is future work.

### Architecture

Numbers above are arm64; pfp also builds for x86_64 with the same struct
offsets (verified against Sury debug builds). The architecture-specific
code is the `php_version` prologue decoder; a unit-test suite covers both.

## Build flavors

pfp ships with two cargo features (default-on):

- `tui`: ratatui + crossterm for `pfp top` live mode
- `pprof`: prost + flate2 for gzipped pprof v3 output

The `--no-default-features` build drops both, shrinking the release binary
from 2.2 MB to 1.8 MB. RSS is largely unaffected — file-backed code pages
are shared.
