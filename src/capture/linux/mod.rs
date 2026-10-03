//! Linux USB UVC discovery and private libpulse recording control.
//!
//! Node classification uses `device_caps` when QUERYCAP reports DEVICE_CAPS,
//! otherwise its global `capabilities`. VIDEO_CAPTURE and VIDEO_CAPTURE_MPLANE
//! queues qualify independently, with no STREAMING requirement. Metadata-only
//! and output-only nodes do not qualify; mixed nodes expose only capture queues.
//!
//! Observations group nodes by physical USB ancestry, not by a device number or
//! human-readable name. Discovery is strict all-or-error, but a snapshot is not
//! an atomic hotplug transaction. Deferred queries revalidate observed linkage,
//! physical identity, the opened device number and node-local capabilities.

use std::{
    io,
    os::fd::AsRawFd,
    path::{Path, PathBuf},
};

use serde::Serialize;

use crate::domain::capture::{
    CaptureBufferType, CaptureDataError, CaptureMode, CapturedFourCc, Descriptor, DeviceIdentity,
    FrameIntervals, FrameSize, IdentityError, ModeRequest, NodeCapabilities, SupportVerdict,
    UnknownReason, UnsupportedReason, resolve_identity,
};

mod ffi;
pub(crate) mod pulse;
mod udev;
mod v4l2;

/// Keep malformed device data, unavailable descriptors, unsupported tuples and
/// operating-system failures distinct. I/O variants retain the original errno.
#[derive(Debug, thiserror::Error)]
pub enum CaptureError {
    #[error("no video4linux nodes reported")]
    NoVideoNodes,
    #[error("no eligible USB UVC video-capture nodes reported")]
    NoCaptureNodes,
    #[error("{operation} failed without a reliable operating-system error")]
    UdevFailure { operation: &'static str },
    #[error("{operation} failed: {source}")]
    Udev {
        operation: &'static str,
        #[source]
        source: io::Error,
    },
    #[error("permission denied during {operation} for {path:?}: {source}")]
    PermissionDenied {
        path: PathBuf,
        operation: &'static str,
        #[source]
        source: io::Error,
    },
    #[error("device disappeared during {operation} for {path:?}: {source}")]
    DeviceGone {
        path: PathBuf,
        operation: &'static str,
        #[source]
        source: io::Error,
    },
    #[error("{operation} failed for {path:?}: {source}")]
    Io {
        path: PathBuf,
        operation: &'static str,
        #[source]
        source: io::Error,
    },
    #[error("UVC node {syspath:?} has no devnode")]
    MissingDevnode { syspath: PathBuf },
    #[error("USB device {syspath:?} has no required {attribute} attribute")]
    MissingUsbAttribute {
        syspath: PathBuf,
        attribute: &'static str,
    },
    #[error("USB device {syspath:?} reports an invalid {attribute} attribute")]
    InvalidUsbAttribute {
        syspath: PathBuf,
        attribute: &'static str,
    },
    #[error("malformed {operation} descriptor at index {index} for {path:?}: {source}")]
    MalformedDescriptor {
        path: PathBuf,
        operation: &'static str,
        index: u32,
        #[source]
        source: CaptureDataError,
    },
    #[error(transparent)]
    Data(#[from] CaptureDataError),
    #[error(transparent)]
    Identity(#[from] IdentityError),
    #[error("capture snapshot no longer matches {path:?}")]
    StaleSnapshot { path: PathBuf },
    #[error("node {path:?} does not belong to the requested capture device")]
    InvalidNode { path: PathBuf },
    #[error("capture tuple unsupported at {0:?}")]
    Unsupported(UnsupportedReason),
    #[error("capture descriptor unavailable: {0:?}")]
    DescriptorUnavailable(UnknownReason),
}

/// Read-only observations from one discovery pass, not an atomic hotplug view.
#[derive(Debug, Serialize)]
pub struct CaptureSnapshot {
    devices: Vec<CaptureDevice>,
}

impl CaptureSnapshot {
    pub fn devices(&self) -> &[CaptureDevice] {
        &self.devices
    }
}

/// One physical USB adapter. Its technical identity is stored once, separate
/// from the session paths and human-readable information of its capture nodes.
#[derive(Debug, Serialize)]
pub struct CaptureDevice {
    identity: DeviceIdentity,
    nodes: Vec<CaptureNode>,
}

impl CaptureDevice {
    pub fn identity(&self) -> &DeviceIdentity {
        &self.identity
    }

    pub fn nodes(&self) -> &[CaptureNode] {
        &self.nodes
    }
}

/// One eligible V4L2 node, with only the capture queues it actually advertises.
#[derive(Debug, Serialize)]
pub struct CaptureNode {
    devnode: PathBuf,
    syspath: PathBuf,
    usb_syspath: PathBuf,
    driver: String,
    card_name: String,
    bus_info: String,
    raw_capabilities: u32,
    raw_device_caps: u32,
    effective_capabilities: u32,
    capabilities: NodeCapabilities,
}

impl CaptureNode {
    pub fn devnode(&self) -> &Path {
        &self.devnode
    }

    pub fn syspath(&self) -> &Path {
        &self.syspath
    }

    pub fn usb_syspath(&self) -> &Path {
        &self.usb_syspath
    }

    pub fn driver(&self) -> &str {
        &self.driver
    }

    pub fn card_name(&self) -> &str {
        &self.card_name
    }

    pub fn bus_info(&self) -> &str {
        &self.bus_info
    }

    pub fn raw_capabilities(&self) -> u32 {
        self.raw_capabilities
    }

    pub fn raw_device_caps(&self) -> u32 {
        self.raw_device_caps
    }

    pub fn effective_capabilities(&self) -> u32 {
        self.effective_capabilities
    }

    pub fn capabilities(&self) -> &NodeCapabilities {
        &self.capabilities
    }
}

/// Exact intervals reported for one specific FourCC and size.
#[derive(Debug, Serialize)]
pub struct ExactSizeIntervals {
    pub captured_fourcc: CapturedFourCc,
    pub size: FrameSize,
    pub intervals: Descriptor<FrameIntervals>,
}

/// One snapshot node and one genuinely announced capture-buffer queue.
#[derive(Debug, Serialize)]
pub struct ValidatedRoute<'a> {
    node: &'a CaptureNode,
    buffer_type: CaptureBufferType,
}

impl<'a> ValidatedRoute<'a> {
    pub fn node(&self) -> &'a CaptureNode {
        self.node
    }

    pub fn buffer_type(&self) -> CaptureBufferType {
        self.buffer_type
    }
}

/// All proven routes for a requested tuple. No implicit first-node selection.
#[derive(Debug, Serialize)]
pub struct ValidatedCapture<'a> {
    identity: &'a DeviceIdentity,
    mode: CaptureMode,
    routes: Vec<ValidatedRoute<'a>>,
}

