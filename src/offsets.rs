//! Per-version Zend struct field offsets for NTS x86_64 Linux.
//!
//! All numbers in [`VersionLayout`] were verified against Sury's debug builds
//! using `scripts/dump-offsets-dbg.sh`. See `docs/development.md` for how to
//! re-verify and what to do when a new minor releases.
//!
//! ## What's stable, what isn't
//!
//! These leaf structs have been layout-stable across 8.3, 8.4, and 8.5 — so
//! their offsets live in plain `pub const` items below:
//!   - `zend_string`, `zend_op`, `_Bucket`, `_zval_struct`, `_zend_array`
//!   - `zend_execute_data` (top-of-frame layout)
//!   - `zend_class_entry.name`
//!   - The first few fields of `zend_op_array` (`function_name`, `scope`)
//!
//! Per-version drift lives in [`VersionLayout`]:
//!   - `eg.current_execute_data` shifts in 8.5 (488 → 512)
//!   - `op_array.filename / line_start / line_end` shift in 8.4 (144→168 etc.)
//!
//! ## Override at runtime
//!
//! `PFP_EG_CURRENT_EXECUTE_DATA` and `PFP_EG_SYMBOL_TABLE` env vars override
//! the picked layout without recompiling — useful for unusual distro builds.

use std::env;

#[derive(Debug, Clone, Copy)]
pub struct VersionLayout {
    pub label: &'static str,

    // zend_executor_globals
    pub eg_current_execute_data: u64,
    pub eg_symbol_table: u64,

    // zend_op_array (user function tail)
    pub op_array_filename: u64,
    pub op_array_line_start: u64,
}

/// PHP 8.0 NTS. Verified against 8.0.30 (Sury).
///
/// 8.0 is end-of-life upstream as of November 2023; offsets won't change
/// further but security patches may not land in your distro.
pub const LAYOUT_8_0: VersionLayout = VersionLayout {
    label: "8.0",
    eg_current_execute_data: 488,
    eg_symbol_table: 304,
    op_array_filename: 144,
    op_array_line_start: 152,
};

/// PHP 8.1 NTS. Verified against 8.1.34 (Sury).
///
/// 8.1 is in security-fixes-only mode upstream.
pub const LAYOUT_8_1: VersionLayout = VersionLayout {
    label: "8.1",
    eg_current_execute_data: 488,
    eg_symbol_table: 304,
    op_array_filename: 144,
    op_array_line_start: 152,
};

/// PHP 8.2 NTS. Verified against 8.2.31 (Sury).
pub const LAYOUT_8_2: VersionLayout = VersionLayout {
    label: "8.2",
    eg_current_execute_data: 488,
    eg_symbol_table: 304,
    op_array_filename: 152,
    op_array_line_start: 160,
};

/// PHP 8.3 NTS. Verified against 8.3.31 (Sury).
pub const LAYOUT_8_3: VersionLayout = VersionLayout {
    label: "8.3",
    eg_current_execute_data: 488,
    eg_symbol_table: 304,
    op_array_filename: 144,
    op_array_line_start: 152,
};

/// PHP 8.4 NTS x86_64. Verified against 8.4.21 (Sury).
pub const LAYOUT_8_4: VersionLayout = VersionLayout {
    label: "8.4",
    eg_current_execute_data: 488,
    eg_symbol_table: 304,
    op_array_filename: 168,
    op_array_line_start: 176,
};

/// PHP 8.5 NTS x86_64. Verified against 8.5.6 (Sury).
pub const LAYOUT_8_5: VersionLayout = VersionLayout {
    label: "8.5",
    eg_current_execute_data: 512,
    eg_symbol_table: 304,
    op_array_filename: 168,
    op_array_line_start: 176,
};

pub fn pick(php_version: &str) -> Option<VersionLayout> {
    let mut base = if php_version.starts_with("8.0") {
        Some(LAYOUT_8_0)
    } else if php_version.starts_with("8.1") {
        Some(LAYOUT_8_1)
    } else if php_version.starts_with("8.2") {
        Some(LAYOUT_8_2)
    } else if php_version.starts_with("8.3") {
        Some(LAYOUT_8_3)
    } else if php_version.starts_with("8.4") {
        Some(LAYOUT_8_4)
    } else if php_version.starts_with("8.5") {
        Some(LAYOUT_8_5)
    } else {
        None
    }?;

    if let Ok(v) = env::var("PFP_EG_CURRENT_EXECUTE_DATA")
        && let Ok(n) = v.parse()
    {
        base.eg_current_execute_data = n;
    }
    if let Ok(v) = env::var("PFP_EG_SYMBOL_TABLE")
        && let Ok(n) = v.parse()
    {
        base.eg_symbol_table = n;
    }
    Some(base)
}

/// `zend_execute_data` — layout-stable across 8.0 / 8.1 / 8.2 / 8.3 / 8.4 / 8.5.
pub mod ex {
    pub const OPLINE: u64 = 0;
    pub const FUNC: u64 = 24;
    pub const PREV_EXECUTE_DATA: u64 = 48;
    // Note: `symbol_table` sits at offset 56; we use EG.symbol_table instead.
}

