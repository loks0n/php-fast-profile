use anyhow::{Context, Result, anyhow};
use object::elf::{EM_AARCH64, EM_X86_64, ET_DYN, ET_EXEC};
use object::read::elf::{ElfFile64, FileHeader};
use object::{Object, ObjectSection, ObjectSymbol};
use std::collections::HashMap;

/// Resolved virtual addresses (post-relocation, ready for remote reads).
pub struct Symbols {
    pub executor_globals: u64,
    pub php_version: Option<u64>,
}

pub struct ResolveOptions<'a> {
    pub load_base: u64,
    pub executor_globals_override: Option<u64>,
    pub php_version_override: Option<u64>,
    pub allow_stripped: bool,
    pub label: &'a str,
}

pub fn resolve(elf_bytes: &[u8], opts: ResolveOptions<'_>) -> Result<Symbols> {
    let file = ElfFile64::<object::endian::LittleEndian>::parse(elf_bytes)
        .with_context(|| format!("parsing ELF {}", opts.label))?;

    let header = file.elf_header();
    let endian = file.endian();
    let e_type = header.e_type(endian);
    let bias = match e_type {
        ET_DYN => opts.load_base,
        ET_EXEC => 0,
        other => return Err(anyhow!("unsupported ELF type {other}")),
    };

    // Sanity check: refuse to attach to a target whose architecture doesn't
    // match the profiler binary. The php_version prologue decoder is
    // arch-specific, and struct field reads assume native byte order.
    let e_machine = header.e_machine(endian);
    let expected_machine = if cfg!(target_arch = "x86_64") {
        EM_X86_64
    } else if cfg!(target_arch = "aarch64") {
        EM_AARCH64
    } else {
        return Err(anyhow!(
            "pfp built for unsupported host arch (need x86_64 or aarch64)"
        ));
    };
    if e_machine != expected_machine {
        return Err(anyhow!(
            "{} is built for ELF machine {:#x}, but pfp was built for {:#x}. \
             Run a pfp binary that matches the target's architecture.",
            opts.label,
            e_machine,
            expected_machine
        ));
    }

    let mut wanted: HashMap<&str, Option<u64>> = HashMap::from([
        ("executor_globals", None),
        ("php_version", None),
        // ZTS-only: replaces the global executor_globals with a per-thread cache.
        ("tsrm_get_ls_cache", None),
        ("executor_globals_offset", None),
    ]);
    let mut scan = |sym: &dyn ObjectSymbol| {
        if let Ok(name) = sym.name()
            && let Some(slot) = wanted.get_mut(name)
            && slot.is_none()
        {
            *slot = Some(sym.address());
        }
    };
    for sym in file.symbols() {
        scan(&sym);
    }
    for sym in file.dynamic_symbols() {
        scan(&sym);
    }

    let is_zts =
        wanted["tsrm_get_ls_cache"].is_some() || wanted["executor_globals_offset"].is_some();

    let executor_globals = resolve_executor_globals(
        opts.executor_globals_override,
        wanted["executor_globals"],
        is_zts,
        opts.allow_stripped,
        opts.label,
        opts.load_base,
        bias,
    )?;

    let php_version = match (opts.php_version_override, wanted["php_version"]) {
        (Some(addr), _) => Some(apply_override(addr, opts.load_base)),
        (None, Some(rel)) => Some(rel + bias),
        (None, None) => None,
    };

    Ok(Symbols {
        executor_globals,
        php_version,
    })
}

/// If the user passed an address that is already above the load base, treat
/// it as absolute. Otherwise treat it as ELF-relative and rebias.
fn apply_override(addr: u64, load_base: u64) -> u64 {
    if addr >= load_base {
        addr
    } else {
        addr + load_base
    }
}

/// Pure dispatch from "what symbols did we find" + "what overrides did the
/// user provide" to "the absolute address to read EG from, or a clear error".
/// Extracted for unit testing.
fn resolve_executor_globals(
    override_addr: Option<u64>,
    sym_addr: Option<u64>,
    is_zts: bool,
    allow_stripped: bool,
    label: &str,
    load_base: u64,
    bias: u64,
) -> Result<u64> {
    match (override_addr, sym_addr) {
        (Some(addr), _) => Ok(apply_override(addr, load_base)),
        (None, Some(rel)) => Ok(rel + bias),
        (None, None) if is_zts => Err(anyhow!(
            "{label} appears to be a ZTS (thread-safe) PHP build. ZTS \
             attach is not yet supported — see docs/development.md \
             (\"ZTS support\" section). For NTS-only builds, install \
             the non-ZTS package."
        )),
        (None, None) if allow_stripped => Err(anyhow!(
            "symbol `executor_globals` not found in {label} and no \
             --executor-globals override given"
        )),
        (None, None) => Err(anyhow!(
            "symbol `executor_globals` not found in {label} \
             (binary appears fully stripped — pass --executor-globals 0xADDR)"
        )),
    }
}