impl<'a> ValidatedCapture<'a> {
    pub fn identity(&self) -> &'a DeviceIdentity {
        self.identity
    }

    pub fn mode(&self) -> CaptureMode {
        self.mode
    }

    pub fn routes(&self) -> &[ValidatedRoute<'a>] {
        &self.routes
    }
}

/// Discover every eligible USB UVC capture node. Any examined candidate's
/// access or descriptor failure aborts the pass instead of hiding that device.
pub fn discover() -> Result<CaptureSnapshot, CaptureError> {
    let constants = ffi::bridge::v4l2_constants();
    snapshot_from_observations(udev::scan()?, |device, observed| {
        let file = udev::open_node(&observed.devnode)?;
        let raw = v4l2::query_cap(file.as_raw_fd(), &observed.devnode, &constants)?;
        if ffi::capture_buffer_types(raw.effective_capabilities, &constants)
            .next()
            .is_none()
        {
            return Ok(None);
        }
        let capabilities = v4l2::node_capabilities(
            file.as_raw_fd(),
            &observed.devnode,
            raw.effective_capabilities,
            &constants,
        )?;
        Ok(Some(CaptureNode {
            devnode: observed.devnode.clone(),
            syspath: observed.syspath.clone(),
            usb_syspath: device.usb_syspath.clone(),
            driver: raw.driver,
            card_name: raw.card_name,
            bus_info: raw.bus_info,
            raw_capabilities: raw.raw_capabilities,
            raw_device_caps: raw.raw_device_caps,
            effective_capabilities: raw.effective_capabilities,
            capabilities,
        }))
    })
}

fn snapshot_from_observations(
    observations: udev::Observations,
    mut inspect: impl FnMut(
        &udev::ObservedDevice,
        &udev::NodeObservation,
    ) -> Result<Option<CaptureNode>, CaptureError>,
) -> Result<CaptureSnapshot, CaptureError> {
    if observations.raw_video_nodes == 0 {
        return Err(CaptureError::NoVideoNodes);
    }
    let mut devices = Vec::new();
    for observed in observations.devices {
        let mut nodes = Vec::new();
        for node in &observed.nodes {
            if let Some(node) = inspect(&observed, node)? {
                nodes.push(node);
            }
        }
        if !nodes.is_empty() {
            devices.push(CaptureDevice {
                identity: observed.identity,
                nodes,
            });
        }
    }
    if devices.is_empty() {
        return Err(CaptureError::NoCaptureNodes);
    }
    Ok(CaptureSnapshot { devices })
}

fn check_query_target(
    device: &CaptureDevice,
    node: &CaptureNode,
    fourcc: CapturedFourCc,
    size: FrameSize,
) -> Result<(), CaptureError> {
    if !device
        .nodes
        .iter()
        .any(|candidate| std::ptr::eq(candidate, node))
    {
        return Err(CaptureError::InvalidNode {
            path: node.devnode.clone(),
        });
    }
    let Some(capabilities) = node
        .capabilities
        .fourcc_capabilities()
        .iter()
        .find(|entry| entry.captured_fourcc == fourcc)
    else {
        return Err(CaptureError::Unsupported(UnsupportedReason::FourCc));
    };
    let Some(sizes) = capabilities.sizes.as_option() else {
        return Err(CaptureError::DescriptorUnavailable(
            UnknownReason::FrameSizesNotReported,
        ));
    };
    if !sizes.contains(size) {
        return Err(CaptureError::Unsupported(UnsupportedReason::FrameSize));
    }
    Ok(())
}

fn check_fresh_capabilities(
    node: &CaptureNode,
    raw: &ffi::bridge::RawCapabilities,
    constants: &ffi::bridge::V4l2Constants,
) -> Result<(), CaptureError> {
    v4l2::check_error(raw.error, &node.devnode, "query_cap")?;
    if raw.capabilities != node.raw_capabilities
        || raw.device_caps != node.raw_device_caps
        || ffi::effective_capabilities(raw, constants) != node.effective_capabilities
    {
        return Err(CaptureError::StaleSnapshot {
            path: node.devnode.clone(),
        });
    }
    Ok(())
}

/// Query intervals only for an advertised FourCC and exact in-snapshot size.
/// Revalidate observed physical identity, linkage, opened device number and
/// QUERYCAP bits before any deferred interval ioctl. No automatic retry.
pub fn query_intervals(
    device: &CaptureDevice,
    node: &CaptureNode,
    fourcc: CapturedFourCc,
    size: FrameSize,
) -> Result<ExactSizeIntervals, CaptureError> {
    check_query_target(device, node, fourcc, size)?;
    let file = udev::revalidate_node(
        &node.syspath,
        &node.devnode,
        &node.usb_syspath,
        &device.identity,
    )?;
    let constants = ffi::bridge::v4l2_constants();
    let raw = ffi::bridge::query_cap(file.as_raw_fd());
    check_fresh_capabilities(node, &raw, &constants)?;
    let intervals = v4l2::intervals(file.as_raw_fd(), &node.devnode, fourcc, size, &constants)?;
    Ok(ExactSizeIntervals {
        captured_fourcc: fourcc,
        size,
        intervals,
    })
}

