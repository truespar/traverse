//! Where training and inference run, picked at runtime.
//!
//! Every build runs on the CPU (Burn's NdArray backend); `cuda` builds also run on NVIDIA GPUs
//! through Burn's CUDA backend. Training and inference code is generic over the backend: a
//! [`BackendTask`] wraps a piece of it and [`run`] executes it on a [`Device`] chosen at runtime,
//! so one binary serves machines with and without a GPU.
//!
//! GPUs are probed once, without crashing on machines that lack them: the CUDA driver and NVRTC
//! are loaded dynamically, and a device only counts after a small computation has run on it.
//! Models are stored in full precision, so a model trained on one device predicts on any other;
//! GPU results match the CPU's up to float rounding (parallel sums add in no fixed order).

use std::fmt;
use std::str::FromStr;
use std::sync::OnceLock;

use burn::tensor::backend::Backend;

/// A place to run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Device {
    /// The CPU; always available.
    Cpu,
    /// An NVIDIA GPU by CUDA ordinal (`cuda` builds).
    #[cfg(feature = "cuda")]
    Cuda(usize),
}

impl fmt::Display for Device {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Device::Cpu => f.write_str("cpu"),
            #[cfg(feature = "cuda")]
            Device::Cuda(i) => write!(f, "cuda:{i}"),
        }
    }
}

/// A device as asked for in a `device` setting: `auto`, `cpu`, `gpu` (or `cuda`), or a GPU by
/// ordinal, `gpu:1` (or `cuda:1`). Case doesn't matter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DeviceRequest {
    /// The first usable GPU, else the CPU.
    #[default]
    Auto,
    Cpu,
    /// Any usable GPU, or the one with this ordinal.
    Gpu(Option<usize>),
}

impl FromStr for DeviceRequest {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        let lower = s.trim().to_ascii_lowercase();
        let bad = || format!("unknown device '{s}'; use 'auto', 'cpu', 'gpu' or 'gpu:<n>'");
        match lower.as_str() {
            "auto" => Ok(Self::Auto),
            "cpu" => Ok(Self::Cpu),
            "gpu" | "cuda" => Ok(Self::Gpu(None)),
            other => {
                let n = other
                    .strip_prefix("gpu:")
                    .or_else(|| other.strip_prefix("cuda:"))
                    .ok_or_else(bad)?;
                n.parse().map(|i| Self::Gpu(Some(i))).map_err(|_| bad())
            }
        }
    }
}

impl fmt::Display for DeviceRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Auto => f.write_str("auto"),
            Self::Cpu => f.write_str("cpu"),
            Self::Gpu(None) => f.write_str("gpu"),
            Self::Gpu(Some(i)) => write!(f, "gpu:{i}"),
        }
    }
}

/// One GPU the probe looked at.
#[derive(Debug, Clone)]
pub struct Gpu {
    /// CUDA ordinal.
    pub ordinal: usize,
    pub name: String,
    pub memory_bytes: u64,
    /// Why it can't be used; `None` when it can.
    pub unusable: Option<String>,
}

impl Gpu {
    /// The device to run on, if this GPU is usable.
    pub fn device(&self) -> Option<Device> {
        #[cfg(feature = "cuda")]
        if self.unusable.is_none() {
            return Some(Device::Cuda(self.ordinal));
        }
        None
    }
}

/// What the probe found.
#[derive(Debug, Clone, Default)]
pub struct Probe {
    /// Every GPU seen, usable or not.
    pub gpus: Vec<Gpu>,
    /// Why no GPU was looked at (no GPU support in this build, no driver, ...).
    pub note: Option<String>,
}

/// GPUs, probed on first use and then remembered for the life of the process.
pub fn probe() -> &'static Probe {
    static PROBE: OnceLock<Probe> = OnceLock::new();
    PROBE.get_or_init(probe_now)
}

#[cfg(not(feature = "cuda"))]
fn probe_now() -> Probe {
    Probe {
        gpus: Vec::new(),
        note: Some("this build has no GPU support".into()),
    }
}

