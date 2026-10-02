use crate::domain::capture::CaptureBufferType;

/// The borrowed fd stays owned by Rust. Native code performs only read-only
/// capability/descriptor ioctls and returns errno separately from the payload.
#[cxx::bridge(namespace = "furami::capture")]
pub(super) mod bridge {
    #[derive(Clone, Copy, Debug, Default)]
    struct RawCapabilities {
        error: i32,
        driver: [u8; 16],
        card: [u8; 32],
        bus_info: [u8; 32],
        capabilities: u32,
        device_caps: u32,
    }

    #[derive(Clone, Copy, Debug, Default)]
    struct RawFormat {
        error: i32,
        index: u32,
        buffer_type: u32,
        fourcc: u32,
        flags: u32,
        description: [u8; 32],
    }

    #[derive(Clone, Copy, Debug, Default)]
    struct RawSize {
        error: i32,
        index: u32,
        fourcc: u32,
        kind: u32,
        width: u32,
        height: u32,
        min_width: u32,
        max_width: u32,
        step_width: u32,
        min_height: u32,
        max_height: u32,
        step_height: u32,
    }

    #[derive(Clone, Copy, Debug, Default)]
    struct RawInterval {
        error: i32,
        index: u32,
        fourcc: u32,
        width: u32,
        height: u32,
        kind: u32,
        numerator: u32,
        denominator: u32,
        min_numerator: u32,
        min_denominator: u32,
        max_numerator: u32,
        max_denominator: u32,
        step_numerator: u32,
        step_denominator: u32,
    }

    #[derive(Clone, Copy, Debug)]
    struct V4l2Constants {
        cap_device_caps: u32,
        cap_video_capture: u32,
        cap_video_capture_mplane: u32,
        buffer_capture: u32,
        buffer_capture_mplane: u32,
        size_discrete: u32,
        size_continuous: u32,
        size_stepwise: u32,
        interval_discrete: u32,
        interval_continuous: u32,
        interval_stepwise: u32,
        einval: i32,
        enotty: i32,
    }

    unsafe extern "C++" {
        include!("v4l2.h");

        fn v4l2_constants() -> V4l2Constants;
        fn query_cap(fd: i32) -> RawCapabilities;
        fn enum_format(fd: i32, buffer_type: u32, index: u32) -> RawFormat;
        fn enum_size(fd: i32, fourcc: u32, index: u32) -> RawSize;
        fn enum_interval(fd: i32, fourcc: u32, width: u32, height: u32, index: u32) -> RawInterval;
    }
}

pub(super) fn effective_capabilities(
    raw: &bridge::RawCapabilities,
    constants: &bridge::V4l2Constants,
) -> u32 {
    if raw.capabilities & constants.cap_device_caps != 0 {
        raw.device_caps
    } else {
        raw.capabilities
    }
}

/// Only announced video-capture queues count. Metadata, output and streaming
/// flags do not gate capture, and unrelated queues never become routes.
pub(super) fn capture_buffer_types(
    effective: u32,
    constants: &bridge::V4l2Constants,
) -> impl Iterator<Item = (CaptureBufferType, u32)> {
    [
        (
            constants.cap_video_capture,
            CaptureBufferType::SinglePlanar,
            constants.buffer_capture,
        ),
        (
            constants.cap_video_capture_mplane,
            CaptureBufferType::MultiPlanar,
            constants.buffer_capture_mplane,
        ),
    ]
    .into_iter()
    .filter_map(move |(bit, buffer_type, kernel_type)| {
        (effective & bit != 0).then_some((buffer_type, kernel_type))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_capabilities_override_global_capture_bits() {
        let constants = bridge::v4l2_constants();
        let raw = bridge::RawCapabilities {
            capabilities: constants.cap_device_caps | constants.cap_video_capture,
            device_caps: 0,
            ..Default::default()
        };
        assert_eq!(effective_capabilities(&raw, &constants), 0);
        assert_eq!(
            capture_buffer_types(effective_capabilities(&raw, &constants), &constants).count(),
            0
        );
    }

    #[test]
    fn capture_queues_are_classified_without_streaming_requirement() {
        let constants = bridge::v4l2_constants();
        assert_eq!(
            capture_buffer_types(constants.cap_video_capture, &constants).collect::<Vec<_>>(),
            [(CaptureBufferType::SinglePlanar, constants.buffer_capture)]
        );
        assert_eq!(
            capture_buffer_types(constants.cap_video_capture_mplane, &constants)
                .collect::<Vec<_>>(),
            [(
                CaptureBufferType::MultiPlanar,
                constants.buffer_capture_mplane
            )]
        );
        assert_eq!(
            capture_buffer_types(
                constants.cap_video_capture | constants.cap_video_capture_mplane,
                &constants
            )
            .count(),
            2
        );
    }

    #[test]
    fn global_capabilities_are_used_without_device_caps_flag() {
        let constants = bridge::v4l2_constants();
        let raw = bridge::RawCapabilities {
            capabilities: constants.cap_video_capture,
            device_caps: constants.cap_video_capture_mplane,
            ..Default::default()
        };
        assert_eq!(
            effective_capabilities(&raw, &constants),
            constants.cap_video_capture
        );
        assert_eq!(
            capture_buffer_types(effective_capabilities(&raw, &constants), &constants)
                .collect::<Vec<_>>(),
            [(CaptureBufferType::SinglePlanar, constants.buffer_capture)]
        );
    }

    #[test]
    fn unrelated_flags_do_not_create_capture_routes() {
        let constants = bridge::v4l2_constants();
        let unrelated = !(constants.cap_video_capture | constants.cap_video_capture_mplane);
        assert_eq!(capture_buffer_types(unrelated, &constants).count(), 0);
        assert_eq!(
            capture_buffer_types(unrelated | constants.cap_video_capture, &constants)
                .collect::<Vec<_>>(),
            [(CaptureBufferType::SinglePlanar, constants.buffer_capture)]
        );
    }
}
