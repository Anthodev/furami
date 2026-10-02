//! V4L2 POD parsing and exact descriptor enumeration. Every callback below is
//! private to this adapter; production and synthetic POD tests use the same loops.

use std::{io, num::NonZeroU32, path::Path};

use crate::domain::capture::{
    CaptureBufferType, CaptureDataError, CapturedFourCc, Descriptor, DiscreteSize,
    FormatDescriptor, FourCcCapabilities, FrameInterval, FrameIntervalKind, FrameIntervals,
    FrameSize, FrameSizeKind, FrameSizes, NodeCapabilities,
};

use super::{
    CaptureError,
    ffi::{self, bridge},
    udev,
};

pub(super) struct Capabilities {
    pub driver: String,
    pub card_name: String,
    pub bus_info: String,
    pub raw_capabilities: u32,
    pub raw_device_caps: u32,
    pub effective_capabilities: u32,
}

pub(super) fn query_cap(
    fd: i32,
    path: &Path,
    constants: &bridge::V4l2Constants,
) -> Result<Capabilities, CaptureError> {
    parse_capabilities(bridge::query_cap(fd), path, constants)
}

fn parse_capabilities(
    raw: bridge::RawCapabilities,
    path: &Path,
    constants: &bridge::V4l2Constants,
) -> Result<Capabilities, CaptureError> {
    check_error(raw.error, path, "query_cap")?;
    Ok(Capabilities {
        driver: kernel_text(&raw.driver, path, "query_cap", 0)?,
        card_name: kernel_text(&raw.card, path, "query_cap", 0)?,
        bus_info: kernel_text(&raw.bus_info, path, "query_cap", 0)?,
        raw_capabilities: raw.capabilities,
        raw_device_caps: raw.device_caps,
        effective_capabilities: ffi::effective_capabilities(&raw, constants),
    })
}

pub(super) fn check_error(
    error: i32,
    path: &Path,
    operation: &'static str,
) -> Result<(), CaptureError> {
    if error == 0 {
        Ok(())
    } else {
        Err(udev::access_error(
            path,
            operation,
            io::Error::from_raw_os_error(error),
        ))
    }
}

fn malformed(
    path: &Path,
    operation: &'static str,
    index: u32,
    source: CaptureDataError,
) -> CaptureError {
    CaptureError::MalformedDescriptor {
        path: path.to_owned(),
        operation,
        index,
        source,
    }
}

fn invalid(path: &Path, operation: &'static str, index: u32, reason: &'static str) -> CaptureError {
    malformed(
        path,
        operation,
        index,
        CaptureDataError::InvalidDescriptor { reason },
    )
}

fn kernel_text(
    bytes: &[u8],
    path: &Path,
    operation: &'static str,
    index: u32,
) -> Result<String, CaptureError> {
    let end = bytes
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(bytes.len());
    std::str::from_utf8(&bytes[..end])
        .map(str::to_owned)
        .map_err(|_| invalid(path, operation, index, "kernel text is not UTF-8"))
}

enum EnumerationStatus {
    Entry,
    End,
    NotReported,
}

fn enumeration_status(
    error: i32,
    index: u32,
    optional: bool,
    path: &Path,
    operation: &'static str,
    constants: &bridge::V4l2Constants,
) -> Result<EnumerationStatus, CaptureError> {
    if error == 0 {
        Ok(EnumerationStatus::Entry)
    } else if optional && index == 0 && (error == constants.einval || error == constants.enotty) {
        Ok(EnumerationStatus::NotReported)
    } else if error == constants.einval {
        Ok(EnumerationStatus::End)
    } else {
        Err(udev::access_error(
            path,
            operation,
            io::Error::from_raw_os_error(error),
        ))
    }
}

fn next_index(path: &Path, operation: &'static str, index: u32) -> Result<u32, CaptureError> {
    index
        .checked_add(1)
        .ok_or_else(|| invalid(path, operation, index, "enumeration index exhausted"))
}

fn parse_format(
    raw: bridge::RawFormat,
    path: &Path,
    index: u32,
    buffer_type: CaptureBufferType,
    kernel_type: u32,
) -> Result<FormatDescriptor, CaptureError> {
    if raw.index != index || raw.buffer_type != kernel_type {
        return Err(invalid(
            path,
            "enum_format",
            index,
            "returned index or buffer type changed",
        ));
    }
    Ok(FormatDescriptor {
        buffer_type,
        captured_fourcc: CapturedFourCc::from_kernel(raw.fourcc),
        description: kernel_text(&raw.description, path, "enum_format", index)?,
        flags: raw.flags,
    })
}

