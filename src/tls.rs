//! Reading thread-local storage of a remote process.
//!
//! ZTS PHP keeps the per-thread executor_globals pointer in a TLS slot
//! (`_tsrm_ls_cache`, declared `__thread`). To find it from outside the
//! process we have to read the calling thread's TLS base register, since
//! `process_vm_readv` only sees the global address space, not TLS.
//!
//! We do that with a brief ptrace-attach against any one task of the target
//! PID. The TLS base register is:
//!   - x86_64: FS_BASE — read via `ptrace(PTRACE_ARCH_PRCTL, ARCH_GET_FS)`,
//!     fetched out of `user_regs_struct` indirectly via PTRACE_PEEKUSER.
//!     The cleanest portable path is `iov_from(NT_X86_XSTATE)` … but glibc
//!     also exposes FS_BASE as a regular field of `user_regs_struct`.
//!   - aarch64: TPIDR_EL0 — read via `PTRACE_GETREGSET` with `NT_ARM_TLS`.
//!
//! Once we have the TLS base, the slot at `tls_base + tcb_offset` (a constant
//! decoded from `tsrm_get_ls_cache_tcb_offset`) holds a `void*` to the
//! per-thread cache. From there: `EG = *(cache + executor_globals_offset)`.

use anyhow::{Context, Result, anyhow};
use nix::sys::ptrace;
use nix::sys::wait::{WaitStatus, waitpid};
use nix::unistd::Pid;
use std::fs;
use std::time::{Duration, Instant};

use crate::remote::Remote;

/// Decode the small accessor `tsrm_get_ls_cache_tcb_offset`, which is a
/// compiler-generated stub that loads an immediate into the return register
/// and returns. The immediate is the TLS slot offset of `_tsrm_ls_cache`
/// relative to the thread pointer.
///
/// Encoding observed across distros and PHP versions:
///
/// x86_64 (release builds):
///   48 c7 c0 ii ii ii ii   mov $imm32, %rax    (sign-extended, can be neg)
///   c3                     ret
/// Optionally preceded by `f3 0f 1e fa` (endbr64).
///
/// aarch64:
///   movz x0, #lo16, lsl #0    ; d2800000 | (imm16 << 5)
///   movk x0, #hi16, lsl #16   ; f2a00000 | (imm16 << 5)   (optional)
///   ret                       ; d65f03c0
/// Optionally preceded by `bti c` (d503245f / d503241f / d503249f / d50324df).
pub fn decode_tcb_offset(rem: &Remote, fn_addr: u64) -> Result<i64> {
    let mut buf = [0u8; 24];
    rem.read(fn_addr, &mut buf)
        .with_context(|| format!("reading tsrm_get_ls_cache_tcb_offset @ {fn_addr:#x}"))?;
    decode_tcb_offset_bytes(&buf)
}

#[cfg(target_arch = "x86_64")]
fn decode_tcb_offset_bytes(buf: &[u8]) -> Result<i64> {
    const ENDBR64: [u8; 4] = [0xf3, 0x0f, 0x1e, 0xfa];
    let off = if buf.len() >= 4 && buf[..4] == ENDBR64 {
        4
    } else {
        0
    };
    if buf.len() < off + 8 {
        return Err(anyhow!("short tcb_offset prologue: {buf:02x?}"));
    }
    let s = &buf[off..];
    // 48 c7 c0 — REX.W mov $imm32, %rax. The imm32 is sign-extended to 64.
    if s[0] == 0x48 && s[1] == 0xc7 && s[2] == 0xc0 && s[7] == 0xc3 {
        let imm = i32::from_le_bytes([s[3], s[4], s[5], s[6]]) as i64;
        return Ok(imm);
    }
    Err(anyhow!(
        "tcb_offset: expected mov imm32,%rax / ret; got {buf:02x?}"
    ))
}

