//! FUR-006 hardware qualification probe: read-only UVC discovery dump.
//!
//! Uses only `furami::capture` and `furami::domain` — no Qt, no libmpv, no
//! argument parsing, no external discovery process. Prints the full snapshot as
//! JSON: physical identities kept separate from session paths, raw/effective
//! capabilities, every capture queue, every FourCC with its flags and every
//! native size/interval descriptor (discrete intervals carry both the exact
//! interval and its reciprocal rate; ranges keep native interval bounds plus
//! reversed rate bounds and defer exact per-size data to a later query).
//!
//! Then it validates the ShadowCast proof tuple on the unique device with
//! VID/PID `32ed:3701` (never a hardcoded serial): NV12 2560x1440 at 60/1 must
//! be `Supported`, YUYV on the same tuple must be
//! `Unsupported(FrameRate)`. The exact `validate` result is authoritative:
//! `Unsupported` is a valid qualification verdict, `DescriptorUnavailable` is
//! an unknown state. A missing or ambiguous device keeps the general dump and
//! marks the check `unavailable` with the typed reason — the hardware gate is
//! then BLOCKED, never PASS. A contradicting verdict is printed as `failed`
//! with the full report and then exits nonzero. Discovery, serialization and
//! stdout failures exit nonzero with the typed message before any report.
//!
//! Exit summary: 0 = passed, blocked (missing/ambiguous device) or unknown
//! descriptor state with the report on stdout; nonzero = failed proof,
//! discovery, I/O or serialization error.

use std::{io::Write, process::ExitCode};

use furami::capture::linux::{CaptureDevice, CaptureError, CaptureNode, CaptureSnapshot};
use furami::domain::capture::{
    CaptureMode, CapturedFourCc, Descriptor, DeviceIdentity, FrameInterval, FrameIntervalKind,
    FrameIntervals, FrameRate, FrameSize, FrameSizeKind, FrameSizes, ModeRequest, SupportVerdict,
    UnsupportedReason,
};
use serde::Serialize;
use serde_json::{Value, json};

const SHADOWCAST_VENDOR_ID: u16 = 0x32ed;
const SHADOWCAST_PRODUCT_ID: u16 = 0x3701;