fn enumerate_formats(
    path: &Path,
    effective: u32,
    constants: &bridge::V4l2Constants,
    mut query: impl FnMut(u32, u32) -> bridge::RawFormat,
) -> Result<Vec<FormatDescriptor>, CaptureError> {
    let mut formats: Vec<FormatDescriptor> = Vec::new();
    for (buffer_type, kernel_type) in ffi::capture_buffer_types(effective, constants) {
        let mut index = 0;
        loop {
            let raw = query(kernel_type, index);
            match enumeration_status(raw.error, index, false, path, "enum_format", constants)? {
                EnumerationStatus::End | EnumerationStatus::NotReported => break,
                EnumerationStatus::Entry => {}
            }
            let format = parse_format(raw, path, index, buffer_type, kernel_type)?;
            if formats.iter().any(|other| {
                other.buffer_type == buffer_type && other.captured_fourcc == format.captured_fourcc
            }) {
                return Err(invalid(
                    path,
                    "enum_format",
                    index,
                    "duplicate format entry",
                ));
            }
            formats.push(format);
            index = next_index(path, "enum_format", index)?;
        }
    }
    Ok(formats)
}

enum SizeEntry {
    Discrete(FrameSize),
    Range(FrameSizes),
}

enum EnumeratedSizes {
    Discrete(Vec<FrameSize>),
    Range(FrameSizes),
}

fn parse_size(
    raw: bridge::RawSize,
    path: &Path,
    fourcc: CapturedFourCc,
    index: u32,
    constants: &bridge::V4l2Constants,
) -> Result<SizeEntry, CaptureError> {
    if raw.index != index || raw.fourcc != fourcc.kernel_value() {
        return Err(invalid(
            path,
            "enum_size",
            index,
            "returned index or FourCC changed",
        ));
    }
    let error = |source| malformed(path, "enum_size", index, source);
    if raw.kind == constants.size_discrete {
        return Ok(SizeEntry::Discrete(
            FrameSize::new(raw.width, raw.height).map_err(error)?,
        ));
    }
    if raw.kind != constants.size_stepwise && raw.kind != constants.size_continuous {
        return Err(invalid(path, "enum_size", index, "unknown size tag"));
    }
    let min = FrameSize::new(raw.min_width, raw.min_height).map_err(error)?;
    let max = FrameSize::new(raw.max_width, raw.max_height).map_err(error)?;
    let kind = if raw.kind == constants.size_continuous {
        FrameSizeKind::Continuous { min, max }
    } else {
        let step_width = NonZeroU32::new(raw.step_width)
            .ok_or_else(|| invalid(path, "enum_size", index, "zero width step"))?;
        let step_height = NonZeroU32::new(raw.step_height)
            .ok_or_else(|| invalid(path, "enum_size", index, "zero height step"))?;
        FrameSizeKind::Stepwise {
            min,
            max,
            step_width,
            step_height,
        }
    };
    Ok(SizeEntry::Range(FrameSizes::new(kind).map_err(error)?))
}

fn enumerate_sizes(
    path: &Path,
    fourcc: CapturedFourCc,
    constants: &bridge::V4l2Constants,
    mut query: impl FnMut(u32) -> bridge::RawSize,
) -> Result<Descriptor<EnumeratedSizes>, CaptureError> {
    let mut discrete = Vec::new();
    let mut index = 0;
    loop {
        let raw = query(index);
        match enumeration_status(raw.error, index, true, path, "enum_size", constants)? {
            EnumerationStatus::NotReported => return Ok(Descriptor::NotReported),
            EnumerationStatus::End => {
                return Ok(Descriptor::Available(EnumeratedSizes::Discrete(discrete)));
            }
            EnumerationStatus::Entry => {}
        }
        match parse_size(raw, path, fourcc, index, constants)? {
            SizeEntry::Range(range) if index == 0 => {
                return Ok(Descriptor::Available(EnumeratedSizes::Range(range)));
            }
            SizeEntry::Range(_) => {
                return Err(invalid(path, "enum_size", index, "mixed size tags"));
            }
            SizeEntry::Discrete(size) => {
                if discrete.contains(&size) {
                    return Err(invalid(path, "enum_size", index, "duplicate discrete size"));
                }
                discrete.push(size);
            }
        }
        index = next_index(path, "enum_size", index)?;
    }
}

enum IntervalEntry {
    Discrete(FrameInterval),
    Range(FrameIntervals),
}

