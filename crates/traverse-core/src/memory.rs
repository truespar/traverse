//! Process memory tracking for budget enforcement.
//!
//! Reads process RSS on demand (`/proc/self/statm` on Linux, the process memory
//! counters on Windows) instead of wrapping the global allocator, which would
//! tax every allocation. Elsewhere (and in unit tests) callers fall back to
//! `Graph::estimated_memory()` via `is_tracking_active()`.
//!
//! RSS is cached and refreshed every 256 calls to keep syscalls off hot write
//! paths (CREATE/SET).

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

static TRACKING_ACTIVE: AtomicBool = AtomicBool::new(false);
static CACHED_RSS: AtomicUsize = AtomicUsize::new(0);
static CALL_COUNT: AtomicUsize = AtomicUsize::new(0);
static MEMORY_CEILING: AtomicUsize = AtomicUsize::new(0);
/// Set once a ceiling is configured (including "no limit"); until then [`memory_ceiling`] uses the
/// default.
static CEILING_CONFIGURED: AtomicBool = AtomicBool::new(false);
/// Lazy default ceiling (90 % of physical memory), so server, CLI and embedded bindings are
/// protected without config. `0` = couldn't detect.
static DEFAULT_CEILING: std::sync::OnceLock<usize> = std::sync::OnceLock::new();

/// Share of physical memory used as the ceiling when none is configured.
pub const DEFAULT_CEILING_PERCENT: usize = 90;

const REFRESH_INTERVAL: usize = 256;

/// Process RSS in bytes, cached and refreshed every 256 calls. 0 if the read
/// fails.
#[inline]
pub fn allocated_bytes() -> usize {
    let count = CALL_COUNT.fetch_add(1, Ordering::Relaxed);
    if count.is_multiple_of(REFRESH_INTERVAL) {
        let rss = process_rss().unwrap_or(0);
        CACHED_RSS.store(rss, Ordering::Relaxed);
        rss
    } else {
        CACHED_RSS.load(Ordering::Relaxed)
    }
}

#[cfg(target_os = "linux")]
fn process_rss() -> Option<usize> {
    use std::io::Read;
    let mut buf = [0u8; 128];
    let mut f = std::fs::File::open("/proc/self/statm").ok()?;
    let n = f.read(&mut buf).ok()?;
    let s = std::str::from_utf8(&buf[..n]).ok()?;
    // /proc/self/statm: size resident shared text lib data dt; field 2 = RSS in pages
    let rss_pages: usize = s.split_whitespace().nth(1)?.parse().ok()?;
    Some(rss_pages * 4096)
}

#[cfg(target_os = "windows")]
fn process_rss() -> Option<usize> {
    #[repr(C)]
    #[allow(non_snake_case)]
    struct ProcessMemoryCounters {
        cb: u32,
        PageFaultCount: u32,
        PeakWorkingSetSize: usize,
        WorkingSetSize: usize,
        QuotaPeakPagedPoolUsage: usize,
        QuotaPagedPoolUsage: usize,
        QuotaPeakNonPagedPoolUsage: usize,
        QuotaNonPagedPoolUsage: usize,
        PagefileUsage: usize,
        PeakPagefileUsage: usize,
    }

    unsafe extern "system" {
        // kernel32
        fn GetCurrentProcess() -> *mut core::ffi::c_void;
    }

    #[link(name = "psapi")]
    unsafe extern "system" {
        fn GetProcessMemoryInfo(
            process: *mut core::ffi::c_void,
            ppsmemCounters: *mut ProcessMemoryCounters,
            cb: u32,
        ) -> i32;
    }

    // SAFETY: an all-integer #[repr(C)] struct (zeroed is valid) that the call fills in; `cb`
    // tells it the size, and GetCurrentProcess returns a pseudo-handle that needs no closing.
    unsafe {
        let mut pmc = core::mem::zeroed::<ProcessMemoryCounters>();
        pmc.cb = core::mem::size_of::<ProcessMemoryCounters>() as u32;
        let ok = GetProcessMemoryInfo(GetCurrentProcess(), &mut pmc, pmc.cb);
        if ok != 0 {
            Some(pmc.WorkingSetSize)
        } else {
            None
        }
    }
}

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
fn process_rss() -> Option<usize> {
    None
}

/// Sets the global memory ceiling in bytes (`0` disables it). Call once at
/// startup; until then the ceiling is [`DEFAULT_CEILING_PERCENT`] of physical
/// memory (see [`memory_ceiling`]).
pub fn set_memory_ceiling(bytes: usize) {
    MEMORY_CEILING.store(bytes, Ordering::Release);
    CEILING_CONFIGURED.store(true, Ordering::Release);
}

