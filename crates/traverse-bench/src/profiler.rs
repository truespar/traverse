use std::sync::{Arc, Mutex};
use std::time::Instant;

use serde::Serialize;
use tokio::time::{self, Duration};

const SAMPLE_INTERVAL: Duration = Duration::from_secs(1);

/// A single resource sample.
#[derive(Debug, Clone)]
struct Sample {
    wall: f64,
    cpu_secs: f64,
    rss_bytes: u64,
    vsize_bytes: u64,
    threads: u64,
}

/// Aggregated resource statistics (same shape as Python output).
#[derive(Debug, Clone, Serialize)]
pub struct ResourceStats {
    pub samples: usize,
    pub duration_sec: f64,
    pub cpu_pct: CpuStats,
    pub rss_mb: RssStats,
    pub vsize_mb: VsizeStats,
    pub threads: ThreadStats,
}

#[derive(Debug, Clone, Serialize)]
pub struct CpuStats {
    pub avg: f64,
    pub min: f64,
    pub max: f64,
    pub p50: f64,
    pub p95: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct RssStats {
    pub min: f64,
    pub avg: f64,
    pub max: f64,
    #[serde(rename = "final")]
    pub final_val: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct VsizeStats {
    pub max: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ThreadStats {
    pub min: u64,
    pub max: u64,
}

/// Shared state for the resource sampler.
struct Inner {
    samples: Vec<Sample>,
    query_start_idx: Option<usize>,
    epoch: Instant,
}

/// Background resource sampler that reads process metrics.
pub struct ResourceSampler {
    pid: u32,
    inner: Arc<Mutex<Inner>>,
    stop: tokio::sync::watch::Sender<bool>,
    handle: Option<tokio::task::JoinHandle<()>>,
}

// -- Platform-specific process reading ----------------------------------

/// Read process CPU time (in seconds), thread count, and virtual size.
#[cfg(unix)]
fn read_process_stats(pid: u32) -> Option<(f64, u64, u64)> {
    let data = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let close_paren = data.rfind(')')?;
    let fields: Vec<&str> = data[close_paren + 2..].split_whitespace().collect();
    let utime: u64 = fields.get(11)?.parse().ok()?;
    let stime: u64 = fields.get(12)?.parse().ok()?;
    let threads: u64 = fields.get(17)?.parse().ok()?;
    let vsize: u64 = fields.get(20)?.parse().ok()?;
    let clk_tck = unsafe { libc::sysconf(libc::_SC_CLK_TCK) } as f64;
    let cpu_secs = (utime + stime) as f64 / clk_tck;
    Some((cpu_secs, threads, vsize))
}

#[cfg(unix)]
fn read_rss_bytes(pid: u32) -> Option<u64> {
    let data = std::fs::read_to_string(format!("/proc/{pid}/statm")).ok()?;
    let rss_pages: u64 = data.split_whitespace().nth(1)?.parse().ok()?;
    let page_size = unsafe { libc::sysconf(libc::_SC_PAGE_SIZE) } as u64;
    Some(rss_pages * page_size)
}

#[cfg(windows)]
fn read_process_stats(pid: u32) -> Option<(f64, u64, u64)> {
    use std::mem;
    use windows_sys::Win32::Foundation::{CloseHandle, FILETIME};
    use windows_sys::Win32::System::ProcessStatus::{
        GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS,
    };
    use windows_sys::Win32::System::Threading::{
        GetProcessTimes, OpenProcess, PROCESS_QUERY_INFORMATION, PROCESS_VM_READ,
    };

    // SAFETY: Win32 calls on a handle we open, check and close here; the out-structs are
    // all-integer #[repr(C)] (zeroed is valid) with their size fields set
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_INFORMATION | PROCESS_VM_READ, 0, pid);
        if handle.is_null() {
            return None;
        }

        // CPU times
        let (mut creation, mut exit, mut kernel, mut user) = (
            mem::zeroed::<FILETIME>(),
            mem::zeroed::<FILETIME>(),
            mem::zeroed::<FILETIME>(),
            mem::zeroed::<FILETIME>(),
        );
        if GetProcessTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user) == 0 {
            CloseHandle(handle);
            return None;
        }
        let ft_to_secs = |ft: FILETIME| {
            let ticks = (ft.dwHighDateTime as u64) << 32 | ft.dwLowDateTime as u64;
            ticks as f64 / 10_000_000.0
        };
        let cpu_secs = ft_to_secs(kernel) + ft_to_secs(user);

        // Memory (for vsize we use PagefileUsage as a proxy)
        let mut pmc: PROCESS_MEMORY_COUNTERS = mem::zeroed();
        pmc.cb = mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
        let vsize = if GetProcessMemoryInfo(
            handle,
            &mut pmc,
            mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
        ) != 0
        {
            pmc.PagefileUsage as u64
        } else {
            0
        };

        // Thread count via snapshot
        let threads = count_threads(pid);

        CloseHandle(handle);
        Some((cpu_secs, threads, vsize))
    }
}