/// Scan the ELF's read-only sections for a PHP version literal — a
/// `\0`-terminated ASCII string matching `^8\.\d+\.\d+$`.
///
/// Used as a fallback when the binary doesn't export a `php_version` symbol
/// (which is the case for PHP 8.0 / 8.1 / 8.2 — the symbol was added in 8.3).
/// Every Sury / upstream PHP build embeds the bare version string in
/// `.rodata` (referenced from `phpinfo()` output, the `PHP_VERSION` constant
/// initialiser, etc.).
pub fn find_php_version_in_elf(elf_bytes: &[u8]) -> Option<String> {
    let file = ElfFile64::<object::endian::LittleEndian>::parse(elf_bytes).ok()?;
    for section in file.sections() {
        let name = section.name().unwrap_or("");
        if !matches!(name, ".rodata" | ".rodata1" | ".data.rel.ro") {
            continue;
        }
        let Ok(data) = section.data() else { continue };
        if let Some(s) = scan_for_version(data) {
            return Some(s);
        }
    }
    None
}

fn scan_for_version(data: &[u8]) -> Option<String> {
    // Walk \0-terminated runs; match the first one of the form 8.x.y where
    // x and y are 1–3 digit unsigned integers.
    let mut i = 0;
    while i < data.len() {
        let end = data[i..].iter().position(|&b| b == 0).map(|p| i + p)?;
        if end > i + 5 {
            // shortest match is "8.0.0" = 5 bytes
            if let Ok(s) = std::str::from_utf8(&data[i..end])
                && looks_like_php_version(s)
            {
                return Some(s.to_string());
            }
        }
        i = end + 1;
    }
    None
}

fn looks_like_php_version(s: &str) -> bool {
    let mut parts = s.split('.');
    let major = parts.next();
    let minor = parts.next();
    let patch = parts.next();
    let extra = parts.next();
    if extra.is_some() {
        return false;
    }
    let (Some(major), Some(minor), Some(patch)) = (major, minor, patch) else {
        return false;
    };
    major == "8"
        && (1..=3).contains(&minor.len())
        && minor.bytes().all(|b| b.is_ascii_digit())
        && (1..=3).contains(&patch.len())
        && patch.bytes().all(|b| b.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn looks_like_php_version_accepts_real_versions() {
        assert!(looks_like_php_version("8.0.30"));
        assert!(looks_like_php_version("8.1.34"));
        assert!(looks_like_php_version("8.5.6"));
    }

    #[test]
    fn looks_like_php_version_rejects_garbage() {
        assert!(!looks_like_php_version(""));
        assert!(!looks_like_php_version("8"));
        assert!(!looks_like_php_version("8.0"));
        assert!(!looks_like_php_version("7.4.33")); // major must be 8
        assert!(!looks_like_php_version("8.0.30-rc1")); // extra dash
        assert!(!looks_like_php_version("8.0.30.1")); // 4 parts
        assert!(!looks_like_php_version("8.x.y"));
    }

    #[test]
    fn resolve_eg_uses_override_when_provided() {
        let addr =
            resolve_executor_globals(Some(0x1000), None, false, false, "x", 0x10000, 0x10000)
                .unwrap();
        // Below load_base, so it's treated as relative and rebiased.
        assert_eq!(addr, 0x11000);

        let addr =
            resolve_executor_globals(Some(0x20000), None, false, false, "x", 0x10000, 0x10000)
                .unwrap();
        // Above load_base, used as absolute.
        assert_eq!(addr, 0x20000);
    }

    #[test]
    fn resolve_eg_adds_bias_for_pie_symbol() {
        let addr = resolve_executor_globals(None, Some(0x500), false, false, "x", 0x10000, 0x10000)
            .unwrap();
        assert_eq!(addr, 0x10500);
    }

    #[test]
    fn resolve_eg_zts_error_mentions_zts_and_docs() {
        let err = resolve_executor_globals(None, None, true, false, "/usr/bin/php8.3-zts", 0, 0)
            .unwrap_err()
            .to_string();
        assert!(err.contains("ZTS"), "missing ZTS hint: {err}");
        assert!(err.contains("not yet supported"), "missing status: {err}");
        assert!(err.contains("/usr/bin/php8.3-zts"), "missing label: {err}");
    }

    #[test]
    fn resolve_eg_stripped_message_suggests_override() {
        let err = resolve_executor_globals(None, None, false, false, "/usr/bin/php", 0, 0)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("--executor-globals"),
            "missing flag hint: {err}"
        );
    }

    #[test]
    fn scan_finds_version_among_other_strings() {
        // Simulate a .rodata blob: random strings + version literal.
        let mut data = Vec::new();
        data.extend_from_slice(b"some other string\0");
        data.extend_from_slice(b"X-Powered-By: PHP/8.0.30\0");
        data.extend_from_slice(b"8.0.30\0");
        data.extend_from_slice(b"junk\0");
        let v = scan_for_version(&data).unwrap();
        assert_eq!(v, "8.0.30");
    }
}