fn parse_interval(
    raw: bridge::RawInterval,
    path: &Path,
    fourcc: CapturedFourCc,
    size: FrameSize,
    index: u32,
    constants: &bridge::V4l2Constants,
) -> Result<IntervalEntry, CaptureError> {
    if raw.index != index
        || raw.fourcc != fourcc.kernel_value()
        || raw.width != size.width()
        || raw.height != size.height()
    {
        return Err(invalid(
            path,
            "enum_interval",
            index,
            "returned index, FourCC or size changed",
        ));
    }
    let error = |source| malformed(path, "enum_interval", index, source);
    if raw.kind == constants.interval_discrete {
        return Ok(IntervalEntry::Discrete(
            FrameInterval::new(raw.numerator, raw.denominator).map_err(error)?,
        ));
    }
    if raw.kind != constants.interval_stepwise && raw.kind != constants.interval_continuous {
        return Err(invalid(
            path,
            "enum_interval",
            index,
            "unknown interval tag",
        ));
    }
    let min = FrameInterval::new(raw.min_numerator, raw.min_denominator).map_err(error)?;
    let max = FrameInterval::new(raw.max_numerator, raw.max_denominator).map_err(error)?;
    let kind = if raw.kind == constants.interval_continuous {
        FrameIntervalKind::Continuous { min, max }
    } else {
        let step = FrameInterval::new(raw.step_numerator, raw.step_denominator).map_err(error)?;
        FrameIntervalKind::Stepwise { min, max, step }
    };
    Ok(IntervalEntry::Range(
        FrameIntervals::new(kind).map_err(error)?,
    ))
}

pub(super) fn enumerate_intervals(
    path: &Path,
    fourcc: CapturedFourCc,
    size: FrameSize,
    constants: &bridge::V4l2Constants,
    mut query: impl FnMut(u32) -> bridge::RawInterval,
) -> Result<Descriptor<FrameIntervals>, CaptureError> {
    let mut discrete = Vec::new();
    let mut index = 0;
    loop {
        let raw = query(index);
        match enumeration_status(raw.error, index, true, path, "enum_interval", constants)? {
            EnumerationStatus::NotReported => return Ok(Descriptor::NotReported),
            EnumerationStatus::End => {
                let intervals = FrameIntervals::new(FrameIntervalKind::Discrete(discrete))
                    .map_err(|source| malformed(path, "enum_interval", index, source))?;
                return Ok(Descriptor::Available(intervals));
            }
            EnumerationStatus::Entry => {}
        }
        match parse_interval(raw, path, fourcc, size, index, constants)? {
            IntervalEntry::Range(range) if index == 0 => return Ok(Descriptor::Available(range)),
            IntervalEntry::Range(_) => {
                return Err(invalid(path, "enum_interval", index, "mixed interval tags"));
            }
            IntervalEntry::Discrete(interval) => {
                if discrete.contains(&interval) {
                    return Err(invalid(
                        path,
                        "enum_interval",
                        index,
                        "duplicate discrete interval",
                    ));
                }
                discrete.push(interval);
            }
        }
        index = next_index(path, "enum_interval", index)?;
    }
}

pub(super) fn intervals(
    fd: i32,
    path: &Path,
    fourcc: CapturedFourCc,
    size: FrameSize,
    constants: &bridge::V4l2Constants,
) -> Result<Descriptor<FrameIntervals>, CaptureError> {
    enumerate_intervals(path, fourcc, size, constants, |index| {
        bridge::enum_interval(
            fd,
            fourcc.kernel_value(),
            size.width(),
            size.height(),
            index,
        )
    })
}

pub(super) fn node_capabilities(
    fd: i32,
    path: &Path,
    effective: u32,
    constants: &bridge::V4l2Constants,
) -> Result<NodeCapabilities, CaptureError> {
    node_capabilities_with(
        path,
        effective,
        constants,
        |buffer_type, index| bridge::enum_format(fd, buffer_type, index),
        |fourcc, index| bridge::enum_size(fd, fourcc.kernel_value(), index),
        |fourcc, size, index| {
            bridge::enum_interval(
                fd,
                fourcc.kernel_value(),
                size.width(),
                size.height(),
                index,
            )
        },
    )
}

fn node_capabilities_with(
    path: &Path,
    effective: u32,
    constants: &bridge::V4l2Constants,
    formats: impl FnMut(u32, u32) -> bridge::RawFormat,
    mut sizes: impl FnMut(CapturedFourCc, u32) -> bridge::RawSize,
    mut intervals: impl FnMut(CapturedFourCc, FrameSize, u32) -> bridge::RawInterval,
) -> Result<NodeCapabilities, CaptureError> {
    let formats = enumerate_formats(path, effective, constants, formats)?;
    let mut native_sizes: Vec<(CapturedFourCc, Descriptor<EnumeratedSizes>)> = Vec::new();
    for format in &formats {
        if !native_sizes
            .iter()
            .any(|(fourcc, _)| *fourcc == format.captured_fourcc)
        {
            let fourcc = format.captured_fourcc;
            native_sizes.push((
                fourcc,
                enumerate_sizes(path, fourcc, constants, |index| sizes(fourcc, index))?,
            ));
        }
    }
    // Finish every size sequence before any interval query. Preserve first-seen
    // FourCC order; do not borrow a descriptor from another node or size.
    let mut caps = Vec::with_capacity(native_sizes.len());
    for (fourcc, native) in native_sizes {
        let sizes = match native {
            Descriptor::NotReported => Descriptor::NotReported,
            Descriptor::Available(EnumeratedSizes::Range(range)) => Descriptor::Available(range),
            Descriptor::Available(EnumeratedSizes::Discrete(discrete)) => {
                let mut entries = Vec::with_capacity(discrete.len());
                for size in discrete {
                    let intervals = enumerate_intervals(path, fourcc, size, constants, |index| {
                        intervals(fourcc, size, index)
                    })?;
                    entries.push(DiscreteSize { size, intervals });
                }
                Descriptor::Available(
                    FrameSizes::new(FrameSizeKind::Discrete(entries))
                        .map_err(|source| malformed(path, "enum_size", 0, source))?,
                )
            }
        };
        caps.push(FourCcCapabilities {
            captured_fourcc: fourcc,
            sizes,
        });
    }
    NodeCapabilities::new(formats, caps).map_err(|source| malformed(path, "enum_format", 0, source))
}