#[cfg(windows)]
fn count_threads(pid: u32) -> u64 {
    use std::mem;
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
    };

    // SAFETY: Win32 calls on a snapshot handle we create, check and close here; `te` is an
    // all-integer #[repr(C)] struct (zeroed is valid) with dwSize set
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
        if snap.is_null() || snap == -1_isize as *mut _ {
            return 0;
        }
        let mut te: THREADENTRY32 = mem::zeroed();
        te.dwSize = mem::size_of::<THREADENTRY32>() as u32;
        let mut count = 0u64;
        if Thread32First(snap, &mut te) != 0 {
            loop {
                if te.th32OwnerProcessID == pid {
                    count += 1;
                }
                if Thread32Next(snap, &mut te) == 0 {
                    break;
                }
            }
        }
        CloseHandle(snap);
        count
    }
}

#[cfg(windows)]
fn read_rss_bytes(pid: u32) -> Option<u64> {
    use std::mem;
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::ProcessStatus::{
        GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS,
    };
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_QUERY_INFORMATION, PROCESS_VM_READ,
    };

    // SAFETY: Win32 calls on a handle we open, check and close here; `pmc` is an all-integer
    // #[repr(C)] struct (zeroed is valid) with cb set
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_INFORMATION | PROCESS_VM_READ, 0, pid);
        if handle.is_null() {
            return None;
        }
        let mut pmc: PROCESS_MEMORY_COUNTERS = mem::zeroed();
        pmc.cb = mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
        let rss = if GetProcessMemoryInfo(
            handle,
            &mut pmc,
            mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
        ) != 0
        {
            Some(pmc.WorkingSetSize as u64)
        } else {
            None
        };
        CloseHandle(handle);
        rss
    }
}

fn sample_process(pid: u32, epoch: &Instant) -> Option<Sample> {
    let (cpu_secs, threads, vsize_bytes) = read_process_stats(pid)?;
    let rss_bytes = read_rss_bytes(pid)?;
    Some(Sample {
        wall: epoch.elapsed().as_secs_f64(),
        cpu_secs,
        rss_bytes,
        vsize_bytes,
        threads,
    })
}

// -- Statistics ---------------------------------------------------------

fn pct(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let k = (sorted.len() - 1) as f64 * p / 100.0;
    let f = k as usize;
    let c = f + 1;
    if c >= sorted.len() {
        sorted[f]
    } else {
        sorted[f] + (k - f as f64) * (sorted[c] - sorted[f])
    }
}

fn compute_stats(samples: &[Sample], wall_start: f64, wall_end: f64) -> Option<ResourceStats> {
    if samples.len() < 2 {
        return None;
    }

    let wall_dt = wall_end - wall_start;
    if wall_dt <= 0.0 {
        return None;
    }

    let mut cpu_pcts: Vec<f64> = Vec::new();
    for i in 1..samples.len() {
        let dt_wall = samples[i].wall - samples[i - 1].wall;
        let dt_cpu = samples[i].cpu_secs - samples[i - 1].cpu_secs;
        if dt_wall > 0.0 && dt_cpu >= 0.0 {
            let cpu_pct = dt_cpu / dt_wall * 100.0;
            cpu_pcts.push(cpu_pct);
        }
    }

    let rss_mb: Vec<f64> = samples
        .iter()
        .map(|s| s.rss_bytes as f64 / (1024.0 * 1024.0))
        .collect();
    let vsize_mb: Vec<f64> = samples
        .iter()
        .map(|s| s.vsize_bytes as f64 / (1024.0 * 1024.0))
        .collect();
    let thread_counts: Vec<u64> = samples.iter().map(|s| s.threads).collect();

    let mut sorted_cpu = cpu_pcts.clone();
    sorted_cpu.sort_unstable_by(|a, b| a.partial_cmp(b).unwrap());
    let mut sorted_rss = rss_mb.clone();
    sorted_rss.sort_unstable_by(|a, b| a.partial_cmp(b).unwrap());

    let cpu_avg = if sorted_cpu.is_empty() {
        0.0
    } else {
        sorted_cpu.iter().sum::<f64>() / sorted_cpu.len() as f64
    };

    let rss_avg = if sorted_rss.is_empty() {
        0.0
    } else {
        sorted_rss.iter().sum::<f64>() / sorted_rss.len() as f64
    };

    Some(ResourceStats {
        samples: samples.len(),
        duration_sec: round1(wall_dt),
        cpu_pct: CpuStats {
            avg: round1(cpu_avg),
            min: round1(sorted_cpu.first().copied().unwrap_or(0.0)),
            max: round1(sorted_cpu.last().copied().unwrap_or(0.0)),
            p50: round1(pct(&sorted_cpu, 50.0)),
            p95: round1(pct(&sorted_cpu, 95.0)),
        },
        rss_mb: RssStats {
            min: round1(sorted_rss.first().copied().unwrap_or(0.0)),
            avg: round1(rss_avg),
            max: round1(sorted_rss.last().copied().unwrap_or(0.0)),
            final_val: round1(*rss_mb.last().unwrap_or(&0.0)),
        },
        vsize_mb: VsizeStats {
            max: round1(vsize_mb.iter().cloned().fold(0.0_f64, f64::max)),
        },
        threads: ThreadStats {
            min: thread_counts.iter().copied().min().unwrap_or(0),
            max: thread_counts.iter().copied().max().unwrap_or(0),
        },
    })
}

