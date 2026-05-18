#!/bin/sh
# Compare pfp vs phpspy across a matrix of workloads and sample rates.
#
# Designed to run inside a Linux container so the environment is reproducible.
# Works on both x86_64 and aarch64 hosts:
#
#   # native:
#   docker run --rm -v "$PWD":/src --cap-add=SYS_PTRACE rust:latest \
#     /src/scripts/bench.sh
#
#   # cross-arch (slow, but useful for parity checks):
#   docker run --rm -v "$PWD":/src --platform linux/amd64 \
#     --cap-add=SYS_PTRACE rust:latest /src/scripts/bench.sh
#
# Override defaults via env: BENCH_DURATION (sec/cell), BENCH_RATES.
#
# When run on Apple-Silicon Docker via Rosetta, phpspy's auto-discovery breaks
# because /proc/PID/exe points at the Rosetta translator, not the actual
# binary. We work around that by passing executor_globals via -x.

set -u
cd /src

DURATION="${BENCH_DURATION:-5}"
RATES="${BENCH_RATES:-99 999}"
RESULTS=/tmp/bench-results.csv

###############################################################################
# Setup
###############################################################################

setup() {
    export DEBIAN_FRONTEND=noninteractive
    apt-get update >/dev/null
    apt-get install -y --no-install-recommends \
        curl ca-certificates gnupg protobuf-compiler \
        time git build-essential pkg-config procps \
        binutils bsdmainutils >/dev/null

    curl -sSLo /usr/share/keyrings/sury.gpg https://packages.sury.org/php/apt.gpg
    . /etc/os-release
    echo "deb [signed-by=/usr/share/keyrings/sury.gpg] https://packages.sury.org/php/ ${VERSION_CODENAME} main" \
        > /etc/apt/sources.list.d/sury.list
    apt-get update >/dev/null
    apt-get install -y --no-install-recommends php8.3-cli >/dev/null

    echo "### building pfp..."
    cargo build --release 2>&1 | tail -3

    if [ ! -x /usr/local/bin/phpspy ]; then
        echo "### building phpspy from upstream..."
        cd /tmp
        git clone --depth 1 -q https://github.com/adsr/phpspy.git phpspy-src
        cd phpspy-src && make >/dev/null 2>&1
        cp phpspy /usr/local/bin/phpspy
        cd /src
    fi
    echo "### versions:"
    echo "  phpspy: $(phpspy -v 2>&1 | head -1)"
    echo "  php:    $(php8.3 -v | head -1)"

    EG_REL=$(nm -D /usr/bin/php8.3 | awk '/ B executor_globals$/{print $1; exit}')
    echo "  executor_globals (rel): 0x$EG_REL"

    mkdir -p /tmp/workloads
    write_workloads
}

write_workloads() {
    cat > /tmp/workloads/synthetic.php <<'PHP'
<?php
class Worker {
    public function spin(int $iters): void {
        for ($i = 0; $i < $iters; $i++) { $this->level1(); }
    }
    private function level1(): void { $this->level2(); }
    private function level2(): void { $this->level3(); }
    private function level3(): void { $this->level4(); }
    private function level4(): void { $this->level5(); }
    private function level5(): void { $this->level6(); }
    private function level6(): void { $this->level7(); }
    private function level7(): void { $this->level8(); }
    private function level8(): void { usleep(50); }
}
(new Worker())->spin((int)($argv[1] ?? 10000000));
PHP

    cat > /tmp/workloads/framework.php <<'PHP'
<?php
namespace App\Service;
class Repository {
    private array $items = [];
    public function find(int $id): array {
        if (isset($this->items[$id])) return $this->items[$id];
        $this->items[$id] = ['id' => $id, 'name' => "item-$id"];
        return $this->items[$id];
    }
}
class HelloController {
    public function __construct(private Repository $repo) {}
    public function __invoke(int $i): string {
        $items = [];
        for ($n = 0; $n < 20; $n++) $items[] = $this->repo->find(($i + $n) % 200);
        return json_encode($items);
    }
}
$repo = new Repository();
$ctrl = new HelloController($repo);
$iters = (int)($argv[1] ?? 1000000);
for ($i = 0; $i < $iters; $i++) $ctrl($i);
PHP

    cat > /tmp/workloads/wordpress.php <<'PHP'
<?php
class WP_Hook {
    private array $callbacks = [];
    public function add_filter(string $tag, callable $cb, int $prio = 10): void {
        $this->callbacks[$tag][$prio][] = $cb;
    }
    public function apply_filters(string $tag, mixed $value): mixed {
        if (!isset($this->callbacks[$tag])) return $value;
        ksort($this->callbacks[$tag]);
        foreach ($this->callbacks[$tag] as $cbs) {
            foreach ($cbs as $cb) $value = $cb($value);
        }
        return $value;
    }
}
$hook = new WP_Hook();
for ($i = 0; $i < 100; $i++) {
    $hook->add_filter("event_$i", fn($v) => $v + 1);
    $hook->add_filter("event_$i", fn($v) => $v * 2, 20);
}
$iters = (int)($argv[1] ?? 1000000);
for ($i = 0; $i < $iters; $i++) {
    $hook->apply_filters("event_" . ($i % 100), 1);
}
PHP
}