#[cfg(feature = "cuda")]
fn probe_now() -> Probe {
    use cudarc::driver::result;

    let note = |s: String| Probe {
        gpus: Vec::new(),
        note: Some(s),
    };
    // SAFETY: loads the libraries if present (running their own initialisers), nothing more
    if !unsafe { cudarc::driver::sys::is_culib_present() } {
        return note("no NVIDIA driver found".into());
    }
    // SAFETY: as above
    if !unsafe { cudarc::nvrtc::sys::is_culib_present() } {
        return note(
            "the NVIDIA driver is installed but NVRTC (the CUDA runtime compiler, in the CUDA \
             toolkit) isn't"
                .into(),
        );
    }
    if let Err(e) = result::init() {
        return note(format!("the NVIDIA driver didn't start: {e}"));
    }
    let count = match result::device::get_count() {
        Ok(n) => n.max(0) as usize,
        Err(e) => return note(format!("couldn't list GPUs: {e}")),
    };
    if count == 0 {
        return note("the NVIDIA driver reports no GPUs".into());
    }
    let gpus = (0..count)
        .map(|ordinal| {
            let info = result::device::get(ordinal as i32).and_then(|dev| {
                let name = result::device::get_name(dev)?;
                // SAFETY: `dev` was just returned by the driver for a valid ordinal
                let memory = unsafe { result::device::total_mem(dev) }?;
                Ok((name, memory as u64))
            });
            match info {
                Ok((name, memory_bytes)) => Gpu {
                    ordinal,
                    name,
                    memory_bytes,
                    unusable: smoke_test(ordinal).err().map(explain_failure),
                },
                Err(e) => Gpu {
                    ordinal,
                    name: String::new(),
                    memory_bytes: 0,
                    unusable: Some(format!("the driver couldn't describe it: {e}")),
                },
            }
        })
        .collect();
    Probe { gpus, note: None }
}

/// A failed test computation, explained: the usual cause is an NVRTC (from the CUDA toolkit on the
/// library path) newer than the driver, whose code the driver then refuses to load.
#[cfg(feature = "cuda")]
fn explain_failure(error: String) -> String {
    let driver = {
        let mut v = 0;
        // SAFETY: writes one int through a valid pointer; the driver is initialised
        let r = unsafe { cudarc::driver::sys::cuDriverGetVersion(&mut v) };
        (r == cudarc::driver::sys::CUresult::CUDA_SUCCESS).then_some((v / 1000, v % 1000 / 10))
    };
    let nvrtc = {
        let (mut major, mut minor) = (0, 0);
        // SAFETY: writes two ints through valid pointers; the library is loaded
        let r = unsafe { cudarc::nvrtc::sys::nvrtcVersion(&mut major, &mut minor) };
        (r == cudarc::nvrtc::sys::nvrtcResult::NVRTC_SUCCESS).then_some((major, minor))
    };
    match (driver, nvrtc) {
        (Some(d), Some(n)) if n > d => format!(
            "NVRTC {}.{} (from the CUDA toolkit) is newer than this NVIDIA driver supports (CUDA \
             {}.{}): update the driver, or put a CUDA {}.{} or older toolkit first on the library \
             path",
            n.0, n.1, d.0, d.1, d.0, d.1
        ),
        _ => error,
    }
}

/// Runs a small computation on GPU `ordinal`. Burn's CUDA runtime panics when it can't start
/// (kernel compilation failing, say), so that's caught here, once per GPU.
#[cfg(feature = "cuda")]
fn smoke_test(ordinal: usize) -> Result<(), String> {
    use burn::tensor::{ElementConversion, Tensor};
    type B = burn::backend::Cuda;
    let run = || {
        let device = burn::backend::cuda::CudaDevice::new(ordinal);
        let t = Tensor::<B, 1>::from_floats([1.0f32, 2.0, 3.0], &device);
        let sum: f32 = (t.clone() * t).sum().into_scalar().elem();
        sum
    };
    match std::panic::catch_unwind(run) {
        Ok(sum) if (sum - 14.0).abs() < 1e-3 => Ok(()),
        Ok(sum) => Err(format!("a test computation gave {sum} instead of 14")),
        Err(panic) => Err(format!(
            "a test computation failed: {}",
            panic
                .downcast_ref::<String>()
                .map(String::as_str)
                .or_else(|| panic.downcast_ref::<&str>().copied())
                .unwrap_or("unknown cause")
        )),
    }
}

/// Usable GPUs, in ordinal order.
pub fn usable_gpus() -> impl Iterator<Item = &'static Gpu> {
    probe().gpus.iter().filter(|g| g.unusable.is_none())
}