#[cfg(target_arch = "aarch64")]
fn decode_tcb_offset_bytes(buf: &[u8]) -> Result<i64> {
    if buf.len() < 12 {
        return Err(anyhow!("short tcb_offset prologue: {} bytes", buf.len()));
    }
    let mut off = 0usize;
    // Optional BTI landing pad.
    let first = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
    if (first & 0xffffff1f) == 0xd503241f {
        off = 4;
    }
    if buf.len() < off + 8 {
        return Err(anyhow!("short tcb_offset after BTI"));
    }

    let mut imm: u64 = 0;
    let mut consumed_movz = false;

    // We accept up to two MOV-immediate instructions (movz then optional movk),
    // then RET. The immediate is unsigned 21 bits in practice (TLS slots are
    // small positive offsets), but treat as i64 for symmetry with x86_64.
    while off + 4 <= buf.len() {
        let w = u32::from_le_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]]);
        // RET (xzr): 0xd65f03c0
        if w == 0xd65f_03c0 {
            if !consumed_movz {
                return Err(anyhow!("tcb_offset: RET before MOVZ ({:#010x})", w));
            }
            return Ok(imm as i64);
        }
        // MOVZ Xd, #imm16, LSL #(0|16|32|48):  sf=1 opc=10 100101 hw imm16 Rd
        //   binary: 1 10 100101 hw imm16 Rd  → mask 0xff800000 == 0xd2800000
        // MOVK Xd, #imm16, LSL #(0|16|...): sf=1 opc=11 100101 hw imm16 Rd
        //   mask 0xff800000 == 0xf2800000
        let is_movz = (w & 0xff80_0000) == 0xd280_0000;
        let is_movk = (w & 0xff80_0000) == 0xf280_0000;
        if is_movz || is_movk {
            // Rd must be x0 (return register). Bits [4:0].
            if (w & 0x1f) != 0 {
                return Err(anyhow!("tcb_offset: MOV target is not x0: {:#010x}", w));
            }
            let hw = (w >> 21) & 0x3;
            let imm16 = ((w >> 5) & 0xffff) as u64;
            let shift = hw * 16;
            let chunk = imm16 << shift;
            if is_movz {
                // movz clears the entire register first
                imm = chunk;
                consumed_movz = true;
            } else {
                // movk preserves other bits, overwrites the [shift+16:shift] window
                if !consumed_movz {
                    return Err(anyhow!("tcb_offset: MOVK before MOVZ"));
                }
                let mask: u64 = 0xffff_u64 << shift;
                imm = (imm & !mask) | chunk;
            }
            off += 4;
            continue;
        }
        return Err(anyhow!(
            "tcb_offset: unexpected instruction {:#010x} at +{}",
            w,
            off
        ));
    }
    Err(anyhow!("tcb_offset: ran out of bytes before RET"))
}

/// Pick any task (thread) belonging to `pid`. Excludes the leader if
/// possible, since profilers attaching to PHP-FPM workers want to see a
/// request handler thread, but for the TLS-base read it doesn't matter —
/// every thread has its own TLS, and they all observe the same `EG` value
/// per-thread (the cache_ptr differs, but whichever we read is valid for
/// *that* thread). The caller filters frames separately.
fn pick_task(pid: i32) -> Result<i32> {
    let dir = format!("/proc/{pid}/task");
    let entries =
        fs::read_dir(&dir).with_context(|| format!("listing {dir} (target dead or no perms?)"))?;
    let mut first: Option<i32> = None;
    for e in entries.flatten() {
        if let Some(name) = e.file_name().to_str()
            && let Ok(tid) = name.parse::<i32>()
        {
            if first.is_none() {
                first = Some(tid);
            }
            // Prefer a non-leader task if we can find one. PHP main thread is
            // usually the leader (== pid); request worker threads will have
            // different tids.
            if tid != pid {
                return Ok(tid);
            }
        }
    }
    first.ok_or_else(|| anyhow!("no tasks found under {dir}"))
}