/// Memory ceiling: the configured value, else 90 % of physical memory. `None`
/// when explicitly disabled or when physical memory is unknown (WASM, unknown
/// platforms). The default protects the CLI and embedded bindings, which have no
/// config step.
pub fn memory_ceiling() -> Option<usize> {
    if CEILING_CONFIGURED.load(Ordering::Acquire) {
        let v = MEMORY_CEILING.load(Ordering::Acquire);
        return if v == 0 { None } else { Some(v) };
    }
    let v = *DEFAULT_CEILING.get_or_init(default_memory_ceiling);
    if v == 0 { None } else { Some(v) }
}

fn default_memory_ceiling() -> usize {
    total_memory().map_or(0, |total| total / 100 * DEFAULT_CEILING_PERCENT)
}

/// Fresh (uncached) memory reading in bytes; costlier than `allocated_bytes()` but exact.
pub fn allocated_bytes_fresh() -> usize {
    let rss = process_rss().unwrap_or(0);
    CACHED_RSS.store(rss, Ordering::Relaxed);
    rss
}

/// Checks usage against the ceiling with a fresh reading; `MemoryLimitExceeded` if over.
pub fn check_memory(_context: &str) -> crate::error::Result<()> {
    if let Some(ceiling) = memory_ceiling() {
        let used = allocated_bytes_fresh();
        if used > ceiling {
            return Err(crate::error::Error::MemoryLimitExceeded {
                current: used,
                limit: ceiling,
            });
        }
    }
    Ok(())
}

/// Checks that `bytes` more would stay under the ceiling, before an allocation sized by counts read
/// from outside (a file); `MemoryLimitExceeded` if not.
pub fn check_allocation(bytes: usize) -> crate::error::Result<()> {
    if let Some(ceiling) = memory_ceiling() {
        let wanted = allocated_bytes_fresh().saturating_add(bytes);
        if wanted > ceiling {
            return Err(crate::error::Error::MemoryLimitExceeded {
                current: wanted,
                limit: ceiling,
            });
        }
    }
    Ok(())
}

/// Total physical memory in bytes, `None` if unknown.
pub fn total_memory() -> Option<usize> {
    #[cfg(target_os = "linux")]
    {
        let content = std::fs::read_to_string("/proc/meminfo").ok()?;
        for line in content.lines() {
            if let Some(rest) = line.strip_prefix("MemTotal:") {
                let kb: usize = rest.trim().strip_suffix("kB")?.trim().parse().ok()?;
                return Some(kb * 1024);
            }
        }
        None
    }
    #[cfg(target_os = "windows")]
    {
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn GlobalMemoryStatusEx(lpBuffer: *mut MemoryStatusEx) -> i32;
        }
        #[repr(C)]
        struct MemoryStatusEx {
            dw_length: u32,
            dw_memory_load: u32,
            ull_total_phys: u64,
            ull_avail_phys: u64,
            ull_total_page_file: u64,
            ull_avail_page_file: u64,
            ull_total_virtual: u64,
            ull_avail_virtual: u64,
            ull_avail_extended_virtual: u64,
        }
        // SAFETY: all-integer #[repr(C)] struct, so zeroed is a valid value
        let mut status: MemoryStatusEx = unsafe { std::mem::zeroed() };
        status.dw_length = std::mem::size_of::<MemoryStatusEx>() as u32;
        // SAFETY: `status` is a live, correctly sized MEMORYSTATUSEX (dw_length set above)
        if unsafe { GlobalMemoryStatusEx(&mut status) } != 0 {
            Some(status.ull_total_phys as usize)
        } else {
            None
        }
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    {
        None
    }
}

/// Pre-flight check before loading a `file_bytes` file: estimates peak as 4x the file size
/// (buffer + deserialized graph). `MemoryLimitExceeded` if over the ceiling.
pub fn check_memory_preflight(file_bytes: u64) -> crate::error::Result<()> {
    if let Some(ceiling) = memory_ceiling() {
        let used = allocated_bytes_fresh();
        let estimate = file_bytes as usize * 3;
        if used + estimate > ceiling {
            return Err(crate::error::Error::MemoryLimitExceeded {
                current: used,
                limit: ceiling,
            });
        }
    }
    Ok(())
}

/// Marks RSS tracking active. Call once at server startup.
pub fn activate_tracking() {
    TRACKING_ACTIVE.store(true, Ordering::Release);
}

/// RSS tracking available?
#[inline]
pub fn is_tracking_active() -> bool {
    TRACKING_ACTIVE.load(Ordering::Acquire)
}
