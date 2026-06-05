use anyhow::{Context, Result, anyhow, bail};
use clap::{Parser, ValueEnum};
use std::path::PathBuf;
use std::time::Duration;

use crate::discover::{self, ProcessFilter};
use crate::sampler::Sampler;

#[derive(Debug, Clone, Copy, ValueEnum, PartialEq, Eq)]
pub enum Format {
    /// One frame per line, blank line between samples.
    Stacks,
    /// Inferno/FlameGraph folded stacks (semicolon-joined).
    Folded,
    /// gzipped pprof protobuf.
    Pprof,
    /// Live `top`-style display in the terminal.
    Top,
}

#[derive(Parser, Debug)]
#[command(name = "pfp", version, about = "Sampling profiler for PHP 8.0+")]
pub struct Args {
    /// PID of the PHP process to attach to.
    #[arg(short = 'p', long, conflicts_with_all = ["pgrep", "cmdline"])]
    pub pid: Option<i32>,

    /// Discover PIDs whose `comm` (process name) contains this substring.
    /// Re-run periodically to pick up new workers (`--rediscover-secs`).
    #[arg(short = 'P', long)]
    pub pgrep: Option<String>,

    /// Discover PIDs whose `cmdline` contains this substring. Combine with
    /// `--pgrep` for AND semantics.
    #[arg(long)]
    pub cmdline: Option<String>,

    /// Restrict discovery to processes in the same container as this profiler.
    /// Useful when running inside a sidecar.
    #[arg(long)]
    pub this_container: bool,

    /// Re-discover PIDs every N seconds in pgrep/cmdline mode (0 = never).
    #[arg(long, default_value_t = 5)]
    pub rediscover_secs: u64,

    /// Sampling rate in Hz.
    #[arg(short = 'H', long, default_value_t = 99)]
    pub rate_hz: u32,

    /// Stop after this many seconds (0 = run forever).
    #[arg(short = 'd', long, default_value_t = 0)]
    pub duration_secs: u64,

    /// Maximum stack depth to capture per sample.
    #[arg(short = 's', long, default_value_t = 256)]
    pub max_depth: usize,

    /// Output format.
    #[arg(short = 'f', long, value_enum, default_value_t = Format::Stacks)]
    pub format: Format,

    /// Output file (defaults to stdout).
    #[arg(short = 'o', long)]
    pub output: Option<PathBuf>,

    /// Continuously push profiles to this Pyroscope server (e.g.
    /// http://pyroscope:4040). Enables sidecar mode; `--format`/`--output` are
    /// ignored when set.
    #[arg(long, env = "PYROSCOPE_URL")]
    pub pyroscope_url: Option<String>,

    /// Application name reported to Pyroscope.
    #[arg(long, env = "PYROSCOPE_APP", default_value = "php")]
    pub pyroscope_app: String,

    /// Label attached to the Pyroscope series, `key=value`. Repeatable.
    #[arg(long = "pyroscope-label", value_name = "K=V")]
    pub pyroscope_label: Vec<String>,

    /// How often to push accumulated profiles to Pyroscope, in seconds.
    #[arg(long, default_value_t = 10)]
    pub push_interval_secs: u64,

    /// Bearer token for Pyroscope / Grafana Cloud auth.
    #[arg(long, env = "PYROSCOPE_AUTH_TOKEN", hide_env_values = true)]
    pub pyroscope_auth_token: Option<String>,

    /// Tenant id for multi-tenant Pyroscope (sent as X-Scope-OrgID).
    #[arg(long, env = "PYROSCOPE_TENANT_ID")]
    pub pyroscope_tenant_id: Option<String>,

    /// Capture request info ($_SERVER URI/method/etc.).
    #[arg(long)]
    pub request_info: bool,

    /// Force the PHP minor version (e.g. "8.4") when symbols are unavailable
    /// to determine it. Required only for fully stripped binaries.
    #[arg(long)]
    pub php_version: Option<String>,

    /// Override the address of `executor_globals` (hex, e.g. 0x598fa0). Use
    /// when symbols have been stripped. The base address from /proc/PID/maps
    /// is added automatically for PIE executables.
    #[arg(long, value_parser = parse_hex_addr)]
    pub executor_globals: Option<u64>,
}

fn parse_hex_addr(s: &str) -> Result<u64, String> {
    let s = s
        .strip_prefix("0x")
        .or_else(|| s.strip_prefix("0X"))
        .unwrap_or(s);
    u64::from_str_radix(s, 16).map_err(|e| format!("invalid hex address: {e}"))
}

pub fn run(args: Args) -> Result<()> {
    let interval = Duration::from_nanos(1_000_000_000 / args.rate_hz.max(1) as u64);
    let duration = if args.duration_secs == 0 {
        None
    } else {
        Some(Duration::from_secs(args.duration_secs))
    };

    if args.format == Format::Top {
        #[cfg(feature = "tui")]
        return crate::sampler::run_top(&args, interval, duration);
        #[cfg(not(feature = "tui"))]
        bail!("--format top requires the `tui` feature (default-on)");
    }

    if let Some(pid) = args.pid {
        let mut sampler =
            Sampler::attach(pid, &args).with_context(|| format!("attaching to pid {pid}"))?;
        sampler.run(interval, duration)
    } else if args.pgrep.is_some() || args.cmdline.is_some() {
        let cgroup = if args.this_container {
            Some(discover::self_cgroup_id().ok_or_else(|| {
                anyhow!("--this-container set but no cgroup id detected (not in a container?)")
            })?)
        } else {
            None
        };
        let filter = ProcessFilter {
            name: args.pgrep.clone(),
            cmdline: args.cmdline.clone(),
            cgroup,
        };
        crate::sampler::run_multi(filter, &args, interval, duration)
    } else {
        bail!("must specify -p PID, -P PGREP, or --cmdline");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_hex_with_and_without_prefix() {
        assert_eq!(parse_hex_addr("0x598fa0").unwrap(), 0x598fa0);
        assert_eq!(parse_hex_addr("0X598FA0").unwrap(), 0x598fa0);
        assert_eq!(parse_hex_addr("598fa0").unwrap(), 0x598fa0);
        assert_eq!(parse_hex_addr("0").unwrap(), 0);
    }

    #[test]
    fn rejects_invalid_hex() {
        assert!(parse_hex_addr("xyz").is_err());
        assert!(parse_hex_addr("0xGHI").is_err());
        assert!(parse_hex_addr("").is_err());
    }
}