###############################################################################
# Helpers
###############################################################################

# Find load base of /usr/bin/php8.3 in the target's maps. Returns absolute
# executor_globals address as hex (no 0x prefix), or empty on failure.
target_eg_abs() {
    local pid="$1"
    local base
    base=$(awk -F'[- ]' '/\/usr\/bin\/php8\.3$/ && !found { print $1; found=1 }' "/proc/$pid/maps" 2>/dev/null)
    if [ -z "$base" ]; then
        return 1
    fi
    printf '%x' $(( 0x$base + 0x$EG_REL ))
}

# Run one bench cell.
# Globals consumed: DURATION, EG_REL, RESULTS.
# Args: workload-name, profiler (pfp|phpspy), rate-Hz, php-script-path
run_cell() {
    local workload="$1" profiler="$2" rate="$3" script="$4"
    local outfile="/tmp/out.${workload}.${profiler}.${rate}"
    local timefile="/tmp/time.${workload}.${profiler}.${rate}"
    rm -f "$outfile" "$timefile"

    php8.3 "$script" 100000000 >/dev/null 2>&1 &
    local TPID=$!
    sleep 1
    if ! kill -0 "$TPID" 2>/dev/null; then
        printf '  %-12s %-7s %5sHz | TARGET DIED before attach\n' "$workload" "$profiler" "$rate"
        echo "$workload,$profiler,$rate,$DURATION,0,0,0,0,0" >> "$RESULTS"
        return
    fi

    local eg_abs
    eg_abs=$(target_eg_abs "$TPID")
    if [ -z "$eg_abs" ]; then
        printf '  %-12s %-7s %5sHz | NO MAPS for php8.3 in pid %s\n' "$workload" "$profiler" "$rate" "$TPID"
        kill -TERM "$TPID" 2>/dev/null; wait "$TPID" 2>/dev/null
        echo "$workload,$profiler,$rate,$DURATION,0,0,0,0,0" >> "$RESULTS"
        return
    fi

    case "$profiler" in
        pfp)
            /usr/bin/time -f '%U %S %M' -o "$timefile" \
                ./target/release/pfp -p "$TPID" -d "$DURATION" -H "$rate" \
                    -f stacks -o "$outfile" >/dev/null 2>&1 || true
            ;;
        phpspy)
            /usr/bin/time -f '%U %S %M' -o "$timefile" \
                phpspy -p "$TPID" -H "$rate" -i $((DURATION * 1000)) \
                    -V 83 -x "$eg_abs" -o "$outfile" >/dev/null 2>&1 || true
            ;;
    esac

    kill -TERM "$TPID" 2>/dev/null
    wait "$TPID" 2>/dev/null || true

    local U S M
    if [ -f "$timefile" ]; then
        read -r U S M < "$timefile" || { U=0; S=0; M=0; }
    else
        U=0; S=0; M=0
    fi
    local cpu samples outsize
    cpu=$(awk -v u="$U" -v s="$S" 'BEGIN{printf "%.2f", u+s}')
    samples=0; outsize=0
    if [ -f "$outfile" ]; then
        outsize=$(wc -c < "$outfile")
        samples=$(count_samples "$profiler" "$outfile")
    fi
    printf '  %-12s %-7s %5sHz | samples=%6d cpu=%5ss rss=%6sKB\n' \
        "$workload" "$profiler" "$rate" "$samples" "$cpu" "$M"
    echo "$workload,$profiler,$rate,$DURATION,$samples,$U,$S,$M,,$outsize" >> "$RESULTS"
}

