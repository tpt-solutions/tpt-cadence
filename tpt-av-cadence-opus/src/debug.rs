//! Debug-trace flags for the oracle-diffing scaffolds (CELT_BAND_TRACE,
//! SILK_C_FS_DEBUG, SILK_DBG). Resolved once from the environment at
//! decoder construction: `std::env::var_os` allocates on EVERY call on
//! Windows, so a per-frame check would break the crate's real-time
//! (allocation-free decode) contract even when the flag is unset.

use std::sync::OnceLock;

#[derive(Default, Clone, Copy)]
pub(crate) struct DebugFlags {
    pub celt_band_trace: bool,
    pub silk_c_fs_debug: bool,
    pub silk_dbg: bool,
}

static FLAGS: OnceLock<DebugFlags> = OnceLock::new();

/// Force the environment lookup now (allocates). Every public decoder
/// constructor calls this so `flags()` never allocates during decode.
pub(crate) fn init() {
    let _ = flags();
}

pub(crate) fn flags() -> DebugFlags {
    *FLAGS.get_or_init(|| DebugFlags {
        celt_band_trace: std::env::var_os("CELT_BAND_TRACE").is_some(),
        silk_c_fs_debug: std::env::var_os("SILK_C_FS_DEBUG").is_some(),
        silk_dbg: std::env::var_os("SILK_DBG").is_some(),
    })
}
