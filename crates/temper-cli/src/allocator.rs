//! jemalloc's built-in options (its `malloc_conf` C string).
//!
//! By default freed pages are purged only as the same arena keeps allocating,
//! so a burst's high-water stays resident. Here dirty pages decay within a
//! second and muzzy pages at once. On Linux a background thread does the
//! purging; jemalloc supports those threads only there (on macOS asking for
//! one leaves the allocator half-initialized). `_RJEM_MALLOC_CONF` in the
//! environment is read after this and still overrides it.

/// A pointer to jemalloc's options string, laid out as the `const char *` it reads.
#[repr(transparent)]
pub struct JemallocConf(*const std::ffi::c_char);

// SAFETY: the pointer is to a 'static C string literal that is never written.
unsafe impl Sync for JemallocConf {}

/// Read by jemalloc at startup under its prefixed name.
#[cfg(target_os = "linux")]
#[unsafe(export_name = "_rjem_malloc_conf")]
pub static JEMALLOC_CONF: JemallocConf =
    JemallocConf(c"background_thread:true,dirty_decay_ms:1000,muzzy_decay_ms:0".as_ptr());

/// Read by jemalloc at startup under its prefixed name.
#[cfg(not(target_os = "linux"))]
#[unsafe(export_name = "_rjem_malloc_conf")]
pub static JEMALLOC_CONF: JemallocConf =
    JemallocConf(c"dirty_decay_ms:1000,muzzy_decay_ms:0".as_ptr());
