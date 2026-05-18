use anyhow::{Context, Result, anyhow};

use crate::offsets::{self, VersionLayout};
use crate::proc;
use crate::remote::Remote;
use crate::symbols::{self, ResolveOptions, Symbols};
use crate::zend::{self, Frame, StackWalker};

#[derive(Default)]
pub struct AttachOptions<'a> {
    pub executor_globals: Option<u64>,
    pub php_version_addr: Option<u64>,
    pub php_version_string: Option<&'a str>,
}

pub struct Target {
    pub pid: i32,
    pub remote: Remote,
    pub symbols: Symbols,
    pub php_version: String,
    pub layout: VersionLayout,
    pub exe: String,
    walker: StackWalker,
}

impl Target {
    pub fn attach(pid: i32, opts: AttachOptions<'_>) -> Result<Self> {
        let maps = proc::read_maps(pid)?;
        let principal = proc::principal_binary(&maps)
            .ok_or_else(|| anyhow!("no principal binary found in /proc/{pid}/maps"))?;
        let bin_path = principal
            .path
            .clone()
            .ok_or_else(|| anyhow!("principal binary has no path"))?;
        let base = principal.start;

        // mmap rather than read — keeps the ELF as a shared file-backed
        // mapping instead of dirtying anon pages on every worker attach.
        let elf_file =
            std::fs::File::open(&bin_path).with_context(|| format!("opening {bin_path}"))?;
        let elf_map = unsafe { memmap2::Mmap::map(&elf_file) }
            .with_context(|| format!("mmaping {bin_path}"))?;
        let symbols = symbols::resolve(
            &elf_map,
            ResolveOptions {
                load_base: base,
                executor_globals_override: opts.executor_globals,
                php_version_override: opts.php_version_addr,
                allow_stripped: opts.executor_globals.is_some()
                    || opts.php_version_string.is_some(),
                label: &bin_path,
            },
        )
        .with_context(|| format!("resolving symbols in {bin_path}"))?;

        let remote = Remote::new(pid);

        let php_version = if let Some(s) = opts.php_version_string {
            s.to_string()
        } else if let Some(addr) = symbols.php_version {
            read_php_version(&remote, addr).context("reading php_version()")?
        } else if let Some(version) = symbols::find_php_version_in_elf(&elf_map) {
            // PHP 8.0 / 8.1 / 8.2 don't export `php_version` as a function;
            // fall back to scanning the ELF for the literal version string.
            tracing::debug!("php_version symbol absent; scanned .rodata: {version}");
            version
        } else {
            return Err(anyhow!(
                "could not determine PHP version: php_version symbol absent, \
                 no version literal found in .rodata, and --php-version not given"
            ));
        };

        let layout = offsets::pick(&php_version).ok_or_else(|| {
            anyhow!(
                "unsupported PHP version {:?} (need 8.0, 8.1, 8.2, 8.3, 8.4, or 8.5)",
                php_version
            )
        })?;

        Ok(Self {
            pid,
            remote,
            symbols,
            php_version,
            layout,
            exe: bin_path,
            walker: StackWalker::new(),
        })
    }

    pub fn capture_stack(&mut self, max_depth: usize) -> Result<Vec<Frame>> {
        let mut frames = Vec::with_capacity(16);
        let mut ex =
            zend::current_execute_data(&self.remote, self.symbols.executor_globals, &self.layout)?;
        let mut depth = 0;
        while ex != 0 && depth < max_depth {
            let (frame, prev) = match self.walker.read_frame(&self.remote, ex, &self.layout) {
                Ok(p) => p,
                Err(_) => break,
            };
            frames.push(frame);
            ex = prev;
            depth += 1;
        }
        Ok(frames)
    }

    /// Look up a key in `$_SERVER`. EG.symbol_table holds all globals; the
    /// `_SERVER` entry is an IS_ARRAY zval pointing at another HashTable.
    pub fn request_var(&self, key: &str) -> Option<String> {
        request_var_impl(self, key).ok().flatten()
    }
}