/// The device for a request. `Auto` takes the first usable GPU, else the CPU; asking for a GPU
/// that can't be used is an error saying why, never a quiet CPU run.
pub fn resolve(request: DeviceRequest) -> Result<Device, String> {
    let p = probe();
    let why_not = || match (&p.note, p.gpus.iter().find_map(|g| g.unusable.as_ref())) {
        (Some(note), _) => note.clone(),
        (None, Some(reason)) => reason.clone(),
        (None, None) => "no GPU found".into(),
    };
    match request {
        DeviceRequest::Cpu => Ok(Device::Cpu),
        DeviceRequest::Auto => Ok(usable_gpus().find_map(Gpu::device).unwrap_or(Device::Cpu)),
        DeviceRequest::Gpu(None) => usable_gpus()
            .find_map(Gpu::device)
            .ok_or_else(|| format!("no usable GPU: {}", why_not())),
        DeviceRequest::Gpu(Some(i)) => match p.gpus.iter().find(|g| g.ordinal == i) {
            Some(g) => g.device().ok_or_else(|| {
                format!(
                    "GPU {i} can't be used: {}",
                    g.unusable.as_deref().unwrap_or("unknown cause")
                )
            }),
            None if p.gpus.is_empty() => Err(format!("no GPU {i}: {}", why_not())),
            None => Err(format!(
                "no GPU {i}; there are {} (numbered from 0)",
                p.gpus.len()
            )),
        },
    }
}

/// Work generic over the Burn backend. [`run`] calls [`execute`](Self::execute) with the backend of
/// the chosen device; training uses `burn::backend::Autodiff<B>` on top of it.
pub trait BackendTask {
    type Output;

    fn execute<B: Backend>(self, device: B::Device) -> Self::Output;
}

/// Runs `task` on `device`.
pub fn run<T: BackendTask>(device: Device, task: T) -> T::Output {
    match device {
        Device::Cpu => task.execute::<burn::backend::NdArray>(Default::default()),
        #[cfg(feature = "cuda")]
        Device::Cuda(i) => {
            task.execute::<burn::backend::Cuda>(burn::backend::cuda::CudaDevice::new(i))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_parse_and_print() {
        for (text, want) in [
            ("auto", DeviceRequest::Auto),
            (" CPU ", DeviceRequest::Cpu),
            ("gpu", DeviceRequest::Gpu(None)),
            ("cuda", DeviceRequest::Gpu(None)),
            ("GPU:2", DeviceRequest::Gpu(Some(2))),
            ("cuda:0", DeviceRequest::Gpu(Some(0))),
        ] {
            assert_eq!(text.parse::<DeviceRequest>(), Ok(want), "{text}");
            assert_eq!(want.to_string().parse::<DeviceRequest>(), Ok(want));
        }
        for bad in ["", "tpu", "gpu:", "gpu:-1", "cuda:x"] {
            assert!(bad.parse::<DeviceRequest>().is_err(), "{bad}");
        }
    }

    #[test]
    fn the_cpu_is_always_there() {
        assert_eq!(resolve(DeviceRequest::Cpu), Ok(Device::Cpu));
        // auto never fails: a GPU when there's a usable one, the CPU otherwise
        let auto = resolve(DeviceRequest::Auto).unwrap();
        assert_eq!(auto == Device::Cpu, usable_gpus().next().is_none());
    }

    #[test]
    fn asking_for_a_missing_gpu_says_why() {
        let err = resolve(DeviceRequest::Gpu(Some(97))).unwrap_err();
        assert!(err.contains("97"), "{err}");
    }

    /// GPU validation runs set `TRAVERSE_EXPECT_GPU` so a missing or broken GPU fails loudly
    /// instead of every test quietly running on the CPU.
    #[test]
    fn an_expected_gpu_is_usable() {
        if std::env::var_os("TRAVERSE_EXPECT_GPU").is_none() {
            return;
        }
        let p = probe();
        assert!(usable_gpus().next().is_some(), "no usable GPU: {p:?}");
        assert_ne!(resolve(DeviceRequest::Gpu(None)), Ok(Device::Cpu));
    }

    struct Square(Vec<f32>);

    impl BackendTask for Square {
        type Output = Vec<f32>;

        fn execute<B: Backend>(self, device: B::Device) -> Vec<f32> {
            let t = burn::tensor::Tensor::<B, 1>::from_floats(self.0.as_slice(), &device);
            (t.clone() * t).into_data().iter::<f32>().collect()
        }
    }

    #[test]
    fn tasks_run_on_every_usable_device() {
        let mut devices = vec![Device::Cpu];
        devices.extend(usable_gpus().filter_map(Gpu::device));
        for d in devices {
            assert_eq!(
                run(d, Square(vec![1.0, -2.0, 3.0])),
                vec![1.0, 4.0, 9.0],
                "{d}"
            );
        }
    }
}