/// Typed probe failure. Discovery keeps the original capture error; the proof
/// failure carries the verdicts that contradicted the expectations.
#[derive(Debug, thiserror::Error)]
enum ProbeError {
    #[error("discovery failed: {0}")]
    Discovery(#[from] CaptureError),
    #[error("report serialization failed: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("stdout write failed: {0}")]
    Stdout(#[source] std::io::Error),
    #[error("shadowcast proof failed: {0}")]
    Proof(String),
}

/// Why the ShadowCast device cannot be qualified at all. Kept private and
/// distinct: absence is not ambiguity, and a VID/PID multiplicity is a probe
/// selection problem, not the domain's serial-less ambiguity.
#[derive(Debug, Serialize, thiserror::Error)]
enum QualificationError {
    #[error("no device with VID/PID {vendor_id:04x}:{product_id:04x}")]
    NotFound { vendor_id: u16, product_id: u16 },
    #[error("{candidates} devices share VID/PID {vendor_id:04x}:{product_id:04x}")]
    MultipleMatchingDevices {
        candidates: usize,
        vendor_id: u16,
        product_id: u16,
    },
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            let _ = writeln!(std::io::stderr(), "capture_dump: {message}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), ProbeError> {
    let snapshot = furami::capture::linux::discover().map_err(ProbeError::Discovery)?;
    let (shadowcast_check, proof_failure) = shadowcast_check(&snapshot)?;
    let mut report = snapshot_report(&snapshot);
    report["shadowcast_check"] = shadowcast_check;

    let rendered = serde_json::to_string_pretty(&report)?;
    let mut stdout = std::io::stdout().lock();
    stdout
        .write_all(rendered.as_bytes())
        .and_then(|()| stdout.write_all(b"\n"))
        .map_err(ProbeError::Stdout)?;

    // The failed report is already on stdout; only now exit nonzero.
    match proof_failure {
        Some(detail) => Err(ProbeError::Proof(detail)),
        None => Ok(()),
    }
}

// ---------------------------------------------------------------------------
// Snapshot rendering
// ---------------------------------------------------------------------------

fn snapshot_report(snapshot: &CaptureSnapshot) -> Value {
    json!({
        "devices": snapshot
            .devices()
            .iter()
            .map(device_report)
            .collect::<Vec<Value>>(),
    })
}

fn device_report(device: &CaptureDevice) -> Value {
    json!({
        "identity": identity_report(device.identity()),
        "nodes": device.nodes().iter().map(node_report).collect::<Vec<Value>>(),
    })
}

fn identity_report(identity: &DeviceIdentity) -> Value {
    json!({
        "vendor_id": format!("{:04x}", identity.vendor_id()),
        "product_id": format!("{:04x}", identity.product_id()),
        "topology": {
            "controller": identity.topology().controller(),
            "ports": identity
                .topology()
                .ports()
                .iter()
                .map(|port| port.get())
                .collect::<Vec<u8>>(),
        },
        "serial": identity.serial(),
    })
}

fn node_report(node: &CaptureNode) -> Value {
    json!({
        "devnode": node.devnode(),
        "syspath": node.syspath(),
        "usb_syspath": node.usb_syspath(),
        "driver": node.driver(),
        "card_name": node.card_name(),
        "bus_info": node.bus_info(),
        "raw_capabilities": format!("0x{:08x}", node.raw_capabilities()),
        "raw_device_caps": format!("0x{:08x}", node.raw_device_caps()),
        "effective_capabilities": format!("0x{:08x}", node.effective_capabilities()),
        "formats": node
            .capabilities()
            .formats()
            .iter()
            .map(|format| {
                json!({
                    "buffer_type": format.buffer_type,
                    "fourcc": format.captured_fourcc.kernel_value(),
                    "fourcc_label": fourcc_label(format.captured_fourcc),
                    "description": format.description,
                    "flags": format!("0x{:08x}", format.flags),
                })
            })
            .collect::<Vec<Value>>(),
        "fourcc_capabilities": node
            .capabilities()
            .fourcc_capabilities()
            .iter()
            .map(|entry| {
                json!({
                    "fourcc": entry.captured_fourcc.kernel_value(),
                    "fourcc_label": fourcc_label(entry.captured_fourcc),
                    "sizes": match &entry.sizes {
                        Descriptor::Available(sizes) => sizes_report(sizes),
                        Descriptor::NotReported => Value::String("not_reported".into()),
                    },
                })
            })
            .collect::<Vec<Value>>(),
    })
}

/// Native frame sizes. A range is never sampled into a mode list: exact
/// intervals for an interior size require a later per-size query.
fn sizes_report(sizes: &FrameSizes) -> Value {
    match sizes.kind() {
        FrameSizeKind::Discrete(entries) => json!({
            "kind": "discrete",
            "sizes": entries
                .iter()
                .map(|entry| {
                    json!({
                        "size": size_value(&entry.size),
                        "intervals": match &entry.intervals {
                            Descriptor::Available(intervals) => intervals_report(intervals),
                            Descriptor::NotReported => {
                                Value::String("not_reported".into())
                            }
                        },
                    })
                })
                .collect::<Vec<Value>>(),
        }),
        FrameSizeKind::Stepwise {
            min,
            max,
            step_width,
            step_height,
        } => json!({
            "kind": "stepwise",
            "min": size_value(min),
            "max": size_value(max),
            "step_width": step_width.get(),
            "step_height": step_height.get(),
            "intervals": "query_required_for_exact_size",
        }),
        FrameSizeKind::Continuous { min, max } => json!({
            "kind": "continuous",
            "min": size_value(min),
            "max": size_value(max),
            "intervals": "query_required_for_exact_size",
        }),
    }
}

/// Native frame intervals. Discrete entries show the exact interval and its
/// reciprocal rate. Interval ranges keep their native bounds in seconds and
/// add the reversed rate bounds (min rate = 1/max interval, and conversely).
fn intervals_report(intervals: &FrameIntervals) -> Value {
    match intervals.kind() {
        FrameIntervalKind::Discrete(entries) => json!({
            "kind": "discrete",
            "intervals": entries
                .iter()
                .map(|interval| {
                    let rate = interval.rate();
                    json!({
                        "interval": rational(interval.numerator(), interval.denominator()),
                        "rate": rational(rate.numerator(), rate.denominator()),
                    })
                })
                .collect::<Vec<Value>>(),
        }),
        FrameIntervalKind::Stepwise { min, max, step } => {
            let mut value = json!({
                "kind": "stepwise",
                "min_interval": rational(min.numerator(), min.denominator()),
                "max_interval": rational(max.numerator(), max.denominator()),
                "step_interval": rational(step.numerator(), step.denominator()),
            });
            value["rate_bounds"] = rate_bounds(min, max);
            value
        }
        FrameIntervalKind::Continuous { min, max } => {
            let mut value = json!({
                "kind": "continuous",
                "min_interval": rational(min.numerator(), min.denominator()),
                "max_interval": rational(max.numerator(), max.denominator()),
            });
            value["rate_bounds"] = rate_bounds(min, max);
            value
        }
    }
}

/// Rate bounds are the inverses of the interval bounds: the smallest rate is
/// the reciprocal of the largest interval, and vice versa.
fn rate_bounds(min_interval: &FrameInterval, max_interval: &FrameInterval) -> Value {
    let min_rate = max_interval.rate();
    let max_rate = min_interval.rate();
    json!({
        "min": rational(min_rate.numerator(), min_rate.denominator()),
        "max": rational(max_rate.numerator(), max_rate.denominator()),
    })
}

fn size_value(size: &FrameSize) -> Value {
    json!({
        "width": size.width(),
        "height": size.height(),
    })
}

fn rational(numerator: u32, denominator: u32) -> Value {
    json!({
        "numerator": numerator,
        "denominator": denominator,
    })
}

/// Human-readable ASCII rendering of a FourCC when all bytes are printable,
/// otherwise the raw little-endian bytes in hex.
fn fourcc_label(fourcc: CapturedFourCc) -> String {
    let bytes = fourcc.bytes();
    if bytes.iter().all(|byte| (0x20..0x7f).contains(byte)) {
        String::from_utf8_lossy(&bytes).into_owned()
    } else {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }
}

// ---------------------------------------------------------------------------
// ShadowCast proof tuple
// ---------------------------------------------------------------------------

/// Returns the check object plus the failed-proof detail: `Some` only for a
/// contradicted expectation, so the caller prints the full report first and
/// then exits nonzero.
fn shadowcast_check(snapshot: &CaptureSnapshot) -> Result<(Value, Option<String>), ProbeError> {
    let candidates: Vec<&CaptureDevice> = snapshot
        .devices()
        .iter()
        .filter(|device| {
            device.identity().vendor_id() == SHADOWCAST_VENDOR_ID
                && device.identity().product_id() == SHADOWCAST_PRODUCT_ID
        })
        .collect();

    // Probe-level device selection: absence and VID/PID multiplicity are
    // distinct blocked states with their own typed errors; neither reuses the
    // domain's serial-less ambiguity, which is about identity resolution.
    let device = match candidates.as_slice() {
        [device] => device,
        [] => {
            return Ok((
                unavailable(QualificationError::NotFound {
                    vendor_id: SHADOWCAST_VENDOR_ID,
                    product_id: SHADOWCAST_PRODUCT_ID,
                }),
                None,
            ));
        }
        many => {
            return Ok((
                unavailable(QualificationError::MultipleMatchingDevices {
                    candidates: many.len(),
                    vendor_id: SHADOWCAST_VENDOR_ID,
                    product_id: SHADOWCAST_PRODUCT_ID,
                }),
                None,
            ));
        }
    };

    // The validate verdict is authoritative: Ok means proven routes,
    // Unsupported is a valid qualification verdict, DescriptorUnavailable is
    // an unknown state. Only required I/O, stale-snapshot and malformed-
    // descriptor errors are fatal and propagate.
    let nv12 = validate_tuple(snapshot, device, *b"NV12")?;
    let yuyv = validate_tuple(snapshot, device, *b"YUYV")?;

    let nv12_expected = nv12.verdict == SupportVerdict::Supported;
    let yuyv_expected = matches!(
        yuyv.verdict,
        SupportVerdict::Unsupported(UnsupportedReason::FrameRate)
    );
    let any_unknown = matches!(
        nv12.verdict,
        SupportVerdict::Unknown(_) | SupportVerdict::NeedsExactIntervals
    ) || matches!(
        yuyv.verdict,
        SupportVerdict::Unknown(_) | SupportVerdict::NeedsExactIntervals
    );
    let status = if nv12_expected && yuyv_expected {
        "passed"
    } else if any_unknown {
        "unavailable"
    } else {
        "failed"
    };

    let check = json!({
        "status": status,
        "identity": identity_report(device.identity()),
        "nv12_2560x1440_60fps": {
            "fourcc_label": fourcc_label(nv12.request.mode.captured_fourcc),
            "mode": mode_value(&nv12.request.mode),
            "verdict": verdict_value(&nv12.verdict),
            "verdict_debug": format!("{:?}", nv12.verdict),
        },
        "yuyv_2560x1440_60fps": {
            "fourcc_label": fourcc_label(yuyv.request.mode.captured_fourcc),
            "mode": mode_value(&yuyv.request.mode),
            "verdict": verdict_value(&yuyv.verdict),
            "verdict_debug": format!("{:?}", yuyv.verdict),
        },
    });
    if status == "failed" {
        let detail = format!(
            "nv12 verdict {:?}, yuyv verdict {:?}",
            nv12.verdict, yuyv.verdict
        );
        Ok((check, Some(detail)))
    } else {
        Ok((check, None))
    }
}

struct TupleCheck {
    request: ModeRequest,
    verdict: SupportVerdict,
}

/// Validates one FourCC at 2560x1440, 60/1 against the device. `Unsupported`
/// and `DescriptorUnavailable` are qualification verdicts, not failures; any
/// other error (I/O, stale snapshot, malformed descriptor) is fatal.
fn validate_tuple(
    snapshot: &CaptureSnapshot,
    device: &CaptureDevice,
    fourcc: [u8; 4],
) -> Result<TupleCheck, ProbeError> {
    let request = ModeRequest {
        identity: device.identity().clone(),
        mode: CaptureMode {
            captured_fourcc: CapturedFourCc::from_bytes(fourcc),
            size: FrameSize::new(2560, 1440)
                .map_err(|error| ProbeError::Proof(error.to_string()))?,
            rate: FrameRate::new(60, 1).map_err(|error| ProbeError::Proof(error.to_string()))?,
        },
    };
    let verdict = match furami::capture::linux::validate(snapshot, &request) {
        // Ok proves at least one route; the tuple is supported.
        Ok(_) => SupportVerdict::Supported,
        Err(CaptureError::Unsupported(reason)) => SupportVerdict::Unsupported(reason),
        Err(CaptureError::DescriptorUnavailable(reason)) => SupportVerdict::Unknown(reason),
        Err(error) => return Err(ProbeError::Discovery(error)),
    };
    Ok(TupleCheck { request, verdict })
}

fn mode_value(mode: &CaptureMode) -> Value {
    json!({
        "fourcc": mode.captured_fourcc.kernel_value(),
        "size": size_value(&mode.size),
        "rate": rational(mode.rate.numerator(), mode.rate.denominator()),
    })
}

fn verdict_value(verdict: &SupportVerdict) -> Value {
    match verdict {
        SupportVerdict::Supported => json!("Supported"),
        SupportVerdict::NeedsExactIntervals => json!("NeedsExactIntervals"),
        SupportVerdict::Unsupported(reason) => json!({ "Unsupported": reason }),
        SupportVerdict::Unknown(reason) => json!({ "Unknown": reason }),
    }
}

fn unavailable(error: QualificationError) -> Value {
    let message = error.to_string();
    json!({
        "status": "unavailable",
        "error": {
            "kind": error,
            "message": message,
        },
    })
}