/// ptrace-attach a single task, run a closure, detach. Handles the SIGSTOP
/// dance — PTRACE_ATTACH sends SIGSTOP, we must wait for it before reading.
fn with_attached_task<R>(tid: i32, f: impl FnOnce(Pid) -> Result<R>) -> Result<R> {
    let p = Pid::from_raw(tid);
    ptrace::attach(p).with_context(|| format!("ptrace(ATTACH, {tid}) — need CAP_SYS_PTRACE or matching uid; check /proc/sys/kernel/yama/ptrace_scope"))?;

    // Wait for the SIGSTOP delivered by PTRACE_ATTACH. Bound the wait so a
    // pathological state (already-stopped, ignored signal, etc.) doesn't
    // hang us forever.
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match waitpid(p, Some(nix::sys::wait::WaitPidFlag::WNOHANG)) {
            Ok(WaitStatus::StillAlive) => {
                if Instant::now() >= deadline {
                    let _ = ptrace::detach(p, None);
                    return Err(anyhow!("timed out waiting for tid {tid} to stop"));
                }
                std::thread::sleep(Duration::from_millis(2));
            }
            Ok(WaitStatus::Stopped(_, _)) => break,
            Ok(other) => {
                let _ = ptrace::detach(p, None);
                return Err(anyhow!("unexpected wait status for tid {tid}: {other:?}"));
            }
            Err(e) => {
                let _ = ptrace::detach(p, None);
                return Err(anyhow!("waitpid({tid}): {e}"));
            }
        }
    }

    let result = f(p);

    // Always try to detach, even on error — leaving a task ptrace-stopped
    // would freeze the target's profiling thread.
    let _ = ptrace::detach(p, None);
    result
}

#[cfg(target_arch = "x86_64")]
fn read_tls_base_register(tid: i32) -> Result<u64> {
    // user_regs_struct on x86_64 contains fs_base at offset 0xa8 (168), per
    // <sys/user.h>: r15..rax (16*8) + orig_rax + rip + cs + eflags + rsp + ss
    // + fs_base + gs_base + ds + es + fs + gs.
    // We use libc::user_regs_struct for the layout but read it via
    // PTRACE_GETREGS / PTRACE_GETREGSET (NT_PRSTATUS).
    use nix::sys::ptrace;
    with_attached_task(tid, |p| {
        let regs =
            ptrace::getregs(p).with_context(|| format!("PTRACE_GETREGS({tid}) for FS_BASE"))?;
        Ok(regs.fs_base)
    })
}

#[cfg(target_arch = "aarch64")]
fn read_tls_base_register(tid: i32) -> Result<u64> {
    // aarch64 doesn't expose a `user_regs_struct.tls`. We use
    // PTRACE_GETREGSET with NT_ARM_TLS (0x401), which returns a u64 holding
    // TPIDR_EL0.
    with_attached_task(tid, |p| {
        let mut tls: u64 = 0;
        let mut iov = libc::iovec {
            iov_base: &mut tls as *mut u64 as *mut libc::c_void,
            iov_len: std::mem::size_of::<u64>(),
        };
        const NT_ARM_TLS: libc::c_uint = 0x401;
        let pid_raw = p.as_raw() as libc::pid_t;
        // Safety: ptrace is a thin syscall wrapper; the iovec is valid for
        // the duration of the call (`tls` outlives `iov`).
        let ret = unsafe {
            libc::ptrace(
                libc::PTRACE_GETREGSET,
                pid_raw,
                NT_ARM_TLS as libc::c_long,
                &mut iov as *mut libc::iovec,
            )
        };
        if ret < 0 {
            return Err(anyhow!(
                "PTRACE_GETREGSET(NT_ARM_TLS) on tid {tid}: {}",
                std::io::Error::last_os_error()
            ));
        }
        if iov.iov_len < std::mem::size_of::<u64>() {
            return Err(anyhow!(
                "PTRACE_GETREGSET returned {} bytes, expected 8",
                iov.iov_len
            ));
        }
        Ok(tls)
    })
}

