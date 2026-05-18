//! Typed remote reads of Zend VM structures.
//!
//! Hot-path reads are batched: each frame costs at most 2 fixed-size
//! `process_vm_readv` calls plus cached string lookups. See [`StackWalker`].

use anyhow::Result;
use std::collections::HashMap;
use std::sync::Arc;

use crate::offsets::{VersionLayout, bucket, ce, ex, func, ht, op, zstr, zval};
use crate::remote::Remote;

const MAX_STR_LEN: usize = 4096;

/// Bytes spanning the fields we read from `zend_execute_data`. Layout-stable
/// across 8.3/8.4/8.5; `prev_execute_data` lives at offset 48 so 64 covers it.
const EX_BUF: usize = 64;

/// Bytes spanning the fields we read from `zend_function`. The op_array
/// `filename`/`line_start`/`line_end` triple ends at offset 184 in 8.4/8.5
/// (168+16); 192 gives a 16-byte cushion.
const FUNC_BUF: usize = 192;

pub fn current_execute_data(rem: &Remote, eg_addr: u64, layout: &VersionLayout) -> Result<u64> {
    rem.read_u64(eg_addr + layout.eg_current_execute_data)
}

#[derive(Clone)]
pub struct Frame {
    pub function: Arc<str>,
    pub class: Option<Arc<str>>,
    pub file: Option<Arc<str>>,
    pub line: u32,
}

/// Reusable per-process state for stack walking. Caches resolved
/// `zend_string` addresses (function names, filenames, class names) so a
/// loop sampling the same hot code only touches each string once.
///
/// Strings are stored as `Arc<str>`, so cache hits clone an Arc rather than
/// allocating a new `String` per frame. Plus there's a small intern table
/// for the few hot constants ("{main}", "<internal>", "<unknown>") so they
/// share a single allocation across the whole process.
pub struct StackWalker {
    string_cache: HashMap<u64, Arc<str>>,
    intern_main: Arc<str>,
    intern_internal: Arc<str>,
    intern_unknown: Arc<str>,
}

impl StackWalker {
    pub fn new() -> Self {
        Self {
            string_cache: HashMap::with_capacity(256),
            intern_main: Arc::from("{main}"),
            intern_internal: Arc::from("<internal>"),
            intern_unknown: Arc::from("<unknown>"),
        }
    }

    /// Read one frame. Returns `(Frame, prev_execute_data)`.
    pub fn read_frame(
        &mut self,
        rem: &Remote,
        ex_addr: u64,
        layout: &VersionLayout,
    ) -> Result<(Frame, u64)> {
        // 1) one bulk read of execute_data → covers opline, func, prev.
        let ex_buf = rem.read_array::<EX_BUF>(ex_addr)?;
        let opline_ptr = read_u64(&ex_buf, ex::OPLINE);
        let func_ptr = read_u64(&ex_buf, ex::FUNC);
        let prev = read_u64(&ex_buf, ex::PREV_EXECUTE_DATA);

        let mut frame = Frame {
            function: Arc::clone(&self.intern_unknown),
            class: None,
            file: None,
            line: 0,
        };

        if func_ptr == 0 {
            return Ok((frame, prev));
        }

        // 2) one bulk read of the func struct — covers common header AND the
        // op_array tail we need (filename / line_start).
        let func_buf = rem.read_array::<FUNC_BUF>(func_ptr)?;
        let ftype = func_buf[func::TYPE as usize];
        let name_ptr = read_u64(&func_buf, func::FUNCTION_NAME);
        let scope_ptr = read_u64(&func_buf, func::SCOPE);

        frame.function = if name_ptr == 0 {
            Arc::clone(&self.intern_main)
        } else {
            self.read_zstring_cached(rem, name_ptr)?
        };

        if scope_ptr != 0 {
            // ce.name is one extra 8-byte read; class names get cached too.
            let class_name_ptr = rem.read_u64(scope_ptr + ce::NAME)?;
            if class_name_ptr != 0 {
                frame.class = Some(self.read_zstring_cached(rem, class_name_ptr)?);
            }
        }

        if ftype == func::TYPE_USER {
            let file_ptr = read_u64(&func_buf, layout.op_array_filename);
            if file_ptr != 0 {
                frame.file = Some(self.read_zstring_cached(rem, file_ptr)?);
            }
            if opline_ptr != 0 {
                // lineno is the only field we need from the opline; one u32.
                frame.line = rem.read_u32(opline_ptr + op::LINENO).unwrap_or(0);
            }
            if frame.line == 0 {
                frame.line = read_u32(&func_buf, layout.op_array_line_start);
            }
        } else if ftype == func::TYPE_INTERNAL {
            frame.file = Some(Arc::clone(&self.intern_internal));
        }

        Ok((frame, prev))
    }