/// Recheck the explicitly selected route immediately before backend handoff.
/// This read-only descriptor is closed here; it never certifies libmpv's capture.
pub(crate) fn revalidate_route(
    identity: &DeviceIdentity,
    route: &ValidatedRoute<'_>,
) -> Result<(), CaptureError> {
    let node = route.node();
    let file = udev::revalidate_node(&node.syspath, &node.devnode, &node.usb_syspath, identity)?;
    let constants = ffi::bridge::v4l2_constants();
    let raw = ffi::bridge::query_cap(file.as_raw_fd());
    check_fresh_capabilities(node, &raw, &constants)
}

/// Resolve physical identity first, then prove the tuple on every genuinely
/// announced route. Missing optional data does not veto a proven route, but
/// required I/O and malformed-descriptor errors always propagate.
pub fn validate<'a>(
    snapshot: &'a CaptureSnapshot,
    request: &ModeRequest,
) -> Result<ValidatedCapture<'a>, CaptureError> {
    validate_with_query(snapshot, request, query_intervals)
}

/// Internal seam: identical validation with an injected exact-size interval
/// query, so range-descriptor fixtures never touch real `/dev` nodes.
pub(crate) fn validate_with_injected_query<'a>(
    snapshot: &'a CaptureSnapshot,
    request: &ModeRequest,
    query: impl FnMut(
        &CaptureDevice,
        &CaptureNode,
        CapturedFourCc,
        FrameSize,
    ) -> Result<ExactSizeIntervals, CaptureError>,
) -> Result<ValidatedCapture<'a>, CaptureError> {
    validate_with_query(snapshot, request, query)
}

fn validate_with_query<'a>(
    snapshot: &'a CaptureSnapshot,
    request: &ModeRequest,
    mut query: impl FnMut(
        &CaptureDevice,
        &CaptureNode,
        CapturedFourCc,
        FrameSize,
    ) -> Result<ExactSizeIntervals, CaptureError>,
) -> Result<ValidatedCapture<'a>, CaptureError> {
    let index = resolve_identity(
        &request.identity,
        snapshot.devices.iter().map(|device| &device.identity),
    )?;
    let device = &snapshot.devices[index];
    let mut routes = Vec::new();
    let mut unsupported = UnsupportedReason::FourCc;
    let mut unavailable = None;
    for node in &device.nodes {
        let Some(first) = node
            .capabilities
            .formats()
            .iter()
            .find(|format| format.captured_fourcc == request.mode.captured_fourcc)
        else {
            continue;
        };
        let supported = match node.capabilities.assess(first.buffer_type, request.mode) {
            SupportVerdict::Supported => true,
            SupportVerdict::Unsupported(reason) => {
                if unsupported_rank(reason) > unsupported_rank(unsupported) {
                    unsupported = reason;
                }
                false
            }
            SupportVerdict::Unknown(reason) => {
                // Prefer the more precise missing interval descriptor when
                // another route lacks even sizes. Neither invents non-support.
                if unavailable.is_none() || reason == UnknownReason::FrameIntervalsNotReported {
                    unavailable = Some(reason);
                }
                false
            }
            SupportVerdict::NeedsExactIntervals => {
                // Descriptors are shared between queues of this node only.
                // Query this exact size once, never a range endpoint.
                let exact = query(
                    device,
                    node,
                    request.mode.captured_fourcc,
                    request.mode.size,
                )?;
                match exact.intervals {
                    Descriptor::NotReported => {
                        unavailable = Some(UnknownReason::FrameIntervalsNotReported);
                        false
                    }
                    Descriptor::Available(intervals) => {
                        if intervals.supports(request.mode.rate) {
                            true
                        } else {
                            unsupported = UnsupportedReason::FrameRate;
                            false
                        }
                    }
                }
            }
        };
        if supported {
            for format in node
                .capabilities
                .formats()
                .iter()
                .filter(|format| format.captured_fourcc == request.mode.captured_fourcc)
            {
                routes.push(ValidatedRoute {
                    node,
                    buffer_type: format.buffer_type,
                });
            }
        }
    }
    if !routes.is_empty() {
        Ok(ValidatedCapture {
            identity: &device.identity,
            mode: request.mode,
            routes,
        })
    } else if let Some(reason) = unavailable {
        Err(CaptureError::DescriptorUnavailable(reason))
    } else {
        Err(CaptureError::Unsupported(unsupported))
    }
}

fn unsupported_rank(reason: UnsupportedReason) -> u8 {
    match reason {
        UnsupportedReason::FourCc => 0,
        UnsupportedReason::FrameSize => 1,
        UnsupportedReason::FrameRate => 2,
    }
}