count_samples() {
    case "$1" in
        pfp)    grep -c '^0 ' "$2" 2>/dev/null || echo 0 ;;
        phpspy) awk 'BEGIN{c=0;s=0} /^[^#]/&&NF{s=1} /^$/{if(s){c++;s=0}} END{if(s)c++; print c}' "$2" ;;
    esac
}

###############################################################################
# Multi-PID
###############################################################################

run_multi_cell() {
    local profiler="$1" rate="$2"
    local outfile="/tmp/out.multi.${profiler}.${rate}"
    local timefile="/tmp/time.multi.${profiler}.${rate}"
    rm -f "$outfile" "$timefile"

    local pids
    pids=""
    for i in 1 2 3 4; do
        php8.3 /tmp/workloads/synthetic.php 100000000 >/dev/null 2>&1 &
        pids="$pids $!"
    done
    sleep 1

    # Filter to those still alive.
    local alive=""
    for p in $pids; do
        if kill -0 "$p" 2>/dev/null; then alive="$alive $p"; fi
    done
    if [ -z "$alive" ]; then
        printf '  %-12s %-7s %5sHz | all targets died before attach\n' multi-pid "$profiler" "$rate"
        echo "multi-pid,$profiler,$rate,$DURATION,0,0,0,0,0" >> "$RESULTS"
        return
    fi
    local first_pid eg_abs
    first_pid=$(echo $alive | awk '{print $1}')
    eg_abs=$(target_eg_abs "$first_pid")

    case "$profiler" in
        pfp)
            /usr/bin/time -f '%U %S %M' -o "$timefile" \
                ./target/release/pfp -P php8.3 --cmdline synthetic.php \
                    -d "$DURATION" -H "$rate" -f stacks -o "$outfile" \
                    >/dev/null 2>&1 || true
            ;;
        phpspy)
            # phpspy -P uses pgrep args. We match the synthetic script's
            # cmdline. -x sets the EG address (Rosetta workaround); same
            # binary so the address is the same for all workers.
            /usr/bin/time -f '%U %S %M' -o "$timefile" \
                phpspy -P '-f synthetic.php' -H "$rate" -i $((DURATION * 1000)) \
                    -V 83 -x "$eg_abs" -o "$outfile" >/dev/null 2>&1 || true
            ;;
    esac

    for p in $pids; do kill -TERM "$p" 2>/dev/null; done
    wait 2>/dev/null || true

    local U S M
    if [ -f "$timefile" ]; then
        read -r U S M < "$timefile" || { U=0; S=0; M=0; }
    else
        U=0; S=0; M=0
    fi
    local cpu samples outsize
    cpu=$(awk -v u="$U" -v s="$S" 'BEGIN{printf "%.2f", u+s}')
    samples=0; outsize=0
    if [ -f "$outfile" ]; then
        outsize=$(wc -c < "$outfile")
        samples=$(count_samples "$profiler" "$outfile")
    fi
    printf '  %-12s %-7s %5sHz | samples=%6d cpu=%5ss rss=%6sKB\n' \
        multi-pid "$profiler" "$rate" "$samples" "$cpu" "$M"
    echo "multi-pid,$profiler,$rate,$DURATION,$samples,$U,$S,$M,,$outsize" >> "$RESULTS"
}

###############################################################################
# Main
###############################################################################

main() {
    setup
    echo
    echo "### benchmark matrix (duration=${DURATION}s/cell, rates=${RATES})"
    echo

    echo "workload,profiler,rate_hz,duration_s,samples,user_cpu_s,sys_cpu_s,rss_kb,target_wall_s,output_size_b" > "$RESULTS"

    for workload in synthetic framework wordpress; do
        for rate in $RATES; do
            run_cell "$workload" pfp    "$rate" "/tmp/workloads/${workload}.php"
            run_cell "$workload" phpspy "$rate" "/tmp/workloads/${workload}.php"
        done
    done
    for rate in $RATES; do
        run_multi_cell pfp    "$rate"
        run_multi_cell phpspy "$rate"
    done

    echo
    echo "### results"
    column -t -s, "$RESULTS" 2>/dev/null || cat "$RESULTS"
    echo
    echo "### CSV: $RESULTS"
}

main "$@"