/// `php_version` in libphp is a tiny accessor that returns a pointer to a
/// `.rodata` string. We decode the prologue to find that pointer.
///
/// On x86_64:
///   [endbr64]                ; f3 0f 1e fa   (optional, CET landing pad)
///   lea  rax, [rip + disp32] ; 48 8d 05 xx xx xx xx
///   ret                      ; c3
///
/// On aarch64:
///   [bti c]                  ; 4 bytes (optional — Armv8.5-A BTI landing pad)
///   adrp x0, <page>          ; 4 bytes — PC-relative page address
///   add  x0, x0, #imm12      ; 4 bytes — low 12 bits
///   ret                      ; d6 5f 03 c0
fn read_php_version(rem: &Remote, fn_addr: u64) -> Result<String> {
    let mut buf = [0u8; 20];
    rem.read(fn_addr, &mut buf)?;

    let str_addr = decode_php_version_prologue(fn_addr, &buf)?;
    rem.read_cstring(str_addr, 32)
}

#[cfg(target_arch = "x86_64")]
fn decode_php_version_prologue(fn_addr: u64, buf: &[u8]) -> Result<u64> {
    const ENDBR64: [u8; 4] = [0xf3, 0x0f, 0x1e, 0xfa];
    let lea_off = if buf.len() >= 4 && buf[..4] == ENDBR64 {
        4
    } else {
        0
    };
    let lea = &buf[lea_off..];
    if lea.len() < 8 || lea[..3] != [0x48, 0x8d, 0x05] || lea[7] != 0xc3 {
        return Err(anyhow!(
            "php_version prologue not recognized: {buf:02x?} \
             (expected [endbr64] lea rip+disp32, ret)"
        ));
    }
    let disp = i32::from_le_bytes([lea[3], lea[4], lea[5], lea[6]]) as i64;
    let next_insn = fn_addr as i64 + lea_off as i64 + 7;
    Ok((next_insn + disp) as u64)
}

#[cfg(target_arch = "aarch64")]
fn decode_php_version_prologue(fn_addr: u64, buf: &[u8]) -> Result<u64> {
    if buf.len() < 16 {
        return Err(anyhow!("short read for aarch64 php_version: {}", buf.len()));
    }
    // Skip an optional BTI landing pad (Armv8.5-A). All four BTI variants
    // (bti, bti c, bti j, bti jc) match (insn & 0xffffff1f) == 0xd503241f.
    let first = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
    let prologue_off = if (first & 0xffffff1f) == 0xd503241f {
        4
    } else {
        0
    };

    if buf.len() < prologue_off + 12 {
        return Err(anyhow!(
            "short read for aarch64 php_version after BTI: {}",
            buf.len()
        ));
    }
    let p = &buf[prologue_off..];
    let adrp = u32::from_le_bytes([p[0], p[1], p[2], p[3]]);
    let add = u32::from_le_bytes([p[4], p[5], p[6], p[7]]);
    let ret = u32::from_le_bytes([p[8], p[9], p[10], p[11]]);

    let adrp_pc = fn_addr.wrapping_add(prologue_off as u64);

    // ADRP: top byte 1xx10000 (0x90 with op<<31 set). Mask: 0x9f000000 == 0x90000000.
    if (adrp & 0x9f000000) != 0x90000000 {
        return Err(anyhow!(
            "php_version prologue: expected ADRP, got {adrp:#010x} (buf={buf:02x?})"
        ));
    }
    // ADD (immediate, shifted 0): 0x91000000 mask 0xff800000.
    if (add & 0xff800000) != 0x91000000 {
        return Err(anyhow!(
            "php_version prologue: expected ADD imm, got {add:#010x}"
        ));
    }
    // RET: 0xd65f03c0 (literal).
    if ret != 0xd65f03c0 {
        return Err(anyhow!(
            "php_version prologue: expected RET, got {ret:#010x}"
        ));
    }

    // ADRP encoding:
    //   bits [30:29] = immlo (2 bits)
    //   bits [23:5]  = immhi (19 bits)
    //   imm = sign_extend(immhi:immlo, 21) << 12
    //   target = (PC & ~0xfff) + imm
    let immlo = ((adrp >> 29) & 0x3) as u64;
    let immhi = ((adrp >> 5) & 0x7ffff) as u64;
    let mut imm = (immhi << 2) | immlo;
    // sign-extend from 21 bits
    if imm & (1 << 20) != 0 {
        imm |= !((1u64 << 21) - 1);
    }
    let page = (adrp_pc & !0xfff).wrapping_add(imm.wrapping_shl(12));

    // ADD imm12, shift=0: bits [21:10] are imm12. We've masked off imm shift
    // already with the 0xff800000 check. Shift bit lives at [22:21]; both
    // zero means logical-shift-zero, the only encoding the compiler emits
    // for this idiom.
    let imm12 = ((add >> 10) & 0xfff) as u64;
    Ok(page.wrapping_add(imm12))
}