#[cfg(test)]
pub(crate) fn session_fixture(paths: &[&str], mode: CaptureMode) -> CaptureSnapshot {
    use crate::domain::capture::{
        DiscreteSize, FormatDescriptor, FourCcCapabilities, FrameIntervalKind, FrameSizeKind,
        FrameSizes, UsbTopology,
    };
    let identity = DeviceIdentity::new(
        0x32ed,
        0x3701,
        UsbTopology::new(
            "pci-fixture".into(),
            vec![std::num::NonZeroU8::new(1).unwrap()],
        )
        .unwrap(),
        Some("fixture".into()),
    )
    .unwrap();
    let nodes = paths
        .iter()
        .map(|path| CaptureNode {
            devnode: (*path).into(),
            syspath: format!("/sys{path}").into(),
            usb_syspath: "/sys/usb/fixture".into(),
            driver: "uvcvideo".into(),
            card_name: "fixture".into(),
            bus_info: "fixture".into(),
            raw_capabilities: 0,
            raw_device_caps: 0,
            effective_capabilities: 0,
            capabilities: NodeCapabilities::new(
                vec![FormatDescriptor {
                    buffer_type: CaptureBufferType::SinglePlanar,
                    captured_fourcc: mode.captured_fourcc,
                    description: "fixture".into(),
                    flags: 0,
                }],
                vec![FourCcCapabilities {
                    captured_fourcc: mode.captured_fourcc,
                    sizes: Descriptor::Available(
                        FrameSizes::new(FrameSizeKind::Discrete(vec![DiscreteSize {
                            size: mode.size,
                            intervals: Descriptor::Available(
                                FrameIntervals::new(FrameIntervalKind::Discrete(vec![
                                    mode.rate.interval(),
                                ]))
                                .unwrap(),
                            ),
                        }]))
                        .unwrap(),
                    ),
                }],
            )
            .unwrap(),
        })
        .collect();
    CaptureSnapshot {
        devices: vec![CaptureDevice { identity, nodes }],
    }
}

/// Range-descriptor fixture: same identity, but sizes are announced as a
/// Stepwise range containing the requested size, so proving the tuple needs an
/// exact interval query (injected via validate_with_injected_query in tests).
#[cfg(test)]
pub(crate) fn session_range_fixture(paths: &[&str], mode: CaptureMode) -> CaptureSnapshot {
    use crate::domain::capture::{
        FormatDescriptor, FourCcCapabilities, FrameSizeKind, FrameSizes, UsbTopology,
    };
    let identity = DeviceIdentity::new(
        0x32ed,
        0x3701,
        UsbTopology::new(
            "pci-fixture".into(),
            vec![std::num::NonZeroU8::new(1).unwrap()],
        )
        .unwrap(),
        Some("fixture".into()),
    )
    .unwrap();
    let nodes = paths
        .iter()
        .map(|path| CaptureNode {
            devnode: (*path).into(),
            syspath: format!("/sys{path}").into(),
            usb_syspath: "/sys/usb/fixture".into(),
            driver: "uvcvideo".into(),
            card_name: "fixture".into(),
            bus_info: "fixture".into(),
            raw_capabilities: 0,
            raw_device_caps: 0,
            effective_capabilities: 0,
            capabilities: NodeCapabilities::new(
                vec![FormatDescriptor {
                    buffer_type: CaptureBufferType::SinglePlanar,
                    captured_fourcc: mode.captured_fourcc,
                    description: "fixture".into(),
                    flags: 0,
                }],
                vec![FourCcCapabilities {
                    captured_fourcc: mode.captured_fourcc,
                    sizes: Descriptor::Available(
                        FrameSizes::new(FrameSizeKind::Stepwise {
                            min: FrameSize::new(640, 360).unwrap(),
                            max: FrameSize::new(3840, 2160).unwrap(),
                            step_width: std::num::NonZeroU32::new(2).unwrap(),
                            step_height: std::num::NonZeroU32::new(2).unwrap(),
                        })
                        .unwrap(),
                    ),
                }],
            )
            .unwrap(),
        })
        .collect();
    CaptureSnapshot {
        devices: vec![CaptureDevice { identity, nodes }],
    }
}