    fn read_zstring_cached(&mut self, rem: &Remote, addr: u64) -> Result<Arc<str>> {
        if let Some(s) = self.string_cache.get(&addr) {
            return Ok(Arc::clone(s));
        }
        let s: Arc<str> = Arc::from(read_zend_string(rem, addr)?);
        // Bound the cache so a long-running profile of dynamic-eval-heavy
        // code can't grow it unboundedly. 8k entries ≈ a few MB of strings.
        if self.string_cache.len() < 8192 {
            self.string_cache.insert(addr, Arc::clone(&s));
        }
        Ok(s)
    }
}

impl Default for StackWalker {
    fn default() -> Self {
        Self::new()
    }
}

#[inline]
fn read_u64(buf: &[u8], off: u64) -> u64 {
    let off = off as usize;
    u64::from_le_bytes(buf[off..off + 8].try_into().unwrap())
}

#[inline]
fn read_u32(buf: &[u8], off: u64) -> u32 {
    let off = off as usize;
    u32::from_le_bytes(buf[off..off + 4].try_into().unwrap())
}

pub fn read_zend_string(rem: &Remote, addr: u64) -> Result<String> {
    let len = rem.read_u64(addr + zstr::LEN)? as usize;
    let len = len.min(MAX_STR_LEN);
    if len == 0 {
        return Ok(String::new());
    }
    let bytes = rem.read_vec(addr + zstr::VAL, len)?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

pub fn ht_get_string(rem: &Remote, ht_addr: u64, want_key: &str) -> Result<Option<String>> {
    let n_used = rem.read_u32(ht_addr + ht::N_NUM_USED)? as u64;
    let ar_data = rem.read_u64(ht_addr + ht::AR_DATA)?;
    if ar_data == 0 || n_used == 0 {
        return Ok(None);
    }
    for i in 0..n_used.min(4096) {
        let b = ar_data + i * bucket::SIZE;
        let type_info = rem.read_u32(b + bucket::VAL + zval::TYPE_INFO).unwrap_or(0);
        let tag = (type_info & 0xff) as u8;
        if tag == zval::IS_UNDEF {
            continue;
        }
        let key_ptr = rem.read_u64(b + bucket::KEY).unwrap_or(0);
        if key_ptr == 0 {
            continue;
        }
        let key = match read_zend_string(rem, key_ptr) {
            Ok(k) => k,
            Err(_) => continue,
        };
        if key != want_key {
            continue;
        }
        let real_tag = if tag == zval::IS_REFERENCE {
            let ref_ptr = rem.read_u64(b + bucket::VAL + zval::VALUE)?;
            if ref_ptr == 0 {
                return Ok(None);
            }
            let inner_type = rem.read_u32(ref_ptr + 16 + zval::TYPE_INFO).unwrap_or(0);
            let inner_tag = (inner_type & 0xff) as u8;
            if inner_tag == zval::IS_STRING {
                let s = rem.read_u64(ref_ptr + 16 + zval::VALUE)?;
                return Ok(Some(read_zend_string(rem, s)?));
            }
            inner_tag
        } else {
            tag
        };
        if real_tag == zval::IS_STRING {
            let s = rem.read_u64(b + bucket::VAL + zval::VALUE)?;
            return Ok(Some(read_zend_string(rem, s)?));
        }
        return Ok(None);
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_u64_helper_decodes_le() {
        let mut buf = [0u8; 64];
        buf[24..32].copy_from_slice(&0x_DEAD_BEEF_CAFE_BABE_u64.to_le_bytes());
        assert_eq!(read_u64(&buf, 24), 0x_DEAD_BEEF_CAFE_BABE);
    }

    #[test]
    fn read_u32_helper_decodes_le() {
        let mut buf = [0u8; 32];
        buf[8..12].copy_from_slice(&0xAABB_CCDD_u32.to_le_bytes());
        assert_eq!(read_u32(&buf, 8), 0xAABB_CCDD);
    }
}