fn round1(v: f64) -> f64 {
    (v * 10.0).round() / 10.0
}

impl ResourceSampler {
    pub fn new(pid: u32) -> Self {
        let (stop_tx, _) = tokio::sync::watch::channel(false);
        Self {
            pid,
            inner: Arc::new(Mutex::new(Inner {
                samples: Vec::new(),
                query_start_idx: None,
                epoch: Instant::now(),
            })),
            stop: stop_tx,
            handle: None,
        }
    }

    pub fn start(&mut self) {
        let pid = self.pid;
        let inner = Arc::clone(&self.inner);
        let mut stop_rx = self.stop.subscribe();

        let handle = tokio::spawn(async move {
            let mut interval = time::interval(SAMPLE_INTERVAL);
            loop {
                tokio::select! {
                    _ = interval.tick() => {}
                    _ = stop_rx.changed() => break,
                }

                let epoch = inner.lock().unwrap().epoch;
                if let Some(sample) = sample_process(pid, &epoch) {
                    inner.lock().unwrap().samples.push(sample);
                }
            }
        });
        self.handle = Some(handle);
    }

    pub fn stop(&self) {
        let _ = self.stop.send(true);
    }

    pub fn mark_query_start(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.query_start_idx = Some(inner.samples.len());
    }

    pub fn mark_query_end(&self) -> Option<ResourceStats> {
        let mut inner = self.inner.lock().unwrap();
        let start_idx = inner.query_start_idx.take()?;
        let samples = &inner.samples[start_idx..];
        if samples.len() < 2 {
            return None;
        }
        compute_stats(samples, samples[0].wall, samples.last().unwrap().wall)
    }

    pub fn get_overall_stats(&self) -> Option<ResourceStats> {
        let inner = self.inner.lock().unwrap();
        if inner.samples.len() < 2 {
            return None;
        }
        compute_stats(
            &inner.samples,
            inner.samples[0].wall,
            inner.samples.last().unwrap().wall,
        )
    }
}

// -- PID discovery ------------------------------------------------------

/// Detect server PID listening on a given port.
#[cfg(unix)]
pub fn find_server_pid(port: u16) -> Option<u32> {
    let output = std::process::Command::new("ss")
        .args(["-tlnp"])
        .output()
        .ok()?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let port_pat = format!(":{port}");

    for line in stdout.lines() {
        if line.contains(&port_pat) {
            if let Some(idx) = line.find("pid=") {
                let rest = &line[idx + 4..];
                let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
                if !digits.is_empty() {
                    return digits.parse().ok();
                }
            }
        }
    }
    None
}

#[cfg(windows)]
pub fn find_server_pid(port: u16) -> Option<u32> {
    // Use netstat to find PID listening on the given port
    let output = std::process::Command::new("netstat")
        .args(["-ano", "-p", "TCP"])
        .output()
        .ok()?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let port_pat = format!(":{port}");

    for line in stdout.lines() {
        let line = line.trim();
        if line.contains(&port_pat) && line.contains("LISTENING") {
            // Last field is the PID
            if let Some(pid_str) = line.split_whitespace().last()
                && let Ok(pid) = pid_str.parse::<u32>()
            {
                return Some(pid);
            }
        }
    }
    None
}