/// Deterministic exact-interval answer for range fixtures: announces exactly
/// the requested rate for the requested tuple, without any `/dev` access.
#[cfg(test)]
pub(crate) fn fixture_exact_intervals(
    fourcc: CapturedFourCc,
    size: FrameSize,
    rate: crate::domain::capture::FrameRate,
) -> ExactSizeIntervals {
    use crate::domain::capture::{FrameIntervalKind, FrameIntervals};
    ExactSizeIntervals {
        captured_fourcc: fourcc,
        size,
        intervals: crate::domain::capture::Descriptor::Available(
            FrameIntervals::new(FrameIntervalKind::Discrete(vec![rate.interval()])).unwrap(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use std::error::Error;

    use super::*;

    #[test]
    fn domain_failures_keep_their_typed_variants() {
        let data = CaptureError::from(CaptureDataError::InvalidDescriptor {
            reason: "test descriptor",
        });
        assert!(matches!(
            data,
            CaptureError::Data(CaptureDataError::InvalidDescriptor {
                reason: "test descriptor"
            })
        ));
        let identity = CaptureError::from(IdentityError::DuplicateSerial { candidates: 2 });
        assert!(matches!(
            identity,
            CaptureError::Identity(IdentityError::DuplicateSerial { candidates: 2 })
        ));
    }

    #[test]
    fn io_failures_expose_original_os_source() {
        let error = CaptureError::Io {
            path: PathBuf::from("/dev/video0"),
            operation: "enum_interval",
            source: io::Error::from_raw_os_error(5),
        };
        let source = error.source().unwrap().downcast_ref::<io::Error>().unwrap();
        assert_eq!(source.raw_os_error(), Some(5));
    }

    #[test]
    fn malformed_descriptors_keep_context_and_domain_source() {
        let error = CaptureError::MalformedDescriptor {
            path: PathBuf::from("/dev/video0"),
            operation: "enum_size",
            index: 2,
            source: CaptureDataError::InvalidDescriptor {
                reason: "returned FourCC changed",
            },
        };
        assert!(matches!(
            error.source().unwrap().downcast_ref::<CaptureDataError>(),
            Some(CaptureDataError::InvalidDescriptor {
                reason: "returned FourCC changed"
            })
        ));
        assert!(matches!(
            error,
            CaptureError::MalformedDescriptor {
                operation: "enum_size",
                index: 2,
                ..
            }
        ));
    }

    use crate::domain::capture::{
        CaptureBufferType, CaptureMode, CapturedFourCc, Descriptor, DeviceIdentity, DiscreteSize,
        FormatDescriptor, FourCcCapabilities, FrameInterval, FrameIntervalKind, FrameIntervals,
        FrameRate, FrameSize, FrameSizeKind, FrameSizes, ModeRequest, NodeCapabilities,
        UsbTopology,
    };
    use std::{cell::Cell, num::NonZeroU8};

    const NV12: CapturedFourCc = CapturedFourCc::from_bytes(*b"NV12");
    const YUYV: CapturedFourCc = CapturedFourCc::from_bytes(*b"YUYV");

    fn identity(serial: Option<&str>, port: u8) -> DeviceIdentity {
        DeviceIdentity::new(
            0x32ed,
            0x3701,
            UsbTopology::new(
                "pci-0000:16:00.0".into(),
                vec![NonZeroU8::new(port).unwrap()],
            )
            .unwrap(),
            serial.map(str::to_owned),
        )
        .unwrap()
    }

    fn size(width: u32, height: u32) -> FrameSize {
        FrameSize::new(width, height).unwrap()
    }

    fn intervals(rate: u32) -> Descriptor<FrameIntervals> {
        Descriptor::Available(
            FrameIntervals::new(FrameIntervalKind::Discrete(vec![
                FrameInterval::new(1, rate).unwrap(),
            ]))
            .unwrap(),
        )
    }

    fn discrete(
        width: u32,
        height: u32,
        rates: Descriptor<FrameIntervals>,
    ) -> Descriptor<FrameSizes> {
        Descriptor::Available(
            FrameSizes::new(FrameSizeKind::Discrete(vec![DiscreteSize {
                size: size(width, height),
                intervals: rates,
            }]))
            .unwrap(),
        )
    }

    fn range() -> Descriptor<FrameSizes> {
        Descriptor::Available(
            FrameSizes::new(FrameSizeKind::Continuous {
                min: size(640, 480),
                max: size(2560, 1440),
            })
            .unwrap(),
        )
    }

    fn node(
        path: &str,
        entries: Vec<(CaptureBufferType, CapturedFourCc)>,
        caps: Vec<(CapturedFourCc, Descriptor<FrameSizes>)>,
    ) -> CaptureNode {
        let c = ffi::bridge::v4l2_constants();
        CaptureNode {
            devnode: path.into(),
            syspath: format!("/sys{path}").into(),
            usb_syspath: "/sys/usb/fixture".into(),
            driver: "uvcvideo".into(),
            card_name: "fixture".into(),
            bus_info: "session".into(),
            raw_capabilities: c.cap_device_caps | c.cap_video_capture,
            raw_device_caps: c.cap_video_capture,
            effective_capabilities: c.cap_video_capture,
            capabilities: NodeCapabilities::new(
                entries
                    .into_iter()
                    .map(|(buffer_type, captured_fourcc)| FormatDescriptor {
                        buffer_type,
                        captured_fourcc,
                        description: "fixture".into(),
                        flags: 0,
                    })
                    .collect(),
                caps.into_iter()
                    .map(|(captured_fourcc, sizes)| FourCcCapabilities {
                        captured_fourcc,
                        sizes,
                    })
                    .collect(),
            )
            .unwrap(),
        }
    }

    fn snapshot(nodes: Vec<CaptureNode>) -> CaptureSnapshot {
        CaptureSnapshot {
            devices: vec![CaptureDevice {
                identity: identity(Some("fixture"), 1),
                nodes,
            }],
        }
    }

    fn request(fourcc: CapturedFourCc, rate: u32) -> ModeRequest {
        ModeRequest {
            identity: identity(Some("fixture"), 1),
            mode: CaptureMode {
                captured_fourcc: fourcc,
                size: size(2560, 1440),
                rate: FrameRate::new(rate, 1).unwrap(),
            },
        }
    }

    fn exact_from_pod(
        node: &CaptureNode,
        fourcc: CapturedFourCc,
        size: FrameSize,
        rate: Option<u32>,
    ) -> Result<ExactSizeIntervals, CaptureError> {
        let c = ffi::bridge::v4l2_constants();
        let intervals = v4l2::enumerate_intervals(node.devnode(), fourcc, size, &c, |index| {
            if let Some(rate) = rate.filter(|_| index == 0) {
                ffi::bridge::RawInterval {
                    index,
                    fourcc: fourcc.kernel_value(),
                    width: size.width(),
                    height: size.height(),
                    kind: c.interval_discrete,
                    numerator: 1,
                    denominator: rate,
                    ..Default::default()
                }
            } else {
                ffi::bridge::RawInterval {
                    error: c.einval,
                    ..Default::default()
                }
            }
        })?;
        Ok(ExactSizeIntervals {
            captured_fourcc: fourcc,
            size,
            intervals,
        })
    }

    #[test]
    fn validation_proves_nv12_but_never_borrows_yuyv_intervals() {
        let s = snapshot(vec![node(
            "/dev/video-fixture",
            vec![
                (CaptureBufferType::SinglePlanar, NV12),
                (CaptureBufferType::SinglePlanar, YUYV),
            ],
            vec![
                (NV12, discrete(2560, 1440, intervals(60))),
                (YUYV, discrete(2560, 1440, intervals(50))),
            ],
        )]);
        assert_eq!(validate(&s, &request(NV12, 60)).unwrap().routes().len(), 1);
        assert!(matches!(
            validate(&s, &request(YUYV, 60)),
            Err(CaptureError::Unsupported(UnsupportedReason::FrameRate))
        ));
    }

    #[test]
    fn validation_never_unions_formats_sizes_or_rates_between_nodes() {
        let s = snapshot(vec![
            node(
                "/dev/video-a",
                vec![(CaptureBufferType::SinglePlanar, NV12)],
                vec![(NV12, discrete(1280, 720, intervals(60)))],
            ),
            node(
                "/dev/video-b",
                vec![(CaptureBufferType::SinglePlanar, NV12)],
                vec![(NV12, discrete(2560, 1440, intervals(30)))],
            ),
            node(
                "/dev/video-c",
                vec![(CaptureBufferType::SinglePlanar, YUYV)],
                vec![(YUYV, discrete(2560, 1440, intervals(60)))],
            ),
        ]);
        assert!(matches!(
            validate(&s, &request(NV12, 60)),
            Err(CaptureError::Unsupported(UnsupportedReason::FrameRate))
        ));
    }

    #[test]
    fn validation_returns_all_actual_routes_in_snapshot_order() {
        let s = snapshot(vec![
            node(
                "/dev/video-a",
                vec![
                    (CaptureBufferType::SinglePlanar, NV12),
                    (CaptureBufferType::MultiPlanar, NV12),
                ],
                vec![(NV12, discrete(2560, 1440, intervals(60)))],
            ),
            node(
                "/dev/video-b",
                vec![
                    (CaptureBufferType::MultiPlanar, NV12),
                    (CaptureBufferType::SinglePlanar, YUYV),
                ],
                vec![
                    (NV12, discrete(2560, 1440, intervals(60))),
                    (YUYV, discrete(2560, 1440, intervals(60))),
                ],
            ),
        ]);
        let validated = validate(&s, &request(NV12, 60)).unwrap();
        assert_eq!(validated.routes().len(), 3);
        assert!(std::ptr::eq(
            validated.routes()[0].node(),
            &s.devices()[0].nodes()[0]
        ));
        assert_eq!(
            validated.routes()[0].buffer_type(),
            CaptureBufferType::SinglePlanar
        );
        assert_eq!(
            validated.routes()[1].buffer_type(),
            CaptureBufferType::MultiPlanar
        );
        assert!(std::ptr::eq(
            validated.routes()[2].node(),
            &s.devices()[0].nodes()[1]
        ));
        assert!(std::ptr::eq(
            validated.identity(),
            s.devices()[0].identity()
        ));
        assert_eq!(validated.mode(), request(NV12, 60).mode);
    }

    #[test]
    fn exact_interior_intervals_reject_sixty_even_when_minimum_accepts_sixty() {
        let s = snapshot(vec![node(
            "/dev/video-range",
            vec![
                (CaptureBufferType::SinglePlanar, NV12),
                (CaptureBufferType::MultiPlanar, NV12),
            ],
            vec![(NV12, range())],
        )]);
        let minimum =
            exact_from_pod(&s.devices[0].nodes[0], NV12, size(640, 480), Some(60)).unwrap();
        assert!(
            minimum
                .intervals
                .as_option()
                .unwrap()
                .supports(FrameRate::new(60, 1).unwrap())
        );
        let mut requested = request(NV12, 60);
        requested.mode.size = size(1280, 720);
        let result = validate_with_query(&s, &requested, |_, n, f, exact_size| {
            assert_eq!(exact_size, size(1280, 720));
            exact_from_pod(n, f, exact_size, Some(30))
        });
        assert!(matches!(
            result,
            Err(CaptureError::Unsupported(UnsupportedReason::FrameRate))
        ));
        let result = validate_with_query(&s, &requested, |_, n, f, exact_size| {
            exact_from_pod(n, f, exact_size, None)
        });
        assert!(matches!(
            result,
            Err(CaptureError::DescriptorUnavailable(
                UnknownReason::FrameIntervalsNotReported
            ))
        ));
    }

    #[test]
    fn exact_query_shared_between_actual_buffers_without_extrapolation() {
        let s = snapshot(vec![node(
            "/dev/video-range",
            vec![
                (CaptureBufferType::SinglePlanar, NV12),
                (CaptureBufferType::MultiPlanar, NV12),
            ],
            vec![(NV12, range())],
        )]);
        let queried = Cell::new(false);
        let result = validate_with_query(&s, &request(NV12, 60), |_, n, f, z| {
            assert!(
                !queried.replace(true),
                "one FourCC interval descriptor per node"
            );
            exact_from_pod(n, f, z, Some(60))
        })
        .unwrap();
        assert_eq!(result.routes().len(), 2);
    }

    #[test]
    fn unavailable_loses_to_proven_route_but_beats_unsupported() {
        let s = snapshot(vec![
            node(
                "/dev/video-a",
                vec![(CaptureBufferType::SinglePlanar, NV12)],
                vec![(NV12, discrete(2560, 1440, intervals(30)))],
            ),
            node(
                "/dev/video-b",
                vec![(CaptureBufferType::SinglePlanar, NV12)],
                vec![(NV12, Descriptor::NotReported)],
            ),
        ]);
        assert!(matches!(
            validate(&s, &request(NV12, 60)),
            Err(CaptureError::DescriptorUnavailable(
                UnknownReason::FrameSizesNotReported
            ))
        ));
        let mut s = s;
        s.devices[0].nodes.push(node(
            "/dev/video-c",
            vec![(CaptureBufferType::SinglePlanar, NV12)],
            vec![(NV12, discrete(2560, 1440, intervals(60)))],
        ));
        assert_eq!(validate(&s, &request(NV12, 60)).unwrap().routes().len(), 1);
    }

    #[test]
    fn required_io_and_malformed_query_errors_survive_other_proven_routes() {
        let s = snapshot(vec![
            node(
                "/dev/video-a",
                vec![(CaptureBufferType::SinglePlanar, NV12)],
                vec![(NV12, discrete(2560, 1440, intervals(60)))],
            ),
            node(
                "/dev/video-b",
                vec![(CaptureBufferType::SinglePlanar, NV12)],
                vec![(NV12, range())],
            ),
        ]);
        let result = validate_with_query(&s, &request(NV12, 60), |_, n, _, _| {
            Err(udev::access_error(
                n.devnode(),
                "enum_interval",
                io::Error::from_raw_os_error(5),
            ))
        });
        assert!(matches!(
            result,
            Err(CaptureError::Io {
                operation: "enum_interval",
                ..
            })
        ));
        let result = validate_with_query(&s, &request(NV12, 60), |_, n, f, z| {
            let c = ffi::bridge::v4l2_constants();
            let descriptors =
                v4l2::enumerate_intervals(n.devnode(), f, z, &c, |_| ffi::bridge::RawInterval {
                    index: 1,
                    ..Default::default()
                })?;
            Ok(ExactSizeIntervals {
                captured_fourcc: f,
                size: z,
                intervals: descriptors,
            })
        });
        assert!(matches!(
            result,
            Err(CaptureError::MalformedDescriptor {
                operation: "enum_interval",
                ..
            })
        ));
    }

    #[test]
    fn validation_resolves_identity_before_visiting_routes() {
        let mut s = snapshot(vec![node(
            "/dev/video-fixture",
            vec![(CaptureBufferType::SinglePlanar, NV12)],
            vec![(NV12, range())],
        )]);
        let mut requested = request(NV12, 60);
        requested.identity = identity(Some("missing"), 1);
        assert!(matches!(
            validate_with_query(&s, &requested, |_, _, _, _| panic!(
                "unresolved identity queried"
            )),
            Err(CaptureError::Identity(IdentityError::NotFound))
        ));
        s.devices.push(CaptureDevice {
            identity: identity(Some("fixture"), 2),
            nodes: Vec::new(),
        });
        assert!(matches!(
            validate(&s, &request(NV12, 60)),
            Err(CaptureError::Identity(IdentityError::DuplicateSerial {
                candidates: 2
            }))
        ));
    }

    #[test]
    fn deferred_query_rejects_foreign_equal_node_and_unsupported_target_before_io() {
        let s = snapshot(vec![node(
            "/dev/video-fixture",
            vec![(CaptureBufferType::SinglePlanar, NV12)],
            vec![(NV12, range())],
        )]);
        let d = &s.devices[0];
        let foreign = node(
            "/dev/video-fixture",
            vec![(CaptureBufferType::SinglePlanar, NV12)],
            vec![(NV12, range())],
        );
        assert!(matches!(
            query_intervals(d, &foreign, NV12, size(2560, 1440)),
            Err(CaptureError::InvalidNode { .. })
        ));
        assert!(matches!(
            query_intervals(d, &d.nodes[0], YUYV, size(2560, 1440)),
            Err(CaptureError::Unsupported(UnsupportedReason::FourCc))
        ));
        assert!(matches!(
            query_intervals(d, &d.nodes[0], NV12, size(3000, 2000)),
            Err(CaptureError::Unsupported(UnsupportedReason::FrameSize))
        ));
        let s = snapshot(vec![node(
            "/dev/video-missing",
            vec![(CaptureBufferType::SinglePlanar, NV12)],
            vec![(NV12, Descriptor::NotReported)],
        )]);
        assert!(matches!(
            query_intervals(
                &s.devices[0],
                &s.devices[0].nodes[0],
                NV12,
                size(2560, 1440)
            ),
            Err(CaptureError::DescriptorUnavailable(
                UnknownReason::FrameSizesNotReported
            ))
        ));
    }

    #[test]
    fn deferred_query_requires_same_raw_and_effective_capabilities() {
        let s = snapshot(vec![node(
            "/dev/video-fixture",
            vec![(CaptureBufferType::SinglePlanar, NV12)],
            vec![(NV12, range())],
        )]);
        let n = &s.devices[0].nodes[0];
        let c = ffi::bridge::v4l2_constants();
        let raw = ffi::bridge::RawCapabilities {
            capabilities: n.raw_capabilities,
            device_caps: n.raw_device_caps,
            ..Default::default()
        };
        assert!(check_fresh_capabilities(n, &raw, &c).is_ok());
        for changed in [
            ffi::bridge::RawCapabilities {
                capabilities: raw.capabilities ^ 0x10,
                ..raw
            },
            ffi::bridge::RawCapabilities {
                device_caps: raw.device_caps ^ 0x10,
                ..raw
            },
        ] {
            assert!(matches!(
                check_fresh_capabilities(n, &changed, &c),
                Err(CaptureError::StaleSnapshot { .. })
            ));
        }
    }

    #[test]
    fn snapshot_distinguishes_no_video_from_no_eligible_capture_and_keeps_errors() {
        let empty = udev::Observations {
            raw_video_nodes: 0,
            devices: Vec::new(),
        };
        assert!(matches!(
            snapshot_from_observations(empty, |_, _| panic!("no node")),
            Err(CaptureError::NoVideoNodes)
        ));
        let observations = || udev::Observations {
            raw_video_nodes: 2,
            devices: vec![udev::ObservedDevice {
                identity: identity(Some("fixture"), 1),
                usb_syspath: "/sys/usb".into(),
                nodes: vec![udev::NodeObservation {
                    devnode: "/dev/video-fixture".into(),
                    syspath: "/sys/video-fixture".into(),
                }],
            }],
        };
        assert!(matches!(
            snapshot_from_observations(observations(), |_, _| Ok(None)),
            Err(CaptureError::NoCaptureNodes)
        ));
        assert!(matches!(
            snapshot_from_observations(observations(), |_, n| Err(udev::access_error(
                &n.devnode,
                "query_cap",
                io::Error::from_raw_os_error(13)
            ))),
            Err(CaptureError::PermissionDenied { .. })
        ));
    }

    #[test]
    fn snapshot_filters_metadata_and_keeps_physical_identity_once() {
        let c = ffi::bridge::v4l2_constants();
        let observations = udev::Observations {
            raw_video_nodes: 3,
            devices: vec![udev::ObservedDevice {
                identity: identity(Some("fixture"), 1),
                usb_syspath: "/sys/usb/fixture".into(),
                nodes: ["capture-a", "metadata", "capture-b"]
                    .into_iter()
                    .map(|name| udev::NodeObservation {
                        devnode: format!("/dev/{name}").into(),
                        syspath: format!("/sys/{name}").into(),
                    })
                    .collect(),
            }],
        };
        let snapshot = snapshot_from_observations(observations, |_, observed| {
            let raw = ffi::bridge::RawCapabilities {
                capabilities: c.cap_device_caps | c.cap_video_capture,
                device_caps: if observed.devnode == Path::new("/dev/metadata") {
                    0
                } else {
                    c.cap_video_capture
                },
                ..Default::default()
            };
            if ffi::capture_buffer_types(ffi::effective_capabilities(&raw, &c), &c)
                .next()
                .is_none()
            {
                return Ok(None);
            }
            Ok(Some(node(
                observed.devnode.to_str().unwrap(),
                vec![(CaptureBufferType::SinglePlanar, NV12)],
                vec![(NV12, discrete(2560, 1440, intervals(60)))],
            )))
        })
        .unwrap();
        assert_eq!(snapshot.devices().len(), 1);
        assert_eq!(
            snapshot.devices()[0].identity(),
            &identity(Some("fixture"), 1)
        );
        assert_eq!(snapshot.devices()[0].nodes().len(), 2);
        assert_eq!(
            snapshot.devices()[0].nodes()[0].devnode(),
            Path::new("/dev/capture-a")
        );
        assert_eq!(
            snapshot.devices()[0].nodes()[1].devnode(),
            Path::new("/dev/capture-b")
        );
        let json = serde_json::to_value(&snapshot).unwrap();
        assert!(json["devices"][0]["identity"].get("devnode").is_none());
        assert_eq!(json["devices"][0]["nodes"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn snapshot_failure_after_valid_node_never_returns_partial_observations() {
        let observations = udev::Observations {
            raw_video_nodes: 2,
            devices: vec![udev::ObservedDevice {
                identity: identity(Some("fixture"), 1),
                usb_syspath: "/sys/usb/fixture".into(),
                nodes: ["capture", "denied"]
                    .into_iter()
                    .map(|name| udev::NodeObservation {
                        devnode: format!("/dev/{name}").into(),
                        syspath: format!("/sys/{name}").into(),
                    })
                    .collect(),
            }],
        };
        let result = snapshot_from_observations(observations, |_, observed| {
            if observed.devnode == Path::new("/dev/denied") {
                Err(udev::access_error(
                    &observed.devnode,
                    "open",
                    io::Error::from_raw_os_error(13),
                ))
            } else {
                Ok(Some(node(
                    "/dev/capture",
                    vec![(CaptureBufferType::SinglePlanar, NV12)],
                    vec![(NV12, discrete(2560, 1440, intervals(60)))],
                )))
            }
        });
        assert!(matches!(
            result,
            Err(CaptureError::PermissionDenied {
                operation: "open",
                ..
            })
        ));
    }

    #[test]
    fn precise_unavailable_and_unsupported_reasons_follow_reached_relation() {
        let s = snapshot(vec![
            node(
                "/dev/video-sizes",
                vec![(CaptureBufferType::SinglePlanar, NV12)],
                vec![(NV12, Descriptor::NotReported)],
            ),
            node(
                "/dev/video-intervals",
                vec![(CaptureBufferType::SinglePlanar, NV12)],
                vec![(NV12, discrete(2560, 1440, Descriptor::NotReported))],
            ),
        ]);
        assert!(matches!(
            validate(&s, &request(NV12, 60)),
            Err(CaptureError::DescriptorUnavailable(
                UnknownReason::FrameIntervalsNotReported
            ))
        ));
        assert!(matches!(
            validate(&s, &request(YUYV, 60)),
            Err(CaptureError::Unsupported(UnsupportedReason::FourCc))
        ));
        let s = snapshot(vec![node(
            "/dev/video-size",
            vec![(CaptureBufferType::SinglePlanar, NV12)],
            vec![(NV12, discrete(1280, 720, intervals(60)))],
        )]);
        assert!(matches!(
            validate(&s, &request(NV12, 60)),
            Err(CaptureError::Unsupported(UnsupportedReason::FrameSize))
        ));
    }

    #[test]
    fn deferred_query_rejects_off_lattice_size_without_opening() {
        let sizes = FrameSizes::new(FrameSizeKind::Stepwise {
            min: size(640, 480),
            max: size(2561, 1441),
            step_width: std::num::NonZeroU32::new(16).unwrap(),
            step_height: std::num::NonZeroU32::new(16).unwrap(),
        })
        .unwrap();
        let s = snapshot(vec![node(
            "/dev/video-fixture",
            vec![(CaptureBufferType::SinglePlanar, NV12)],
            vec![(NV12, Descriptor::Available(sizes))],
        )]);
        let d = &s.devices[0];
        assert!(matches!(
            query_intervals(d, &d.nodes[0], NV12, size(2561, 1441)),
            Err(CaptureError::Unsupported(UnsupportedReason::FrameSize))
        ));
        assert!(check_query_target(d, &d.nodes[0], NV12, size(2560, 1440)).is_ok());
    }

    #[test]
    fn fresh_querycap_os_error_ignores_payload_and_keeps_errno() {
        let s = snapshot(vec![node(
            "/dev/video-fixture",
            vec![(CaptureBufferType::SinglePlanar, NV12)],
            vec![(NV12, range())],
        )]);
        let c = ffi::bridge::v4l2_constants();
        let raw = ffi::bridge::RawCapabilities {
            error: 19,
            ..Default::default()
        };
        let result = check_fresh_capabilities(&s.devices[0].nodes[0], &raw, &c);
        assert!(
            matches!(result, Err(CaptureError::DeviceGone { operation: "query_cap", source, .. }) if source.raw_os_error() == Some(19))
        );
    }
}
