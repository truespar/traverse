//! Where ML procedures run and how they batch: the `device` setting of a call, else the server
//! default (`TRAVERSE_ML_DEVICE`, `auto` unless set), and the `batching` / `batchSize` /
//! `fanouts` settings of training calls. `traverse.ml.devices()` lists what's there.

use std::collections::BTreeMap;
use std::sync::{Mutex, PoisonError};

use traverse_ml::cancel::CancellationToken;
use traverse_ml::device::{self, Device, DeviceRequest};
use traverse_ml::sampling::Fanout;
use traverse_ml::selection::{Batching, DEFAULT_BATCH_SIZE, DEFAULT_FANOUTS};

use crate::algorithms::procedure::ProcedureOutput;
use crate::ml::{MlError, number, row};
use crate::types::CypherValue;

pub use traverse_ml::device::DeviceRequest as Request;

static DEFAULT: Mutex<DeviceRequest> = Mutex::new(DeviceRequest::Auto);

/// The device calls without a `device` setting ask for; the server sets it at startup. Returns
/// where they'll run, or why they can't (then each such call fails saying so).
pub fn set_default_device(request: DeviceRequest) -> Result<Device, String> {
    *DEFAULT.lock().unwrap_or_else(PoisonError::into_inner) = request;
    device::resolve(request)
}

/// A line per GPU the probe saw, for the server log; why there's none if there's none.
pub fn gpu_report() -> Vec<String> {
    let probe = device::probe();
    if probe.gpus.is_empty() {
        return vec![format!(
            "no GPU: {}",
            probe.note.as_deref().unwrap_or("none found")
        )];
    }
    probe
        .gpus
        .iter()
        .map(|g| {
            let gib = g.memory_bytes as f64 / (1u64 << 30) as f64;
            match &g.unusable {
                None => format!("GPU cuda:{} {} ({gib:.0} GiB)", g.ordinal, g.name),
                Some(why) => format!("GPU cuda:{} {} can't be used: {why}", g.ordinal, g.name),
            }
        })
        .collect()
}