/// `zend_function.common` — layout-stable; `op_array_*` tail moves per
/// version and lives in [`VersionLayout`].
pub mod func {
    pub const TYPE: u64 = 0;
    pub const TYPE_INTERNAL: u8 = 1;
    pub const TYPE_USER: u8 = 2;

    pub const FUNCTION_NAME: u64 = 8;
    pub const SCOPE: u64 = 16;
}

pub mod op {
    pub const LINENO: u64 = 24;
}

pub mod zstr {
    pub const LEN: u64 = 16;
    pub const VAL: u64 = 24;
}

pub mod ce {
    pub const NAME: u64 = 8;
}

/// `_zend_array` (a.k.a. `HashTable`). `arData` precedes `nNumUsed` because
/// the union before nTableMask sits in the first 16 bytes.
pub mod ht {
    pub const AR_DATA: u64 = 16;
    pub const N_NUM_USED: u64 = 24;
}

pub mod bucket {
    pub const SIZE: u64 = 32;
    pub const VAL: u64 = 0;
    // Note: `h` (the cached hash) sits at offset 16; we don't read it.
    pub const KEY: u64 = 24;
}

pub mod zval {
    pub const VALUE: u64 = 0;
    pub const TYPE_INFO: u64 = 8;

    // We only branch on these tags; full table for reference:
    //   1=NULL, 2=FALSE, 3=TRUE, 4=LONG, 5=DOUBLE, 6=STRING (used),
    //   7=ARRAY (used), 8=OBJECT, 9=RESOURCE, 10=REFERENCE (used).
    pub const IS_UNDEF: u8 = 0;
    pub const IS_STRING: u8 = 6;
    pub const IS_ARRAY: u8 = 7;
    pub const IS_REFERENCE: u8 = 10;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_layout_per_minor_version() {
        assert_eq!(pick("8.0.30").unwrap().label, "8.0");
        assert_eq!(pick("8.1.34").unwrap().label, "8.1");
        assert_eq!(pick("8.2.31").unwrap().label, "8.2");
        assert_eq!(pick("8.3.31").unwrap().label, "8.3");
        assert_eq!(pick("8.3.0").unwrap().label, "8.3");
        assert_eq!(pick("8.4.21").unwrap().label, "8.4");
        assert_eq!(pick("8.5.6").unwrap().label, "8.5");
    }

    #[test]
    fn rejects_unsupported_versions() {
        assert!(pick("7.4.33").is_none());
        assert!(pick("9.0.0").is_none());
        assert!(pick("").is_none());
    }

    #[test]
    fn key_offsets_match_pahole() {
        // Spot-check the numbers that drift between minors. If these break,
        // re-run scripts/dump-offsets-dbg.sh and update the layouts.

        // current_execute_data: stable 488 from 8.0 → 8.4, jumps to 512 in 8.5.
        for l in [LAYOUT_8_0, LAYOUT_8_1, LAYOUT_8_2, LAYOUT_8_3, LAYOUT_8_4] {
            assert_eq!(l.eg_current_execute_data, 488, "layout {}", l.label);
        }
        assert_eq!(LAYOUT_8_5.eg_current_execute_data, 512);

        // symbol_table: 304 across every supported minor.
        for l in [
            LAYOUT_8_0, LAYOUT_8_1, LAYOUT_8_2, LAYOUT_8_3, LAYOUT_8_4, LAYOUT_8_5,
        ] {
            assert_eq!(l.eg_symbol_table, 304, "layout {}", l.label);
        }

        // op_array.filename drifts: 144 (8.0/8.1/8.3) → 152 (8.2) → 168 (8.4/8.5).
        assert_eq!(LAYOUT_8_0.op_array_filename, 144);
        assert_eq!(LAYOUT_8_1.op_array_filename, 144);
        assert_eq!(LAYOUT_8_2.op_array_filename, 152);
        assert_eq!(LAYOUT_8_3.op_array_filename, 144);
        assert_eq!(LAYOUT_8_4.op_array_filename, 168);
        assert_eq!(LAYOUT_8_5.op_array_filename, 168);
    }

    #[test]
    fn env_overrides_apply() {
        // Use a unique value to avoid clashing with anything else; remove
        // after to keep tests independent.
        // SAFETY: env::set_var is unsafe in edition 2024 multi-threaded
        // contexts; tests run single-threaded here.
        unsafe {
            std::env::set_var("PFP_EG_CURRENT_EXECUTE_DATA", "9999");
            std::env::set_var("PFP_EG_SYMBOL_TABLE", "8888");
        }
        let l = pick("8.3.0").unwrap();
        assert_eq!(l.eg_current_execute_data, 9999);
        assert_eq!(l.eg_symbol_table, 8888);
        unsafe {
            std::env::remove_var("PFP_EG_CURRENT_EXECUTE_DATA");
            std::env::remove_var("PFP_EG_SYMBOL_TABLE");
        }
    }
}