/// Resolve the per-thread `executor_globals` address for a ZTS target.
///
/// Steps (returns `Err` with context on any failure):
///   1. decode `tsrm_get_ls_cache_tcb_offset` → constant TLS slot offset
///   2. ptrace-attach a task → read TLS base register (FS_BASE / TPIDR_EL0)
///   3. cache_ptr = read_u64(tls_base + tcb_offset)
///   4. eg_offset = read_u64(eg_offset_var)
///   5. EG = read_u64(cache_ptr + eg_offset)
pub fn resolve_zts_executor_globals(
    rem: &Remote,
    pid: i32,
    tcb_offset_fn: u64,
    eg_offset_var: u64,
) -> Result<u64> {
    let tcb_offset =
        decode_tcb_offset(rem, tcb_offset_fn).context("decoding tsrm_get_ls_cache_tcb_offset")?;

    let tid = pick_task(pid)?;
    let tls_base =
        read_tls_base_register(tid).with_context(|| format!("reading TLS base of tid {tid}"))?;

    let slot_addr = (tls_base as i64).wrapping_add(tcb_offset) as u64;
    let cache_ptr = rem
        .read_u64(slot_addr)
        .with_context(|| format!("reading TSRM cache slot @ {slot_addr:#x}"))?;
    if cache_ptr == 0 {
        return Err(anyhow!(
            "TSRM cache pointer is null — has the target finished initialising? \
             (Try attaching after the first request has been served.)"
        ));
    }

    let eg_offset = rem
        .read_u64(eg_offset_var)
        .with_context(|| format!("reading executor_globals_offset @ {eg_offset_var:#x}"))?;
    let eg_addr = cache_ptr.wrapping_add(eg_offset);

    tracing::debug!(
        tid,
        tls_base = format_args!("{:#x}", tls_base),
        tcb_offset,
        cache_ptr = format_args!("{:#x}", cache_ptr),
        eg_offset,
        eg_addr = format_args!("{:#x}", eg_addr),
        "resolved ZTS executor_globals via TSRM"
    );

    Ok(eg_addr)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn x86_64_decodes_negative_immediate() {
        // Real bytes from amd64 php:8.3-zts:
        //   48 c7 c0 e8 ff ff ff   mov $-0x18, %rax
        //   c3                     ret
        let buf = [0x48, 0xc7, 0xc0, 0xe8, 0xff, 0xff, 0xff, 0xc3, 0, 0, 0, 0];
        assert_eq!(decode_tcb_offset_bytes(&buf).unwrap(), -0x18);
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn x86_64_decodes_positive_immediate() {
        let buf = [0x48, 0xc7, 0xc0, 0x48, 0x01, 0x00, 0x00, 0xc3, 0, 0, 0, 0];
        assert_eq!(decode_tcb_offset_bytes(&buf).unwrap(), 0x148);
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn x86_64_skips_endbr64() {
        let buf = [
            0xf3, 0x0f, 0x1e, 0xfa, 0x48, 0xc7, 0xc0, 0xe8, 0xff, 0xff, 0xff, 0xc3,
        ];
        assert_eq!(decode_tcb_offset_bytes(&buf).unwrap(), -0x18);
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn x86_64_rejects_garbage() {
        let buf = [0u8; 12];
        assert!(decode_tcb_offset_bytes(&buf).is_err());
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn x86_64_rejects_short_buffer() {
        // mov-imm prologue starts but is truncated before ret.
        let buf = [0x48, 0xc7, 0xc0, 0xe8, 0xff, 0xff, 0xff];
        assert!(decode_tcb_offset_bytes(&buf).is_err());
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn x86_64_rejects_missing_ret() {
        // Right shape but a NOP where RET should be — must not silently accept.
        let buf = [0x48, 0xc7, 0xc0, 0xe8, 0xff, 0xff, 0xff, 0x90, 0, 0, 0, 0];
        assert!(decode_tcb_offset_bytes(&buf).is_err());
    }

    #[cfg(target_arch = "aarch64")]
    #[test]
    fn aarch64_decodes_movz_only() {
        // Real bytes from arm64 php:8.3-zts:
        //   d2a00000  movz x0, #0x0,  lsl #16
        //   f2802900  movk x0, #0x148, lsl #0
        //   d65f03c0  ret
        // imm = 0x0148 (the movk-into-the-low-half is what holds the value;
        // movz with shift=16 zeros the register then loads 0 into the high half)
        let movz = 0xd2a0_0000_u32.to_le_bytes();
        let movk = 0xf280_2900_u32.to_le_bytes();
        let ret = 0xd65f_03c0_u32.to_le_bytes();
        let mut buf = [0u8; 16];
        buf[0..4].copy_from_slice(&movz);
        buf[4..8].copy_from_slice(&movk);
        buf[8..12].copy_from_slice(&ret);
        assert_eq!(decode_tcb_offset_bytes(&buf).unwrap(), 0x148);
    }

    #[cfg(target_arch = "aarch64")]
    #[test]
    fn aarch64_decodes_movz_lsl_zero() {
        // movz x0, #0x148, lsl #0  → d2802900
        // ret
        let movz = 0xd280_2900_u32.to_le_bytes();
        let ret = 0xd65f_03c0_u32.to_le_bytes();
        let mut buf = [0u8; 16];
        buf[0..4].copy_from_slice(&movz);
        buf[4..8].copy_from_slice(&ret);
        assert_eq!(decode_tcb_offset_bytes(&buf).unwrap(), 0x148);
    }

    #[cfg(target_arch = "aarch64")]
    #[test]
    fn aarch64_skips_bti() {
        let bti = 0xd503_245f_u32.to_le_bytes();
        let movz = 0xd280_2900_u32.to_le_bytes();
        let ret = 0xd65f_03c0_u32.to_le_bytes();
        let mut buf = [0u8; 16];
        buf[0..4].copy_from_slice(&bti);
        buf[4..8].copy_from_slice(&movz);
        buf[8..12].copy_from_slice(&ret);
        assert_eq!(decode_tcb_offset_bytes(&buf).unwrap(), 0x148);
    }

    #[cfg(target_arch = "aarch64")]
    #[test]
    fn aarch64_rejects_movk_into_non_x0() {
        let movz = 0xd280_2901_u32.to_le_bytes(); // Rd=1
        let ret = 0xd65f_03c0_u32.to_le_bytes();
        let mut buf = [0u8; 12];
        buf[0..4].copy_from_slice(&movz);
        buf[4..8].copy_from_slice(&ret);
        assert!(decode_tcb_offset_bytes(&buf).is_err());
    }

    #[cfg(target_arch = "aarch64")]
    #[test]
    fn aarch64_combines_movz_low_then_movk_high() {
        // movz x0, #0xbeef, lsl #0   → d2 80 | (0xbeef << 5) | 0  = 0xd297_dde0
        // movk x0, #0xdead, lsl #16  → f2 a0 | (0xdead << 5) | 0  = 0xf2bb_d5a0
        // ret
        let movz_low = (0xd280_0000_u32 | (0xbeef_u32 << 5)).to_le_bytes();
        let movk_high = (0xf2a0_0000_u32 | (0xdead_u32 << 5)).to_le_bytes();
        let ret = 0xd65f_03c0_u32.to_le_bytes();
        let mut buf = [0u8; 16];
        buf[0..4].copy_from_slice(&movz_low);
        buf[4..8].copy_from_slice(&movk_high);
        buf[8..12].copy_from_slice(&ret);
        assert_eq!(decode_tcb_offset_bytes(&buf).unwrap(), 0xdead_beef);
    }

    #[cfg(target_arch = "aarch64")]
    #[test]
    fn aarch64_rejects_movk_before_movz() {
        // MOVK without a preceding MOVZ leaves the high bits undefined; treat
        // as malformed rather than producing a garbage offset.
        let movk = 0xf280_2900_u32.to_le_bytes();
        let ret = 0xd65f_03c0_u32.to_le_bytes();
        let mut buf = [0u8; 12];
        buf[0..4].copy_from_slice(&movk);
        buf[4..8].copy_from_slice(&ret);
        assert!(decode_tcb_offset_bytes(&buf).is_err());
    }

    #[cfg(target_arch = "aarch64")]
    #[test]
    fn aarch64_rejects_unknown_instruction() {
        // ADD x0, x0, #1 (not part of the recognized prologue).
        let add = 0x9100_0400_u32.to_le_bytes();
        let ret = 0xd65f_03c0_u32.to_le_bytes();
        let mut buf = [0u8; 12];
        buf[0..4].copy_from_slice(&add);
        buf[4..8].copy_from_slice(&ret);
        assert!(decode_tcb_offset_bytes(&buf).is_err());
    }
}