fn request_var_impl(t: &Target, key: &str) -> Result<Option<String>> {
    use crate::offsets::{bucket, ht, zval};
    let symbol_table = t.symbols.executor_globals + t.layout.eg_symbol_table;
    let n_used = t.remote.read_u32(symbol_table + ht::N_NUM_USED)? as u64;
    let ar_data = t.remote.read_u64(symbol_table + ht::AR_DATA)?;
    if ar_data == 0 {
        return Ok(None);
    }
    for i in 0..n_used.min(4096) {
        let b = ar_data + i * bucket::SIZE;
        let key_ptr = t.remote.read_u64(b + bucket::KEY).unwrap_or(0);
        if key_ptr == 0 {
            continue;
        }
        let k = match zend::read_zend_string(&t.remote, key_ptr) {
            Ok(s) => s,
            Err(_) => continue,
        };
        if k != "_SERVER" {
            continue;
        }
        let type_info = t.remote.read_u32(b + bucket::VAL + zval::TYPE_INFO)?;
        let tag = (type_info & 0xff) as u8;
        let array_ptr = if tag == zval::IS_REFERENCE {
            let r = t.remote.read_u64(b + bucket::VAL + zval::VALUE)?;
            t.remote.read_u64(r + 16 + zval::VALUE)?
        } else if tag == zval::IS_ARRAY {
            t.remote.read_u64(b + bucket::VAL + zval::VALUE)?
        } else {
            return Ok(None);
        };
        if array_ptr == 0 {
            return Ok(None);
        }
        return zend::ht_get_string(&t.remote, array_ptr, key);
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn x86_64_prologue_no_endbr() {
        // Real bytes from Sury php8.3-cli (build without CET) at 0x29d640:
        //   29d640: lea 0x1a63de(%rip), %rax
        //   29d647: ret
        // RIP-relative target = 0x29d640 + 7 + 0x1a63de = 0x443a25
        let buf = [0x48, 0x8d, 0x05, 0xde, 0x63, 0x1a, 0x00, 0xc3, 0, 0, 0, 0];
        let addr = decode_php_version_prologue(0x29d640, &buf).unwrap();
        assert_eq!(addr, 0x443a25);
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn x86_64_prologue_with_endbr64() {
        // Same disp32 but with endbr64 prepended — the function address
        // shifts by 4, so next_insn = fn_addr + 4 + 7 and the target moves
        // by 4 bytes too.
        let buf = [
            0xf3, 0x0f, 0x1e, 0xfa, 0x48, 0x8d, 0x05, 0xde, 0x63, 0x1a, 0x00, 0xc3,
        ];
        let addr = decode_php_version_prologue(0x29d640, &buf).unwrap();
        assert_eq!(addr, 0x443a29);
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn x86_64_prologue_rejects_garbage() {
        let buf = [0x90; 12];
        assert!(decode_php_version_prologue(0x1000, &buf).is_err());
    }

    #[cfg(target_arch = "aarch64")]
    #[test]
    fn aarch64_prologue_decodes_real_sury_bytes() {
        // From Sury aarch64 php8.3-cli at 0x288ba0:
        //   adrp x0, 0x402000   ; needs imm = 0x17a (page delta = 0x17a000)
        //   add  x0, x0, #0xbc8
        //   ret
        //
        // ADRP encoding:
        //   bits 31    op (1)
        //   bits 30:29 immlo  (low 2 bits of 21-bit immediate)
        //   bits 28:24 fixed 0x10
        //   bits 23:5  immhi  (high 19 bits)
        //   bits 4:0   Rd
        // imm=0x17a -> immlo=0b10 (=2), immhi=0x5e
        // word = (1<<31) | (2<<29) | (0x10<<24) | (0x5e<<5) | 0 = 0xd0000bc0
        let adrp = 0xd000_0bc0_u32.to_le_bytes();
        // ADD imm12: 0x91000000 | (0xbc8 << 10) | (0 << 5) | 0 = 0x912f2000
        let add = 0x912f_2000_u32.to_le_bytes();
        let ret = 0xd65f_03c0_u32.to_le_bytes();
        let mut buf = [0u8; 16];
        buf[0..4].copy_from_slice(&adrp);
        buf[4..8].copy_from_slice(&add);
        buf[8..12].copy_from_slice(&ret);

        let str_addr = decode_php_version_prologue(0x288ba0, &buf).unwrap();
        assert_eq!(str_addr, 0x402bc8);
    }

    #[cfg(target_arch = "aarch64")]
    #[test]
    fn aarch64_prologue_skips_bti_landing_pad() {
        // Real bytes from Sury aarch64 php8.3-cli (with BTI):
        //   d503245f  bti c
        //   f0000c00  adrp x0, ...
        //   91152000  add  x0, x0, #0x548
        //   d65f03c0  ret
        let bti = 0xd503_245f_u32.to_le_bytes();
        let adrp = 0xf000_0c00_u32.to_le_bytes();
        let add = 0x9115_2000_u32.to_le_bytes();
        let ret = 0xd65f_03c0_u32.to_le_bytes();
        let mut buf = [0u8; 20];
        buf[0..4].copy_from_slice(&bti);
        buf[4..8].copy_from_slice(&adrp);
        buf[8..12].copy_from_slice(&add);
        buf[12..16].copy_from_slice(&ret);

        // Just verify it parses without erroring; specific addr depends on fn_addr.
        let result = decode_php_version_prologue(0x100000, &buf);
        assert!(result.is_ok(), "BTI prologue should decode: {result:?}");
    }

    #[cfg(target_arch = "aarch64")]
    #[test]
    fn aarch64_prologue_rejects_non_adrp() {
        // First instruction is a NOP (0xd503201f).
        let mut buf = [0u8; 16];
        buf[0..4].copy_from_slice(&0xd503_201f_u32.to_le_bytes());
        buf[4..8].copy_from_slice(&0x9100_0000_u32.to_le_bytes());
        buf[8..12].copy_from_slice(&0xd65f_03c0_u32.to_le_bytes());
        assert!(decode_php_version_prologue(0x1000, &buf).is_err());
    }

    #[cfg(target_arch = "aarch64")]
    #[test]
    fn aarch64_prologue_rejects_missing_ret() {
        let mut buf = [0u8; 16];
        buf[0..4].copy_from_slice(&0x9000_0000_u32.to_le_bytes());
        buf[4..8].copy_from_slice(&0x9100_0000_u32.to_le_bytes());
        buf[8..12].copy_from_slice(&0xdead_beef_u32.to_le_bytes());
        assert!(decode_php_version_prologue(0x1000, &buf).is_err());
    }
}