#[cfg(test)]
mod tests {
    use crate::domain::capture::{CaptureMode, FrameRate, SupportVerdict};
    use std::cell::Cell;

    use super::*;

    const NV12: CapturedFourCc = CapturedFourCc::from_bytes(*b"NV12");
    const YUYV: CapturedFourCc = CapturedFourCc::from_bytes(*b"YUYV");

    fn constants() -> bridge::V4l2Constants {
        bridge::v4l2_constants()
    }

    fn path() -> &'static Path {
        Path::new("/dev/video-fixture")
    }

    fn size() -> FrameSize {
        FrameSize::new(2560, 1440).unwrap()
    }

    fn interval(index: u32, rate: u32) -> bridge::RawInterval {
        bridge::RawInterval {
            index,
            fourcc: NV12.kernel_value(),
            width: 2560,
            height: 1440,
            kind: constants().interval_discrete,
            numerator: 1,
            denominator: rate,
            ..Default::default()
        }
    }

    fn discrete_size(index: u32) -> bridge::RawSize {
        bridge::RawSize {
            index,
            fourcc: NV12.kernel_value(),
            kind: constants().size_discrete,
            width: 2560,
            height: 1440,
            ..Default::default()
        }
    }

    fn format(index: u32, buffer_type: u32, fourcc: CapturedFourCc) -> bridge::RawFormat {
        bridge::RawFormat {
            index,
            buffer_type,
            fourcc: fourcc.kernel_value(),
            flags: 0xfeed,
            description: [b'x'; 32],
            ..Default::default()
        }
    }

    fn malformed<T>(result: Result<T, CaptureError>, operation: &'static str, index: u32) {
        assert!(matches!(result, Err(CaptureError::MalformedDescriptor {
            operation: actual, index: returned, ..
        }) if actual == operation && returned == index));
    }

    #[test]
    fn enumeration_distinguishes_end_absence_and_os_failure() {
        let c = constants();
        assert!(matches!(
            enumeration_status(0, 0, true, path(), "enum_size", &c),
            Ok(EnumerationStatus::Entry)
        ));
        assert!(matches!(
            enumeration_status(c.einval, 0, true, path(), "enum_size", &c),
            Ok(EnumerationStatus::NotReported)
        ));
        assert!(matches!(
            enumeration_status(c.enotty, 0, true, path(), "enum_size", &c),
            Ok(EnumerationStatus::NotReported)
        ));
        assert!(matches!(
            enumeration_status(c.einval, 1, true, path(), "enum_size", &c),
            Ok(EnumerationStatus::End)
        ));
        assert!(matches!(
            enumeration_status(c.einval, 0, false, path(), "enum_format", &c),
            Ok(EnumerationStatus::End)
        ));
        for index in [0, 1] {
            assert!(
                matches!(enumeration_status(5, index, true, path(), "enum_interval", &c), Err(CaptureError::Io { source, .. }) if source.raw_os_error() == Some(5))
            );
            assert!(
                matches!(enumeration_status(13, index, true, path(), "enum_interval", &c), Err(CaptureError::PermissionDenied { source, .. }) if source.raw_os_error() == Some(13))
            );
        }
        assert!(
            matches!(enumeration_status(c.enotty, 1, true, path(), "enum_interval", &c), Err(CaptureError::Io { source, .. }) if source.raw_os_error() == Some(c.enotty))
        );
    }

    #[test]
    fn formats_keep_kernel_order_full_flags_and_actual_buffer_routes() {
        let c = constants();
        let formats = enumerate_formats(
            path(),
            c.cap_video_capture | c.cap_video_capture_mplane,
            &c,
            |buffer, index| match (buffer, index) {
                (b, 0) if b == c.buffer_capture => format(index, buffer, YUYV),
                (b, 1) if b == c.buffer_capture => format(index, buffer, NV12),
                (b, 0) if b == c.buffer_capture_mplane => format(index, buffer, NV12),
                _ => bridge::RawFormat {
                    error: c.einval,
                    ..Default::default()
                },
            },
        )
        .unwrap();
        assert_eq!(formats.len(), 3);
        assert_eq!(formats[0].captured_fourcc, YUYV);
        assert_eq!(formats[1].captured_fourcc, NV12);
        assert_eq!(formats[2].buffer_type, CaptureBufferType::MultiPlanar);
        assert_eq!(formats[2].flags, 0xfeed);
        assert_eq!(formats[2].description, "x".repeat(32));
    }

    #[test]
    fn initial_format_einval_yields_no_invented_format() {
        let c = constants();
        assert!(
            enumerate_formats(path(), c.cap_video_capture, &c, |_, _| bridge::RawFormat {
                error: c.einval,
                ..Default::default()
            })
            .unwrap()
            .is_empty()
        );
    }

    #[test]
    fn format_failures_after_success_do_not_truncate_results() {
        let c = constants();
        let result = enumerate_formats(path(), c.cap_video_capture, &c, |buffer, index| {
            if index == 0 {
                format(index, buffer, NV12)
            } else {
                bridge::RawFormat {
                    error: 5,
                    ..Default::default()
                }
            }
        });
        assert!(matches!(
            result,
            Err(CaptureError::Io {
                operation: "enum_format",
                ..
            })
        ));
    }

    #[test]
    fn format_parser_rejects_changed_index_type_and_invalid_text() {
        let c = constants();
        for raw in [
            bridge::RawFormat {
                index: 1,
                ..format(0, c.buffer_capture, NV12)
            },
            bridge::RawFormat {
                buffer_type: c.buffer_capture_mplane,
                ..format(0, c.buffer_capture, NV12)
            },
            bridge::RawFormat {
                description: [0xff; 32],
                ..format(0, c.buffer_capture, NV12)
            },
        ] {
            malformed(
                parse_format(
                    raw,
                    path(),
                    0,
                    CaptureBufferType::SinglePlanar,
                    c.buffer_capture,
                ),
                "enum_format",
                0,
            );
        }
        let mut raw = format(0, c.buffer_capture, NV12);
        raw.description[1] = 0;
        raw.description[2] = 0xff;
        assert_eq!(
            parse_format(
                raw,
                path(),
                0,
                CaptureBufferType::SinglePlanar,
                c.buffer_capture
            )
            .unwrap()
            .description,
            "x"
        );
    }

    #[test]
    fn querycap_checks_utf8_and_ignores_error_payload() {
        let c = constants();
        malformed(
            parse_capabilities(
                bridge::RawCapabilities {
                    driver: [0xff; 16],
                    ..Default::default()
                },
                path(),
                &c,
            ),
            "query_cap",
            0,
        );
        let result = parse_capabilities(
            bridge::RawCapabilities {
                error: 13,
                driver: [0xff; 16],
                ..Default::default()
            },
            path(),
            &c,
        );
        assert!(matches!(
            result,
            Err(CaptureError::PermissionDenied {
                operation: "query_cap",
                ..
            })
        ));
    }

    #[test]
    fn discrete_sizes_finish_before_exact_intervals_and_share_only_within_node() {
        let c = constants();
        let sizes_finished = Cell::new(0);
        let result = node_capabilities_with(
            path(),
            c.cap_video_capture | c.cap_video_capture_mplane,
            &c,
            |buffer, index| {
                if index == 0 {
                    format(index, buffer, NV12)
                } else {
                    bridge::RawFormat {
                        error: c.einval,
                        ..Default::default()
                    }
                }
            },
            |_, index| {
                if index == 0 {
                    discrete_size(index)
                } else {
                    sizes_finished.set(sizes_finished.get() + 1);
                    bridge::RawSize {
                        error: c.einval,
                        ..Default::default()
                    }
                }
            },
            |fourcc, queried_size, index| {
                assert_eq!(sizes_finished.get(), 1);
                assert_eq!(fourcc, NV12);
                assert_eq!(queried_size, size());
                if index == 0 {
                    interval(index, 60)
                } else {
                    bridge::RawInterval {
                        error: c.einval,
                        ..Default::default()
                    }
                }
            },
        )
        .unwrap();
        assert_eq!(result.formats().len(), 2);
        assert_eq!(result.fourcc_capabilities().len(), 1);
        let requested = CaptureMode {
            captured_fourcc: NV12,
            size: size(),
            rate: FrameRate::new(60, 1).unwrap(),
        };
        assert_eq!(
            result.assess(CaptureBufferType::SinglePlanar, requested),
            SupportVerdict::Supported
        );
        assert_eq!(
            result.assess(CaptureBufferType::MultiPlanar, requested),
            SupportVerdict::Supported
        );
    }

    #[test]
    fn sizes_reject_unknown_mixed_tags_correlation_and_duplicate_dimensions() {
        let c = constants();
        for raw in [
            bridge::RawSize {
                index: 2,
                ..discrete_size(0)
            },
            bridge::RawSize {
                fourcc: YUYV.kernel_value(),
                ..discrete_size(0)
            },
            bridge::RawSize {
                kind: u32::MAX,
                ..discrete_size(0)
            },
            bridge::RawSize {
                width: 0,
                ..discrete_size(0)
            },
        ] {
            malformed(enumerate_sizes(path(), NV12, &c, |_| raw), "enum_size", 0);
        }
        malformed(
            enumerate_sizes(path(), NV12, &c, |index| bridge::RawSize {
                kind: if index == 0 {
                    c.size_discrete
                } else {
                    c.size_continuous
                },
                ..discrete_size(index)
            }),
            "enum_size",
            1,
        );
        malformed(
            enumerate_sizes(path(), NV12, &c, discrete_size),
            "enum_size",
            1,
        );
    }

    #[test]
    fn size_ranges_remain_native_and_never_query_sample_intervals() {
        let c = constants();
        for kind in [c.size_continuous, c.size_stepwise] {
            let result = node_capabilities_with(
                path(),
                c.cap_video_capture,
                &c,
                |buffer, index| {
                    if index == 0 {
                        format(index, buffer, NV12)
                    } else {
                        bridge::RawFormat {
                            error: c.einval,
                            ..Default::default()
                        }
                    }
                },
                |_, index| {
                    assert_eq!(index, 0);
                    bridge::RawSize {
                        fourcc: NV12.kernel_value(),
                        kind,
                        min_width: 640,
                        max_width: 2561,
                        step_width: if kind == c.size_continuous { 0 } else { 16 },
                        min_height: 480,
                        max_height: 1441,
                        step_height: if kind == c.size_continuous { 0 } else { 16 },
                        ..Default::default()
                    }
                },
                |_, _, _| panic!("range must not sample intervals"),
            )
            .unwrap();
            let sizes = result.fourcc_capabilities()[0].sizes.as_option().unwrap();
            assert!(
                matches!((kind, sizes.kind()), (k, FrameSizeKind::Continuous { .. }) if k == c.size_continuous)
                    || matches!((kind, sizes.kind()), (k, FrameSizeKind::Stepwise { .. }) if k == c.size_stepwise)
            );
        }
    }

    #[test]
    fn invalid_size_ranges_are_malformed() {
        let c = constants();
        for raw in [
            bridge::RawSize {
                kind: c.size_stepwise,
                step_width: 0,
                step_height: 1,
                min_width: 1,
                min_height: 1,
                max_width: 2,
                max_height: 2,
                ..discrete_size(0)
            },
            bridge::RawSize {
                kind: c.size_continuous,
                min_width: 3,
                min_height: 1,
                max_width: 2,
                max_height: 2,
                ..discrete_size(0)
            },
        ] {
            malformed(enumerate_sizes(path(), NV12, &c, |_| raw), "enum_size", 0);
        }
    }

    #[test]
    fn optional_descriptors_initial_absence_is_not_support() {
        let c = constants();
        for error in [c.einval, c.enotty] {
            assert!(matches!(
                enumerate_sizes(path(), NV12, &c, |_| bridge::RawSize {
                    error,
                    ..Default::default()
                })
                .unwrap(),
                Descriptor::NotReported
            ));
            assert!(matches!(
                enumerate_intervals(path(), NV12, size(), &c, |_| bridge::RawInterval {
                    error,
                    ..Default::default()
                })
                .unwrap(),
                Descriptor::NotReported
            ));
        }
    }

    #[test]
    fn optional_enumeration_failure_after_entry_propagates() {
        let c = constants();
        for error in [c.enotty, 5, 13] {
            assert!(
                enumerate_sizes(path(), NV12, &c, |index| if index == 0 {
                    discrete_size(index)
                } else {
                    bridge::RawSize {
                        error,
                        ..Default::default()
                    }
                })
                .is_err()
            );
            assert!(
                enumerate_intervals(path(), NV12, size(), &c, |index| if index == 0 {
                    interval(index, 60)
                } else {
                    bridge::RawInterval {
                        error,
                        ..Default::default()
                    }
                })
                .is_err()
            );
        }
    }

    #[test]
    fn intervals_preserve_exact_reciprocals_and_order() {
        let c = constants();
        let result = enumerate_intervals(path(), NV12, size(), &c, |index| match index {
            0 => interval(index, 60),
            1 => bridge::RawInterval {
                numerator: 1001,
                denominator: 60000,
                ..interval(index, 60)
            },
            _ => bridge::RawInterval {
                error: c.einval,
                ..Default::default()
            },
        })
        .unwrap();
        let FrameIntervalKind::Discrete(entries) = result.as_option().unwrap().kind() else {
            panic!("discrete intervals expected")
        };
        assert_eq!(entries[0].rate(), FrameRate::new(60, 1).unwrap());
        assert_eq!(entries[1].rate(), FrameRate::new(60000, 1001).unwrap());
    }

    #[test]
    fn intervals_reject_changed_correlations_unknown_mixed_tags_and_zero_fractions() {
        let c = constants();
        for raw in [
            bridge::RawInterval {
                index: 1,
                ..interval(0, 60)
            },
            bridge::RawInterval {
                fourcc: YUYV.kernel_value(),
                ..interval(0, 60)
            },
            bridge::RawInterval {
                width: 1280,
                ..interval(0, 60)
            },
            bridge::RawInterval {
                height: 720,
                ..interval(0, 60)
            },
            bridge::RawInterval {
                kind: u32::MAX,
                ..interval(0, 60)
            },
            bridge::RawInterval {
                numerator: 0,
                ..interval(0, 60)
            },
            bridge::RawInterval {
                denominator: 0,
                ..interval(0, 60)
            },
        ] {
            malformed(
                enumerate_intervals(path(), NV12, size(), &c, |_| raw),
                "enum_interval",
                0,
            );
        }
        malformed(
            enumerate_intervals(path(), NV12, size(), &c, |index| bridge::RawInterval {
                kind: if index == 0 {
                    c.interval_discrete
                } else {
                    c.interval_stepwise
                },
                ..interval(index, 60)
            }),
            "enum_interval",
            1,
        );
    }

    #[test]
    fn interval_ranges_ignore_continuous_step_but_validate_stepwise_periods() {
        let c = constants();
        for kind in [c.interval_continuous, c.interval_stepwise] {
            let result = enumerate_intervals(path(), NV12, size(), &c, |index| {
                assert_eq!(index, 0);
                bridge::RawInterval {
                    kind,
                    min_numerator: 1,
                    min_denominator: 60,
                    max_numerator: 1,
                    max_denominator: 20,
                    step_numerator: if kind == c.interval_continuous { 0 } else { 1 },
                    step_denominator: if kind == c.interval_continuous { 0 } else { 60 },
                    ..interval(index, 60)
                }
            })
            .unwrap();
            assert!(
                result
                    .as_option()
                    .unwrap()
                    .supports(FrameRate::new(30, 1).unwrap())
            );
            assert_eq!(
                result
                    .as_option()
                    .unwrap()
                    .supports(FrameRate::new(40, 1).unwrap()),
                kind == c.interval_continuous
            );
        }
    }

    #[test]
    fn inverted_interval_ranges_zero_steps_and_duplicates_are_malformed() {
        let c = constants();
        for raw in [
            bridge::RawInterval {
                kind: c.interval_continuous,
                min_numerator: 1,
                min_denominator: 20,
                max_numerator: 1,
                max_denominator: 60,
                ..interval(0, 60)
            },
            bridge::RawInterval {
                kind: c.interval_stepwise,
                min_numerator: 1,
                min_denominator: 60,
                max_numerator: 1,
                max_denominator: 20,
                step_numerator: 0,
                step_denominator: 60,
                ..interval(0, 60)
            },
        ] {
            malformed(
                enumerate_intervals(path(), NV12, size(), &c, |_| raw),
                "enum_interval",
                0,
            );
        }
        malformed(
            enumerate_intervals(path(), NV12, size(), &c, |index| interval(index, 60)),
            "enum_interval",
            1,
        );
    }

    #[test]
    fn all_fourcc_size_sequences_finish_before_any_interval_query() {
        let c = constants();
        let ended = Cell::new(0);
        let capabilities = node_capabilities_with(
            path(),
            c.cap_video_capture,
            &c,
            |buffer, index| match index {
                0 => format(index, buffer, NV12),
                1 => format(index, buffer, YUYV),
                _ => bridge::RawFormat {
                    error: c.einval,
                    ..Default::default()
                },
            },
            |fourcc, index| {
                if index == 0 {
                    bridge::RawSize {
                        fourcc: fourcc.kernel_value(),
                        ..discrete_size(index)
                    }
                } else {
                    ended.set(ended.get() + 1);
                    bridge::RawSize {
                        error: c.einval,
                        ..Default::default()
                    }
                }
            },
            |fourcc, _, index| {
                assert_eq!(ended.get(), 2);
                if index == 0 {
                    bridge::RawInterval {
                        fourcc: fourcc.kernel_value(),
                        denominator: if fourcc == NV12 { 60 } else { 50 },
                        ..interval(index, 60)
                    }
                } else {
                    bridge::RawInterval {
                        error: c.einval,
                        ..Default::default()
                    }
                }
            },
        )
        .unwrap();
        assert_eq!(capabilities.fourcc_capabilities()[0].captured_fourcc, NV12);
        assert_eq!(capabilities.fourcc_capabilities()[1].captured_fourcc, YUYV);
        assert_eq!(
            capabilities.assess(
                CaptureBufferType::SinglePlanar,
                CaptureMode {
                    captured_fourcc: NV12,
                    size: size(),
                    rate: FrameRate::new(60, 1).unwrap()
                }
            ),
            SupportVerdict::Supported
        );
        assert_eq!(
            capabilities.assess(
                CaptureBufferType::SinglePlanar,
                CaptureMode {
                    captured_fourcc: YUYV,
                    size: size(),
                    rate: FrameRate::new(60, 1).unwrap()
                }
            ),
            SupportVerdict::Unsupported(crate::domain::capture::UnsupportedReason::FrameRate)
        );
    }

    #[test]
    fn discrete_sizes_preserve_order_and_query_their_own_exact_dimensions() {
        let c = constants();
        let capabilities = node_capabilities_with(
            path(),
            c.cap_video_capture,
            &c,
            |buffer, index| {
                if index == 0 {
                    format(index, buffer, NV12)
                } else {
                    bridge::RawFormat {
                        error: c.einval,
                        ..Default::default()
                    }
                }
            },
            |_, index| match index {
                0 => bridge::RawSize {
                    width: 1280,
                    height: 720,
                    ..discrete_size(index)
                },
                1 => discrete_size(index),
                _ => bridge::RawSize {
                    error: c.einval,
                    ..Default::default()
                },
            },
            |_, exact, index| {
                if index == 0 {
                    bridge::RawInterval {
                        width: exact.width(),
                        height: exact.height(),
                        denominator: if exact.width() == 1280 { 60 } else { 30 },
                        ..interval(index, 60)
                    }
                } else {
                    bridge::RawInterval {
                        error: c.einval,
                        ..Default::default()
                    }
                }
            },
        )
        .unwrap();
        let FrameSizeKind::Discrete(entries) = capabilities.fourcc_capabilities()[0]
            .sizes
            .as_option()
            .unwrap()
            .kind()
        else {
            panic!("discrete sizes expected")
        };
        assert_eq!(entries[0].size, FrameSize::new(1280, 720).unwrap());
        assert_eq!(entries[1].size, size());
        assert!(
            entries[0]
                .intervals
                .as_option()
                .unwrap()
                .supports(FrameRate::new(60, 1).unwrap())
        );
        assert!(
            !entries[1]
                .intervals
                .as_option()
                .unwrap()
                .supports(FrameRate::new(60, 1).unwrap())
        );
    }

    #[test]
    fn capability_text_fields_all_require_utf8_before_first_nul() {
        let c = constants();
        for raw in [
            bridge::RawCapabilities {
                card: [0xff; 32],
                ..Default::default()
            },
            bridge::RawCapabilities {
                bus_info: [0xff; 32],
                ..Default::default()
            },
        ] {
            malformed(parse_capabilities(raw, path(), &c), "query_cap", 0);
        }
        let mut raw = bridge::RawCapabilities {
            driver: [0xff; 16],
            ..Default::default()
        };
        raw.driver[0] = 0;
        assert_eq!(parse_capabilities(raw, path(), &c).unwrap().driver, "");
    }

    #[test]
    fn eligible_node_without_any_format_is_malformed_not_a_fallback() {
        let c = constants();
        malformed(
            node_capabilities_with(
                path(),
                c.cap_video_capture,
                &c,
                |_, _| bridge::RawFormat {
                    error: c.einval,
                    ..Default::default()
                },
                |_, _| panic!("no formats must not query sizes"),
                |_, _, _| panic!("no formats must not query intervals"),
            ),
            "enum_format",
            0,
        );
    }

    #[test]
    fn zero_fraction_retains_domain_error_and_descriptor_context() {
        let c = constants();
        let result = enumerate_intervals(path(), NV12, size(), &c, |_| interval(0, 0));
        assert!(matches!(
            result,
            Err(CaptureError::MalformedDescriptor {
                operation: "enum_interval",
                index: 0,
                source: CaptureDataError::ZeroRational {
                    field: "denominator"
                },
                ..
            })
        ));
    }

    #[test]
    fn interval_range_pod_with_large_components_uses_exact_domain_arithmetic() {
        let c = constants();
        let maximum = u32::MAX;
        let result = enumerate_intervals(path(), NV12, size(), &c, |_| bridge::RawInterval {
            kind: c.interval_stepwise,
            min_numerator: maximum - 2,
            min_denominator: maximum,
            max_numerator: maximum,
            max_denominator: maximum - 2,
            step_numerator: maximum - 1,
            step_denominator: maximum,
            ..interval(0, 60)
        })
        .unwrap();
        let intervals = result.as_option().unwrap();
        assert!(intervals.supports(FrameRate::new(maximum, maximum - 2).unwrap()));
        assert!(!intervals.supports(FrameRate::new(maximum - 2, maximum).unwrap()));
    }
}
