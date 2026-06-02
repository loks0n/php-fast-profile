use anyhow::{Context, Result, anyhow};
use std::fs;

#[derive(Debug, Clone)]
pub struct MapEntry {
    pub start: u64,
    pub perms: String,
    pub path: Option<String>,
}

pub fn read_maps(pid: i32) -> Result<Vec<MapEntry>> {
    let raw = fs::read_to_string(format!("/proc/{pid}/maps"))
        .with_context(|| format!("reading /proc/{pid}/maps"))?;
    parse_maps(&raw)
}

pub fn parse_maps(raw: &str) -> Result<Vec<MapEntry>> {
    let mut out = Vec::new();
    for line in raw.lines() {
        // address           perms offset  dev   inode  pathname
        let mut parts = line
            .splitn(6, char::is_whitespace)
            .filter(|s| !s.is_empty());
        let range = parts
            .next()
            .ok_or_else(|| anyhow!("bad maps line: {line}"))?;
        let perms = parts
            .next()
            .ok_or_else(|| anyhow!("bad maps line: {line}"))?;
        let _offset = parts.next();
        let _dev = parts.next();
        let _inode = parts.next();
        let path = parts
            .next()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());

        let (start, _end) = range
            .split_once('-')
            .ok_or_else(|| anyhow!("bad range: {range}"))?;
        out.push(MapEntry {
            start: u64::from_str_radix(start, 16)?,
            perms: perms.to_string(),
            path,
        });
    }
    Ok(out)
}

/// Find the binary backing a process. Returns (path-as-listed-in-maps,
/// load-base). The "principal binary" is the file mapped at the lowest
/// address with execute perms whose path looks like a real on-disk file
/// (not `[heap]`, `[stack]`, `[rosetta]`, or anonymous).
///
/// We avoid relying on `/proc/PID/exe` — under Rosetta on Apple-Silicon
/// Docker that link points at the translator binary, not the target ELF.
pub fn principal_binary(maps: &[MapEntry]) -> Option<&MapEntry> {
    let exe_mapping = maps.iter().find(|m| {
        // A real ELF text segment is a *private* file-backed mapping
        // (`r-xp`). Skip *shared* executable mappings (`r-xs`) — Swoole and
        // other extensions place executable shared-memory pools backed by
        // `/dev/zero (deleted)` or `/memfd:… (deleted)` at low addresses,
        // which are not openable on-disk ELFs.
        m.perms.contains('x')
            && m.perms.contains('p')
            && m.path.as_deref().is_some_and(|p| {
                !p.starts_with('[')
                    && !p.is_empty()
                    && !p.starts_with("/dev/")
                    && !p.ends_with("(deleted)")
            })
    })?;
    let path = exe_mapping.path.as_deref()?;
    // The lowest-address mapping for that file (often a r--p header before
    // the r-xp text) is the actual load base.
    maps.iter()
        .filter(|m| m.path.as_deref() == Some(path))
        .min_by_key(|m| m.start)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
555555554000-555555641000 r--p 00000000 00:1d9 120084486                 /usr/bin/php8.3
555555641000-555555969000 r-xp 000ed000 00:1d9 120084486                 /usr/bin/php8.3
555555ae9000-555555d0f000 rw-p 00000000 00:00 0
7ffffdbbc000-7ffffdc3d000 rw-p 00000000 00:00 0
7ffffdc66000-7ffffdc68000 r--p 00000000 00:1d9 120058383                 /usr/lib/php/20230831/tokenizer.so
7ffffe0d2000-7ffffe0f7000 r-xp 00001000 00:1d9 120030011                 /usr/lib/x86_64-linux-gnu/libc.so.6
7ffffffde000-7fffffffe000 rw-p 00000000 00:00 0                          [stack]
";

    #[test]
    fn parses_typical_maps() {
        let maps = parse_maps(SAMPLE).unwrap();
        assert_eq!(maps.len(), 7);

        let first = &maps[0];
        assert_eq!(first.start, 0x555555554000);
        assert_eq!(first.perms, "r--p");
        assert_eq!(first.path.as_deref(), Some("/usr/bin/php8.3"));

        // Anonymous mapping has no path.
        assert_eq!(maps[2].path, None);
        // Stack pseudo-path is preserved.
        assert_eq!(maps[6].path.as_deref(), Some("[stack]"));
    }

    #[test]
    fn principal_binary_skips_anon_and_pseudo() {
        let maps = parse_maps(SAMPLE).unwrap();
        let p = principal_binary(&maps).unwrap();
        // Should pick php8.3 — the first executable mapping with a real path.
        assert_eq!(p.path.as_deref(), Some("/usr/bin/php8.3"));
        // And specifically the lowest-address mapping for that file.
        assert_eq!(p.start, 0x555555554000);
    }

    #[test]
    fn principal_binary_skips_swoole_shared_exec_mapping() {
        // Swoole maps an executable *shared* memory pool backed by
        // `/dev/zero (deleted)` at a lower address than the php text segment.
        // The principal binary must still resolve to the real php ELF.
        let swoole = "\
5649acc00000-5649c4c00000 rw-s 00000000 00:01 4586                       /dev/zero (deleted)
5649c4c00000-5649ccc00000 r-xs 18000000 00:01 4586                       /dev/zero (deleted)
5649ccc00000-5649ccd69000 r--p 00000000 fe:01 656824                     /usr/local/bin/php
5649cce00000-5649cd601000 r-xp 00200000 fe:01 656824                     /usr/local/bin/php
";
        let maps = parse_maps(swoole).unwrap();
        let p = principal_binary(&maps).unwrap();
        assert_eq!(p.path.as_deref(), Some("/usr/local/bin/php"));
        assert_eq!(p.start, 0x5649ccc00000);
    }

    #[test]
    fn principal_binary_handles_only_anon() {
        let only_anon = "\
7ffffdbbc000-7ffffdc3d000 rw-p 00000000 00:00 0
7ffffffde000-7fffffffe000 rw-p 00000000 00:00 0                          [stack]
";
        let maps = parse_maps(only_anon).unwrap();
        assert!(principal_binary(&maps).is_none());
    }

    #[test]
    fn parse_maps_rejects_garbage() {
        assert!(parse_maps("not a maps line").is_err());
    }
}