fn default_request() -> DeviceRequest {
    *DEFAULT.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The device a call runs on: its `device` setting, else the default. An unusable explicit GPU is
/// an error, not a quiet CPU run.
pub(crate) fn device_of(config: &BTreeMap<String, CypherValue>) -> Result<Device, MlError> {
    let request = match config.get("device") {
        Some(CypherValue::String(s)) => s.parse().map_err(MlError::InvalidConfig)?,
        Some(CypherValue::Null) | None => default_request(),
        Some(other) => {
            return Err(MlError::BadArgumentType {
                name: "device",
                expected: "STRING",
                actual: other.type_name().to_string(),
            });
        }
    };
    device::resolve(request).map_err(MlError::InvalidConfig)
}

/// A cancel token following the running query (a call's query, or a training job's): it trips
/// when the query stops or memory runs out, like the algorithm kernels' checks, on any thread.
pub(crate) fn query_cancel() -> CancellationToken {
    let query = crate::query::current();
    CancellationToken::with_check(move || {
        query.as_ref().is_some_and(|q| q.interrupt().is_some())
            || crate::query::is_memory_exhausted()
    })
}

/// A training error, worded for the caller when the query's own limits stopped it.
pub(crate) fn interrupted(e: std::io::Error) -> String {
    if e.kind() == std::io::ErrorKind::Interrupted {
        if let Some(reason) = crate::query::interrupt() {
            return format!("the query {}", reason.as_str());
        }
        if crate::query::is_memory_exhausted() {
            return "memory limit exceeded".into();
        }
    }
    e.to_string()
}

/// The error for a step that failed with `e`: the query's own stop when that's what interrupted
/// it, else `wrap` around the message.
pub(crate) fn step_error(e: std::io::Error, wrap: impl FnOnce(String) -> MlError) -> MlError {
    if e.kind() == std::io::ErrorKind::Interrupted {
        if let Some(reason) = crate::query::interrupt() {
            return MlError::Interrupted(reason);
        }
        if crate::query::is_memory_exhausted() {
            return MlError::MemoryExhausted;
        }
    }
    wrap(e.to_string())
}

/// The error for work its cancel token stopped: the query's own stop when that's what it was,
/// else a plain cancel.
pub(crate) fn cancelled() -> MlError {
    if let Some(reason) = crate::query::interrupt() {
        return MlError::Interrupted(reason);
    }
    if crate::query::is_memory_exhausted() {
        return MlError::MemoryExhausted;
    }
    MlError::TrainingFailed("training cancelled".into())
}

/// `batching` (`'auto'`, `'fullGraph'` or `'miniBatch'`), `batchSize` and `fanouts` (two numbers,
/// neighbors per layer, `-1` for all). `batchSize` or `fanouts` alone mean `'miniBatch'`.
pub(crate) fn batching_of(config: &BTreeMap<String, CypherValue>) -> Result<Batching, MlError> {
    let bad = |m: String| Err(MlError::InvalidConfig(m));
    let batch_size = match config.get("batchSize") {
        None | Some(CypherValue::Null) => None,
        Some(v) => match number(v) {
            Some(f) if f >= 1.0 && f.fract() == 0.0 => Some(f as usize),
            _ => return bad("batchSize must be a positive integer".into()),
        },
    };
    let fanouts = match config.get("fanouts") {
        None | Some(CypherValue::Null) => None,
        Some(CypherValue::List(items)) if items.len() == 2 => {
            let mut out = [Fanout::All; 2];
            for (slot, v) in out.iter_mut().zip(items) {
                *slot = match number(v) {
                    Some(-1.0) => Fanout::All,
                    Some(f) if f >= 1.0 && f.fract() == 0.0 => Fanout::Sample(f as usize),
                    _ => {
                        return bad(
                            "fanouts are neighbors per layer: positive integers, or -1 for all"
                                .into(),
                        );
                    }
                };
            }
            Some(out)
        }
        Some(_) => {
            return bad("fanouts must list two numbers, one per layer, e.g. [25, 10]".into());
        }
    };
    let mini = Batching::MiniBatch {
        batch_size: batch_size.unwrap_or(DEFAULT_BATCH_SIZE),
        fanouts: fanouts.unwrap_or(DEFAULT_FANOUTS),
    };
    let sized = batch_size.is_some() || fanouts.is_some();
    match config.get("batching") {
        None | Some(CypherValue::Null) => Ok(if sized { mini } else { Batching::Auto }),
        Some(CypherValue::String(s)) => match s.as_str() {
            "miniBatch" => Ok(mini),
            "auto" | "fullGraph" if sized => bad(format!(
                "batchSize and fanouts apply to miniBatch, not '{s}'"
            )),
            "auto" => Ok(Batching::Auto),
            "fullGraph" => Ok(Batching::FullGraph),
            other => bad(format!(
                "batching is 'auto', 'fullGraph' or 'miniBatch', not '{other}'"
            )),
        },
        Some(other) => Err(MlError::BadArgumentType {
            name: "batching",
            expected: "STRING",
            actual: other.type_name().to_string(),
        }),
    }
}

/// `batching` as reported in `modelInfo`: `{mode: 'fullGraph'}` or `{mode: 'miniBatch',
/// batchSize, fanouts}` (fanouts `-1` = every neighbor).
pub(crate) fn batching_json(b: Batching) -> serde_json::Value {
    let fanout = |f: Fanout| match f {
        Fanout::All => -1i64,
        Fanout::Sample(k) => k as i64,
    };
    match b {
        Batching::MiniBatch {
            batch_size,
            fanouts,
        } => serde_json::json!({
            "mode": "miniBatch",
            "batchSize": batch_size,
            "fanouts": [fanout(fanouts[0]), fanout(fanouts[1])],
        }),
        Batching::FullGraph => serde_json::json!({ "mode": "fullGraph" }),
        Batching::Auto => serde_json::json!({ "mode": "auto" }),
    }
}

/// `traverse.ml.devices()`: one row per device, `device, type, name, memoryBytes, available,
/// reason, isDefault`. `device` is what a `device` setting takes (`cuda:N` or `gpu:N` alike);
/// `isDefault` marks where calls without one run. Builds without GPU support, or machines without a usable GPU, list a `gpu` row
/// saying why.
pub(crate) fn devices() -> Result<ProcedureOutput, MlError> {
    let default = device::resolve(default_request()).ok();
    let threads = std::thread::available_parallelism().map_or(1, |n| n.get());
    let mut rows = vec![row(vec![
        ("device", CypherValue::String(Device::Cpu.to_string())),
        ("type", CypherValue::String("CPU".into())),
        (
            "name",
            CypherValue::String(format!("CPU ({threads} threads)")),
        ),
        ("memoryBytes", CypherValue::Null),
        ("available", CypherValue::Bool(true)),
        ("reason", CypherValue::Null),
        ("isDefault", CypherValue::Bool(default == Some(Device::Cpu))),
    ])];
    let probe = device::probe();
    for gpu in &probe.gpus {
        let d = gpu.device();
        rows.push(row(vec![
            (
                "device",
                CypherValue::String(format!("cuda:{}", gpu.ordinal)),
            ),
            ("type", CypherValue::String("GPU".into())),
            ("name", CypherValue::String(gpu.name.clone())),
            ("memoryBytes", CypherValue::Integer(gpu.memory_bytes as i64)),
            ("available", CypherValue::Bool(d.is_some())),
            (
                "reason",
                gpu.unusable
                    .clone()
                    .map_or(CypherValue::Null, CypherValue::String),
            ),
            ("isDefault", CypherValue::Bool(d.is_some() && default == d)),
        ]));
    }
    if probe.gpus.is_empty() {
        rows.push(row(vec![
            ("device", CypherValue::String("gpu".into())),
            ("type", CypherValue::String("GPU".into())),
            ("name", CypherValue::Null),
            ("memoryBytes", CypherValue::Null),
            ("available", CypherValue::Bool(false)),
            (
                "reason",
                CypherValue::String(probe.note.clone().unwrap_or_else(|| "no GPU found".into())),
            ),
            ("isDefault", CypherValue::Bool(false)),
        ]));
    }
    Ok(ProcedureOutput {
        rows,
        columns: vec![
            "device",
            "type",
            "name",
            "memoryBytes",
            "available",
            "reason",
            "isDefault",
        ],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(pairs: &[(&str, CypherValue)]) -> BTreeMap<String, CypherValue> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect()
    }

    #[test]
    fn batching_settings() {
        let int = CypherValue::Integer;
        assert_eq!(batching_of(&cfg(&[])).unwrap(), Batching::Auto);
        assert_eq!(
            batching_of(&cfg(&[(
                "batching",
                CypherValue::String("fullGraph".into())
            )]))
            .unwrap(),
            Batching::FullGraph
        );
        assert_eq!(
            batching_of(&cfg(&[("batchSize", int(64))])).unwrap(),
            Batching::MiniBatch {
                batch_size: 64,
                fanouts: DEFAULT_FANOUTS
            }
        );
        assert_eq!(
            batching_of(&cfg(&[(
                "fanouts",
                CypherValue::List(vec![int(-1), int(5)])
            )]))
            .unwrap(),
            Batching::MiniBatch {
                batch_size: DEFAULT_BATCH_SIZE,
                fanouts: [Fanout::All, Fanout::Sample(5)]
            }
        );
        for bad in [
            cfg(&[("batchSize", int(0))]),
            cfg(&[("batchSize", CypherValue::Float(2.5))]),
            cfg(&[("fanouts", CypherValue::List(vec![int(5)]))]),
            cfg(&[("fanouts", CypherValue::List(vec![int(0), int(5)]))]),
            cfg(&[
                ("batching", CypherValue::String("fullGraph".into())),
                ("batchSize", int(8)),
            ]),
            cfg(&[("batching", CypherValue::String("sometimes".into()))]),
        ] {
            assert!(batching_of(&bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn device_settings() {
        assert!(device_of(&cfg(&[])).is_ok());
        assert_eq!(
            device_of(&cfg(&[("device", CypherValue::String("cpu".into()))])).unwrap(),
            Device::Cpu
        );
        assert!(device_of(&cfg(&[("device", CypherValue::String("tpu".into()))])).is_err());
        assert!(device_of(&cfg(&[("device", CypherValue::String("gpu:99".into()))])).is_err());
        assert!(device_of(&cfg(&[("device", CypherValue::Integer(0))])).is_err());
    }

    #[test]
    fn the_cpu_is_listed_and_gpus_explained() {
        let out = devices().unwrap();
        assert_eq!(out.rows[0][0].1, CypherValue::String("cpu".into()));
        // every unavailable device says why
        for r in &out.rows {
            let available = r.iter().find(|c| c.0 == "available").unwrap();
            let reason = r.iter().find(|c| c.0 == "reason").unwrap();
            if available.1 == CypherValue::Bool(false) {
                assert!(matches!(reason.1, CypherValue::String(_)), "{r:?}");
            }
        }
    }
}
