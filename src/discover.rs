//! Process discovery: find PIDs matching a name pattern, optionally scoped to
//! a single container. Backed entirely by `/proc` — no `pgrep` shellout.

use anyhow::{Context, Result};
use std::collections::HashSet;
use std::fs;
use std::path::Path;

#[derive(Debug, Clone)]
pub struct ProcessFilter {
    /// Substring matched against `/proc/PID/comm`.
    pub name: Option<String>,
    /// Substring matched against `/proc/PID/cmdline` (NULs replaced by space).
    pub cmdline: Option<String>,
    /// If set, only return PIDs whose cgroup matches this string. Cheap way
    /// to scope to a container — match the container ID prefix.
    pub cgroup: Option<String>,
}

pub fn discover(filter: &ProcessFilter) -> Result<HashSet<i32>> {
    let mut out = HashSet::new();
    let entries = fs::read_dir("/proc").context("reading /proc")?;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = match name.to_str() {
            Some(s) => s,
            None => continue,
        };
        let pid: i32 = match name.parse() {
            Ok(p) => p,
            Err(_) => continue,
        };
        if matches(pid, filter).unwrap_or(false) {
            out.insert(pid);
        }
    }
    Ok(out)
}

fn matches(pid: i32, f: &ProcessFilter) -> Result<bool> {
    if let Some(needle) = &f.name {
        let comm = read_trim(format!("/proc/{pid}/comm"))?;
        if !comm.contains(needle) {
            return Ok(false);
        }
    }
    if let Some(needle) = &f.cmdline {
        let cmd = read_cmdline(pid)?;
        if !cmd.contains(needle) {
            return Ok(false);
        }
    }
    if let Some(needle) = &f.cgroup {
        let cg = read_trim(format!("/proc/{pid}/cgroup")).unwrap_or_default();
        if !cg.contains(needle) {
            return Ok(false);
        }
    }
    Ok(true)
}

fn read_trim<P: AsRef<Path>>(p: P) -> Result<String> {
    let s = fs::read_to_string(&p)?;
    Ok(s.trim().to_string())
}

fn read_cmdline(pid: i32) -> Result<String> {
    let raw = fs::read(format!("/proc/{pid}/cmdline"))?;
    Ok(String::from_utf8_lossy(&raw)
        .replace('\0', " ")
        .trim()
        .to_string())
}

/// Resolve a `cgroup` substring for the *current* process. Useful as a
/// shortcut to scope discovery to "this same container":
///
/// ```ignore
/// let me = discover::self_cgroup_id().unwrap();
/// let pids = discover::discover(&ProcessFilter {
///     name: Some("php-fpm".into()),
///     cmdline: None,
///     cgroup: Some(me),
/// })?;
/// ```
pub fn self_cgroup_id() -> Option<String> {
    // Matches typical Docker cgroup paths:
    //   12:cpuset:/docker/<id>
    //   0::/system.slice/docker-<id>.scope
    let cg = fs::read_to_string("/proc/self/cgroup").ok()?;
    for line in cg.lines() {
        let path = line.rsplit(':').next()?;
        if let Some(idx) = path.rfind("docker-") {
            let after = &path[idx + 7..];
            return after.split('.').next().map(str::to_string);
        }
        if let Some(idx) = path.rfind("/docker/") {
            let after = &path[idx + 8..];
            // 64-hex container id.
            return Some(after.chars().take(64).collect());
        }
    }
    None
}
