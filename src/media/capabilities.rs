//! FFI-free facts from recorded CLI probes. A listing never proves playback.

use std::collections::HashMap;

/// Content and status read from one manifest command. Callers read raw transcript
/// bytes; this module performs no I/O and never trusts manifest `notes`.
#[derive(Debug, Default)]
pub struct ProbeRun {
    pub argv: Vec<String>,
    pub executable_sha256: String,
    pub ran: bool,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

/// Identity is the SHA-256 of the stack inventory, not a version-string guess.
/// Each command must also match its pinned executable's digest.
#[derive(Debug, Default)]
pub struct ProbeCollection {
    pub stack_sha256: String,
    pub mpv_sha256: String,
    pub ffmpeg_sha256: String,
    /// Resolved stack FFmpeg binary path, for shell-wrapped monitor probes.
    pub ffmpeg_path: String,
    pub commands: HashMap<String, ProbeRun>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilterOrigin {
    MpvNative,
    Lavfi,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CatalogFilter {
    Format,
    Eq,
    Unsharp,
    Hqdn3d,
    Bwdif,
}

impl CatalogFilter {
    pub const fn origin(self) -> FilterOrigin {
        match self {
            Self::Format => FilterOrigin::MpvNative,
            Self::Eq | Self::Unsharp | Self::Hqdn3d | Self::Bwdif => FilterOrigin::Lavfi,
        }
    }

    const fn name(self) -> &'static str {
        match self {
            Self::Format => "format",
            Self::Eq => "eq",
            Self::Unsharp => "unsharp",
            Self::Hqdn3d => "hqdn3d",
            Self::Bwdif => "bwdif",
        }
    }

    const fn runtime_probe(self) -> &'static str {
        match self {
            Self::Format => "mpv_vf_native_format",
            Self::Eq => "mpv_vf_lavfi_eq",
            Self::Unsharp => "mpv_vf_lavfi_unsharp",
            Self::Hqdn3d => "mpv_vf_lavfi_hqdn3d",
            Self::Bwdif => "mpv_vf_lavfi_bwdif",
        }
    }

    const fn runtime_arg(self) -> &'static str {
        match self {
            Self::Format => "--vf=format=fmt=yuv420p",
            Self::Eq => "--vf=lavfi=[eq=contrast=1.06:saturation=1.04]",
            Self::Unsharp => "--vf=lavfi=[unsharp=5:5:0.6]",
            Self::Hqdn3d => "--vf=lavfi=[hqdn3d=1.5:1.5:6:6]",
            Self::Bwdif => {
                "--vf=lavfi=[tinterlace=mode=interleave_top],lavfi=[bwdif=mode=send_field]"
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoOutput {
    GpuNext,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GpuContext {
    X11Vk,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    V4l2,
    Pulse,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat {
    Nv12,
    Yuyv422,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decoder {
    Mjpeg,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureFormat {
    Nv12,
    Yuyv422,
    Mjpeg,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Player {
    Ffmpeg,
    Mpv,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RendererOption {
    Scale,
    Cscale,
    Dscale,
    ScaleAntiring,
    Dither,
    Deband,
    DebandIterations,
    DebandGrain,
}

impl RendererOption {
    const fn name(self) -> &'static str {
        match self {
            Self::Scale => "scale",
            Self::Cscale => "cscale",
            Self::Dscale => "dscale",
            Self::ScaleAntiring => "scale-antiring",
            Self::Dither => "dither",
            Self::Deband => "deband",
            Self::DebandIterations => "deband-iterations",
            Self::DebandGrain => "deband-grain",
        }
    }
}

/// `ListedFilter` proves registration, not functional filtering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Capability<'a> {
    VideoOutput(VideoOutput),
    GpuContext(GpuContext),
    ListedFilter(CatalogFilter),
    OperationalFilter(CatalogFilter),
    Backend(Backend),
    PixelFormat(PixelFormat),
    Decoder(Decoder),
    VulkanRender,
    RendererValue(RendererOption, &'a str),
    LiveInput {
        format: CaptureFormat,
        player: Player,
    },
    PulseSourceCapture,
    PulseSinkMonitor,
    PulseSourcePlayback {
        source: &'a str,
        sink: &'a str,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Component {
    Mpv,
    MpvNativeFilter,
    MpvLavfiBridge,
    FfmpegLibavfilter,
    FfmpegInput,
    FfmpegPixelFormat,
    FfmpegDecoder,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum CapabilityError {
    #[error("missing {component:?} capability in {probe}")]
    Missing {
        component: Component,
        probe: &'static str,
    },
    #[error("unverified probe {probe}: {reason}")]
    Unverified {
        probe: &'static str,
        reason: &'static str,
    },
    #[error("cannot parse probe {probe}: {reason}")]
    Parse {
        probe: &'static str,
        reason: &'static str,
    },
    #[error("probe stack differs from requested stack: expected {expected}, got {actual}")]
    StackMismatch { expected: String, actual: String },
}

/// Construct only from a supplied set of recorded runs. No command executes here.
pub struct Capabilities<'a> {
    probes: &'a ProbeCollection,
}

impl<'a> Capabilities<'a> {
    pub fn from_probes(probes: &'a ProbeCollection) -> Result<Self, CapabilityError> {
        for hash in [
            &probes.stack_sha256,
            &probes.mpv_sha256,
            &probes.ffmpeg_sha256,
        ] {
            if hash.len() != 64 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                return Err(CapabilityError::Parse {
                    probe: "stack_identity",
                    reason: "expected a 64-digit SHA-256 identity",
                });
            }
        }
        Ok(Self { probes })
    }

    /// Require one fact about *this* build. Never pass a local/system stack hash
    /// as a substitute for the packaged build's recorded inventory hash.
    pub fn require<'b>(
        &self,
        expected_stack_sha256: &str,
        capability: Capability<'b>,
    ) -> Result<(), CapabilityError> {
        if expected_stack_sha256 != self.probes.stack_sha256 {
            return Err(CapabilityError::StackMismatch {
                expected: expected_stack_sha256.to_owned(),
                actual: self.probes.stack_sha256.clone(),
            });
        }
        match capability {
            Capability::VideoOutput(VideoOutput::GpuNext) => self.mpv_row(
                "mpv_vo_list",
                "--vo=help",
                "Available video outputs:",
                "gpu-next",
            ),
            Capability::GpuContext(GpuContext::X11Vk) => self.mpv_row(
                "mpv_gpu_context_list",
                "--gpu-context=help",
                "Available GPU contexts:",
                "x11vk",
            ),
            Capability::ListedFilter(filter) => self.listed_filter(filter),
            Capability::OperationalFilter(filter) => {
                self.listed_filter(filter)?;
                let id = filter.runtime_probe();
                let run = self.run(id, Player::Mpv, filter.runtime_arg())?;
                require_mpv_video(id, run, "VO: [null]")
            }
            Capability::Backend(backend) => self.ffmpeg_backend(backend),
            Capability::PixelFormat(format) => self.ffmpeg_pixel_format(format),
            Capability::Decoder(Decoder::Mjpeg) => self.ffmpeg_decoder(),
            Capability::VulkanRender => self.vulkan_render().map(|_| ()),
            Capability::RendererValue(option, value) => {
                let run = self.vulkan_render()?;
                let name = option.name();
                if run.argv.iter().rev().find_map(|arg| {
                    let (key, observed) = arg.strip_prefix("--")?.split_once('=')?;
                    (key == name).then_some(observed)
                }) != Some(value)
                {
                    return Err(CapabilityError::Unverified {
                        probe: "mpv_render_gpu_next_x11vk",
                        reason: "requested renderer value was not last tested value",
                    });
                }
                Ok(())
            }
            Capability::LiveInput { format, player } => self.live_input(format, player),
            Capability::PulseSourceCapture => self.pulse_capture(),
            Capability::PulseSinkMonitor => self.pulse_sink_monitor(),
            Capability::PulseSourcePlayback { source, sink } => {
                self.pulse_source_playback(source, sink)
            }
        }
    }
}

impl Capabilities<'_> {
    fn run(
        &self,
        id: &'static str,
        binary: Player,
        required_arg: &str,
    ) -> Result<&ProbeRun, CapabilityError> {
        let run = self
            .probes
            .commands
            .get(id)
            .ok_or(CapabilityError::Unverified {
                probe: id,
                reason: "probe not recorded",
            })?;
        if !run.ran || run.exit_code.is_none() {
            return Err(CapabilityError::Unverified {
                probe: id,
                reason: "probe not run",
            });
        }
        if run.timed_out {
            return Err(CapabilityError::Unverified {
                probe: id,
                reason: "probe timed out",
            });
        }
        let digest = match binary {
            Player::Mpv => &self.probes.mpv_sha256,
            Player::Ffmpeg => &self.probes.ffmpeg_sha256,
        };
        if &run.executable_sha256 != digest {
            return Err(CapabilityError::Unverified {
                probe: id,
                reason: "executable hash differs from stack",
            });
        }
        if !run.argv.iter().any(|arg| arg == required_arg) {
            return Err(CapabilityError::Parse {
                probe: id,
                reason: "unexpected probe command",
            });
        }
        if required_arg.starts_with("--vf=") {
            effective_vf_is(id, run, required_arg)?;
        }
        if run.exit_code != Some(0) {
            return Err(CapabilityError::Unverified {
                probe: id,
                reason: "probe exited unsuccessfully",
            });
        }
        Ok(run)
    }

    fn mpv_row(
        &self,
        id: &'static str,
        arg: &str,
        header: &str,
        name: &str,
    ) -> Result<(), CapabilityError> {
        let run = self.run(id, Player::Mpv, arg)?;
        let stdout = text(id, &run.stdout)?;
        let mut rows = mpv_section(id, stdout, header)?;
        if rows.any(|line| {
            line.strip_prefix("  ")
                .and_then(|row| row.split_whitespace().next())
                == Some(name)
        }) {
            Ok(())
        } else {
            Err(CapabilityError::Missing {
                component: Component::Mpv,
                probe: id,
            })
        }
    }

    fn listed_filter(&self, filter: CatalogFilter) -> Result<(), CapabilityError> {
        let id = "mpv_vf_list";
        let run = self.run(id, Player::Mpv, "--vf=help")?;
        let stdout = text(id, &run.stdout)?;
        let (header, component) = match filter.origin() {
            FilterOrigin::MpvNative => ("Available video filters:", Component::MpvNativeFilter),
            FilterOrigin::Lavfi => {
                self.ffmpeg_filter(filter.name())?;
                ("Available libavfilter filters:", Component::MpvLavfiBridge)
            }
        };
        let mut rows = mpv_section(id, stdout, header)?;
        if rows.any(|line| {
            line.strip_prefix("  ")
                .and_then(|row| row.split_whitespace().next())
                == Some(filter.name())
        }) {
            Ok(())
        } else {
            Err(CapabilityError::Missing {
                component,
                probe: id,
            })
        }
    }

    fn ffmpeg_filter(&self, name: &str) -> Result<(), CapabilityError> {
        let id = "ffmpeg_filters_list";
        let run = self.run(id, Player::Ffmpeg, "-filters")?;
        let stdout = text(id, &run.stdout)?;
        let mut rows = ffmpeg_rows(id, stdout, "Filters:")?;
        if rows.any(|line| {
            let mut fields = line.split_whitespace();
            let (Some(flags), Some(row_name), Some(pads)) =
                (fields.next(), fields.next(), fields.next())
            else {
                return false;
            };
            flags.len() == 2
                && flags.bytes().all(|b| b == b'T' || b == b'S' || b == b'.')
                && row_name == name
                && pads == "V->V"
        }) {
            Ok(())
        } else {
            Err(CapabilityError::Missing {
                component: Component::FfmpegLibavfilter,
                probe: id,
            })
        }
    }

    fn ffmpeg_backend(&self, backend: Backend) -> Result<(), CapabilityError> {
        let id = "ffmpeg_formats_devices_list";
        let run = self.run(id, Player::Ffmpeg, "-formats")?;
        let stdout = text(id, &run.stdout)?;
        let mut rows = ffmpeg_rows(id, stdout, "Formats:")?;
        let name = match backend {
            Backend::V4l2 => "video4linux2,v4l2",
            Backend::Pulse => "pulse",
        };
        if rows.any(|line| {
            let mut fields = line.split_whitespace();
            // FFmpeg 8 packs the format flags into one three-column token, e.g.
            // "DEd pulse" (D = demuxing/input, E = muxing/output, d = device).
            // Input-capable capture needs column 1 'D' and column 3 'd'.
            let Some(flags) = fields.next() else {
                return false;
            };
            let Some(row_name) = fields.next() else {
                return false;
            };
            flags.len() == 3
                && flags.as_bytes() == [b'D', flags.as_bytes()[1], b'd']
                && (flags.as_bytes()[1] == b'E' || flags.as_bytes()[1] == b'.')
                && row_name == name
        }) {
            Ok(())
        } else {
            Err(CapabilityError::Missing {
                component: Component::FfmpegInput,
                probe: id,
            })
        }
    }

    fn ffmpeg_pixel_format(&self, format: PixelFormat) -> Result<(), CapabilityError> {
        let id = "ffmpeg_pix_fmts_list";
        let run = self.run(id, Player::Ffmpeg, "-pix_fmts")?;
        let stdout = text(id, &run.stdout)?;
        let mut rows = ffmpeg_rows(id, stdout, "Pixel formats:")?;
        let name = match format {
            PixelFormat::Nv12 => "nv12",
            PixelFormat::Yuyv422 => "yuyv422",
        };
        if rows.any(|line| {
            let mut fields = line.split_whitespace();
            let (Some(flags), Some(row_name)) = (fields.next(), fields.next()) else {
                return false;
            };
            flags.len() == 5 && flags.starts_with('I') && row_name == name
        }) {
            Ok(())
        } else {
            Err(CapabilityError::Missing {
                component: Component::FfmpegPixelFormat,
                probe: id,
            })
        }
    }

    fn ffmpeg_decoder(&self) -> Result<(), CapabilityError> {
        let id = "ffmpeg_decoders_list";
        let run = self.run(id, Player::Ffmpeg, "-decoders")?;
        let stdout = text(id, &run.stdout)?;
        let mut rows = ffmpeg_rows(id, stdout, "Decoders:")?;
        if rows.any(|line| {
            let mut fields = line.split_whitespace();
            let (Some(flags), Some(row_name)) = (fields.next(), fields.next()) else {
                return false;
            };
            flags.len() == 6 && flags.starts_with('V') && row_name == "mjpeg"
        }) {
            Ok(())
        } else {
            Err(CapabilityError::Missing {
                component: Component::FfmpegDecoder,
                probe: id,
            })
        }
    }

    fn vulkan_render(&self) -> Result<&ProbeRun, CapabilityError> {
        self.require(
            &self.probes.stack_sha256,
            Capability::VideoOutput(VideoOutput::GpuNext),
        )?;
        self.require(
            &self.probes.stack_sha256,
            Capability::GpuContext(GpuContext::X11Vk),
        )?;
        let id = "mpv_render_gpu_next_x11vk";
        let run = self.run(id, Player::Mpv, "--vo=gpu-next")?;
        if run
            .argv
            .iter()
            .rev()
            .find_map(|arg| arg.strip_prefix("--gpu-context="))
            != Some("x11vk")
            || run
                .argv
                .iter()
                .rev()
                .find_map(|arg| arg.strip_prefix("--gpu-api="))
                != Some("vulkan")
            || !finite_lavfi_source(run)
        {
            return Err(CapabilityError::Parse {
                probe: id,
                reason: "expected finite Vulkan source and context",
            });
        }
        require_mpv_video(id, run, "VO: [gpu-next]")?;
        let stdout = text(id, &run.stdout)?;
        if !stdout.contains("[vo/gpu-next/vulkan] Initializing GPU context 'x11vk'")
            || !selected_vulkan_hardware(stdout)
        {
            return Err(CapabilityError::Unverified {
                probe: id,
                reason: "Vulkan context or selected physical device not recorded",
            });
        }
        Ok(run)
    }

    fn live_input(&self, format: CaptureFormat, player: Player) -> Result<(), CapabilityError> {
        self.ffmpeg_backend(Backend::V4l2)?;
        match format {
            CaptureFormat::Nv12 => self.ffmpeg_pixel_format(PixelFormat::Nv12)?,
            CaptureFormat::Yuyv422 => self.ffmpeg_pixel_format(PixelFormat::Yuyv422)?,
            CaptureFormat::Mjpeg => self.ffmpeg_decoder()?,
        }
        let id = match (format, player) {
            (CaptureFormat::Nv12, Player::Ffmpeg) => "v4l2_open_nv12_ffmpeg",
            (CaptureFormat::Yuyv422, Player::Ffmpeg) => "v4l2_open_yuyv_ffmpeg",
            (CaptureFormat::Mjpeg, Player::Ffmpeg) => "v4l2_open_mjpeg_ffmpeg",
            (CaptureFormat::Nv12, Player::Mpv) => "v4l2_open_nv12_mpv",
            (CaptureFormat::Yuyv422, Player::Mpv) => "v4l2_open_yuyv_mpv",
            (CaptureFormat::Mjpeg, Player::Mpv) => {
                return Err(CapabilityError::Unverified {
                    probe: "v4l2_open_mjpeg_mpv",
                    reason: "MJPEG mpv open not probed",
                });
            }
        };
        match player {
            Player::Ffmpeg => {
                let run = self.run(id, player, "-f")?;
                let expected_format = match format {
                    CaptureFormat::Nv12 => "nv12",
                    CaptureFormat::Yuyv422 => "yuyv422",
                    CaptureFormat::Mjpeg => "mjpeg",
                };
                if !run
                    .argv
                    .windows(2)
                    .any(|a| a[0] == "-input_format" && a[1] == expected_format)
                    || !run.argv.windows(2).any(|a| a[0] == "-f" && a[1] == "v4l2")
                {
                    return Err(CapabilityError::Parse {
                        probe: id,
                        reason: "unexpected V4L2 input format",
                    });
                }
                let stderr = text(id, &run.stderr)?;
                let codec = if format == CaptureFormat::Mjpeg {
                    "Video: mjpeg"
                } else {
                    "Video: rawvideo"
                };
                let stream_matches = stderr.lines().any(|line| {
                    line.contains(codec)
                        && (format == CaptureFormat::Mjpeg
                            || line
                                .split(|c: char| !c.is_ascii_alphanumeric())
                                .any(|part| part.eq_ignore_ascii_case(expected_format)))
                });
                if !stderr.contains("Input #0, video4linux2,v4l2")
                    || !stream_matches
                    || !has_final_frame_count(stderr, 8)
                {
                    return Err(CapabilityError::Unverified {
                        probe: id,
                        reason: "video stream or eight decoded frames not observed",
                    });
                }
                Ok(())
            }
            Player::Mpv => {
                let run = self.run(id, player, "--demuxer-lavf-format=v4l2")?;
                let expected_format = match format {
                    CaptureFormat::Nv12 => "input_format=nv12",
                    CaptureFormat::Yuyv422 => "input_format=yuyv422",
                    CaptureFormat::Mjpeg => unreachable!(),
                };
                let options = run
                    .argv
                    .iter()
                    .rev()
                    .find_map(|a| a.strip_prefix("--demuxer-lavf-o="));
                let valid_options = options.is_some_and(|value| {
                    let mut parts = value.split(',');
                    parts.next() == Some(expected_format)
                        && parts
                            .next()
                            .and_then(|part| part.strip_prefix("video_size="))
                            .is_some_and(|size| !size.is_empty() && !size.contains(':'))
                        && parts
                            .next()
                            .and_then(|part| part.strip_prefix("framerate="))
                            .is_some_and(|rate| !rate.is_empty() && !rate.contains(':'))
                        && parts.next().is_none()
                });
                if !valid_options || !run.argv.iter().any(|a| a.starts_with("av://v4l2:")) {
                    return Err(CapabilityError::Parse {
                        probe: id,
                        reason: "unexpected mpv V4L2 input",
                    });
                }
                let stdout = text(id, &run.stdout)?;
                if !stdout.contains("Video  --vid=")
                    || !stdout.contains("VO: [")
                    || contains_failure(id, run)?
                {
                    return Err(CapabilityError::Unverified {
                        probe: id,
                        reason: "video stream or mpv video output not observed",
                    });
                }
                Ok(())
            }
        }
    }

    fn pulse_capture(&self) -> Result<(), CapabilityError> {
        self.ffmpeg_backend(Backend::Pulse)?;
        let id = "pulse_source_capture";
        let run = self.run(id, Player::Ffmpeg, "pulse")?;
        if !run.argv.windows(2).any(|a| a[0] == "-f" && a[1] == "pulse") {
            return Err(CapabilityError::Parse {
                probe: id,
                reason: "unexpected pulse input",
            });
        }
        let stderr = text(id, &run.stderr)?;
        if !stderr.contains("Input #0, pulse") || !has_volume_levels(stderr) {
            return Err(CapabilityError::Unverified {
                probe: id,
                reason: "pulse input or measured samples not observed",
            });
        }
        Ok(())
    }

    fn pulse_sink_monitor(&self) -> Result<(), CapabilityError> {
        let id = "pulse_sink_tone_monitor";
        // Shell wrapper launches playback and recording. Its executable hash
        // identifies sh, not FFmpeg; bind the FFmpeg argument to the stack.
        let run = self
            .probes
            .commands
            .get(id)
            .ok_or(CapabilityError::Unverified {
                probe: id,
                reason: "probe not recorded",
            })?;
        if !run.ran || run.exit_code != Some(0) || run.timed_out {
            return Err(CapabilityError::Unverified {
                probe: id,
                reason: "sink monitor probe not completed",
            });
        }
        let [shell, option, script, placeholder, ffmpeg, sink] = run.argv.as_slice() else {
            return Err(CapabilityError::Unverified {
                probe: id,
                reason: "unexpected sink monitor wrapper",
            });
        };
        if shell != "sh"
            || option != "-c"
            || placeholder != "sh"
            || self.probes.ffmpeg_path.is_empty()
            || ffmpeg != &self.probes.ffmpeg_path
            || sink.is_empty()
            || !script.contains("-f pulse -device \"$2\"")
            || !script.contains("-f pulse -i \"$2.monitor\"")
        {
            return Err(CapabilityError::Unverified {
                probe: id,
                reason: "wrapper not bound to requested sink and stack FFmpeg",
            });
        }
        let stdout = text(id, &run.stdout)?;
        let Some((monitor, result)) = stdout.split_once("--- monitor volumedetect rc=") else {
            return Err(CapabilityError::Unverified {
                probe: id,
                reason: "monitor process result absent",
            });
        };
        let tone_result = result.lines().nth(1);
        if result.lines().next() != Some("0 ---")
            || !matches!(
                tone_result,
                Some("--- tone rc=0 (143 = our TERM) ---" | "--- tone rc=143 (143 = our TERM) ---")
            )
            || !monitor.lines().any(|line| {
                line.strip_prefix("Input #0, pulse, from '")
                    .and_then(|source| source.strip_suffix("':"))
                    .and_then(|source| source.strip_suffix(".monitor"))
                    == Some(sink.as_str())
            })
            || !has_volume_levels(monitor)
            || !has_non_silent_max_volume(monitor)
        {
            return Err(CapabilityError::Unverified {
                probe: id,
                reason: "sink monitor signal or child success not observed",
            });
        }
        Ok(())
    }
    fn pulse_source_playback(&self, source: &str, sink: &str) -> Result<(), CapabilityError> {
        const ID: &str = "pulse_direct_source_to_sink";
        let run = self
            .probes
            .commands
            .get(ID)
            .ok_or(CapabilityError::Unverified {
                probe: ID,
                reason: "probe not recorded",
            })?;
        if !run.ran || run.exit_code != Some(0) || run.timed_out {
            return Err(CapabilityError::Unverified {
                probe: ID,
                reason: "relay probe not completed",
            });
        }
        let [script, ffmpeg, actual_source, actual_sink] = run.argv.as_slice() else {
            return Err(CapabilityError::Parse {
                probe: ID,
                reason: "unexpected relay command",
            });
        };
        if script.rsplit('/').next() != Some("pulse-direct.sh")
            || run.executable_sha256.len() != 64
            || !run.executable_sha256.bytes().all(|b| b.is_ascii_hexdigit())
            || ffmpeg != &self.probes.ffmpeg_path
            || self.probes.ffmpeg_path.is_empty()
            || actual_source != source
            || actual_sink != sink
            || source.is_empty()
            || sink.is_empty()
        {
            return Err(CapabilityError::Parse {
                probe: ID,
                reason: "relay not bound to requested source, sink and stack FFmpeg",
            });
        }
        let stdout = text(ID, &run.stdout)?;
        let sections = [
            "--- relay stream=",
            "--- sinks ---",
            "--- sinks rc=0 ---",
            "--- sink-inputs ---",
            "--- sink-inputs rc=0 ---",
            "--- pw-link ---",
            "--- pw-link rc=0 ---",
            "--- sink monitor ---",
            "--- monitor rc=0 ---",
            "--- relay transcript ---",
            "--- relay rc=0 ---",
            "--- ffmpeg sha256 ---",
            "--- ffmpeg sha256 rc=0 ---",
        ];
        let mut remainder = stdout;
        let mut parts = Vec::with_capacity(sections.len());
        for marker in sections {
            let Some((before, after)) = remainder.split_once(marker) else {
                return Err(CapabilityError::Unverified {
                    probe: ID,
                    reason: "relay snapshot or child status missing",
                });
            };
            parts.push(before);
            remainder = after;
        }
        let stream = parts[1].trim().strip_suffix(" ---").unwrap_or("");
        if !stream.starts_with("furami-direct-source-to-sink-")
            || !stream["furami-direct-source-to-sink-".len()..]
                .bytes()
                .all(|b| b.is_ascii_digit())
            || stream.len() == "furami-direct-source-to-sink-".len()
            || !remainder.trim().is_empty()
        {
            return Err(CapabilityError::Parse {
                probe: ID,
                reason: "invalid relay stream identity or trailing status",
            });
        }
        if parts[12].trim().split_once("  ")
            != Some((self.probes.ffmpeg_sha256.as_str(), ffmpeg.as_str()))
        {
            return Err(CapabilityError::Unverified {
                probe: ID,
                reason: "relay FFmpeg digest differs from stack",
            });
        }
        let relay = parts[10];
        let monitor = parts[8];
        if !relay.lines().any(|line| {
            line.strip_prefix("Input #0, pulse, from '")
                .and_then(|name| name.strip_suffix("':"))
                == Some(source)
        }) || !relay.lines().any(|line| {
            line.strip_prefix("Output #0, pulse, to '")
                .and_then(|name| name.strip_suffix("':"))
                == Some(stream)
        }) || !has_muxed_pulse_audio(relay)
            || !relay.contains("mean_volume:")
            || !has_non_silent_max_volume(relay)
            || !monitor.lines().any(|line| {
                line.strip_prefix("Input #0, pulse, from '")
                    .and_then(|name| name.strip_suffix("':"))
                    .and_then(|name| name.strip_suffix(".monitor"))
                    == Some(sink)
            })
            || !has_volume_levels(monitor)
            || !has_non_silent_max_volume(monitor)
        {
            return Err(CapabilityError::Unverified {
                probe: ID,
                reason: "live source or sink monitor signal absent",
            });
        }
        let sinks = parts[2];
        let mut sink_ids = sinks.lines().filter_map(|line| {
            let mut columns = line.split_whitespace();
            let index = columns.next()?;
            (columns.next() == Some(sink)).then_some(index)
        });
        let Some(sink_id) = sink_ids.next() else {
            return Err(CapabilityError::Unverified {
                probe: ID,
                reason: "requested sink not uniquely listed",
            });
        };
        if sink_ids.next().is_some() {
            return Err(CapabilityError::Unverified {
                probe: ID,
                reason: "requested sink not uniquely listed",
            });
        }
        let sink_inputs = parts[4];
        let mut matched = sink_inputs.split("Sink Input #").skip(1).filter(|block| {
            block.lines().any(|line| {
                line.trim()
                    .strip_prefix("media.name = \"")
                    .and_then(|name| name.strip_suffix('"'))
                    == Some(stream)
            })
        });
        let Some(own_input) = matched.next() else {
            return Err(CapabilityError::Unverified {
                probe: ID,
                reason: "relay sink-input route not observed at requested sink",
            });
        };
        if matched.next().is_some()
            || !own_input
                .lines()
                .any(|line| line.trim().strip_prefix("Sink: ") == Some(sink_id))
        {
            return Err(CapabilityError::Unverified {
                probe: ID,
                reason: "relay sink-input route not observed at requested sink",
            });
        }
        // pactl identifies this uniquely named stream and its actual sink ID.
        // PipeWire node names need not equal Pulse media.name; an empty scoped
        // graph cannot negate the sink-input route. A contradictory visible
        // own-output link, however, must not be interpreted as success.
        let graph = parts[6];
        for (index, line) in graph.lines().enumerate() {
            if !line
                .strip_prefix(stream)
                .is_some_and(|port| port.starts_with(":output_"))
            {
                continue;
            }
            if graph
                .lines()
                .skip(index + 1)
                .take_while(|next| next.starts_with("  |-> "))
                .any(|next| {
                    !next
                        .trim_start()
                        .strip_prefix("|-> ")
                        .and_then(|peer| peer.strip_prefix(sink))
                        .is_some_and(|port| port.starts_with(":playback_"))
                })
            {
                return Err(CapabilityError::Unverified {
                    probe: ID,
                    reason: "relay PipeWire link contradicts sink-input route",
                });
            }
        }
        Ok(())
    }
}

fn text<'a>(id: &'static str, bytes: &'a [u8]) -> Result<&'a str, CapabilityError> {
    std::str::from_utf8(bytes).map_err(|_| CapabilityError::Parse {
        probe: id,
        reason: "transcript is not UTF-8",
    })
}

fn mpv_section<'a>(
    id: &'static str,
    output: &'a str,
    header: &str,
) -> Result<impl Iterator<Item = &'a str>, CapabilityError> {
    let rest = output
        .split_once(header)
        .map(|(_, rest)| rest)
        .ok_or(CapabilityError::Parse {
            probe: id,
            reason: "mpv listing header absent",
        })?;
    Ok(rest
        .lines()
        .take_while(|line| !line.starts_with("Available ") && !line.starts_with("If libavfilter")))
}

fn ffmpeg_rows<'a>(
    id: &'static str,
    output: &'a str,
    header: &str,
) -> Result<impl Iterator<Item = &'a str>, CapabilityError> {
    let rest = output.strip_prefix(header).ok_or(CapabilityError::Parse {
        probe: id,
        reason: "FFmpeg listing header absent",
    })?;
    if !rest.lines().any(|line| line.trim().starts_with("---")) {
        return Err(CapabilityError::Parse {
            probe: id,
            reason: "FFmpeg table separator absent",
        });
    }
    Ok(rest.lines())
}

fn contains_failure(id: &'static str, run: &ProbeRun) -> Result<bool, CapabilityError> {
    let stdout = text(id, &run.stdout)?;
    let stderr = text(id, &run.stderr)?;
    Ok([stdout, stderr].iter().any(|log| {
        [
            "Error parsing option",
            "Unknown option",
            "Failed to initialize video output",
            "Could not initialize video chain",
            "Failed to initialize a decoder",
        ]
        .iter()
        .any(|marker| log.contains(marker))
    }))
}

fn finite_lavfi_source(run: &ProbeRun) -> bool {
    run.argv
        .iter()
        .any(|arg| arg.starts_with("av://lavfi:testsrc2") && arg.contains(":d="))
        && run.argv.iter().any(|arg| arg == "--frames=90")
}

// mpv v0.41 list options mutate state in order: `--vf-add`/`--vf-pre` append,
// `--vf-del` removes, `--vf-clr` clears, and each later `--vf` overrides all
// earlier ones. The required filter is proven only when it is the effective
// final chain; any mutation leaves the run unable to attest it.
fn effective_vf_is(
    id: &'static str,
    run: &ProbeRun,
    required: &str,
) -> Result<(), CapabilityError> {
    const VF_KEYS: [&str; 6] = ["vf", "vf-add", "vf-pre", "vf-del", "vf-clr", "vf-set"];
    let required_value = &required["--vf=".len()..];
    let mut effective: Option<&str> = None;
    for arg in &run.argv {
        let Some(rest) = arg.strip_prefix("--") else {
            continue;
        };
        let (key, value) = rest.split_once('=').unwrap_or((rest, ""));
        if !VF_KEYS.contains(&key) {
            continue;
        }
        if key == "vf" {
            effective = Some(value);
        } else {
            return Err(CapabilityError::Parse {
                probe: id,
                reason: "vf list mutated after or beside required filter",
            });
        }
    }
    if effective != Some(required_value) {
        return Err(CapabilityError::Parse {
            probe: id,
            reason: "required vf not the effective final filter chain",
        });
    }
    Ok(())
}

fn require_mpv_video(id: &'static str, run: &ProbeRun, vo: &str) -> Result<(), CapabilityError> {
    let stdout = text(id, &run.stdout)?;
    if !finite_lavfi_source(run) {
        return Err(CapabilityError::Parse {
            probe: id,
            reason: "expected finite video fixture",
        });
    }
    if !stdout.contains(vo)
        || !stdout.contains("Exiting... (End of file)")
        || contains_failure(id, run)?
    {
        return Err(CapabilityError::Unverified {
            probe: id,
            reason: "video output or complete fixture playback not observed",
        });
    }
    Ok(())
}

// Candidate GPU rows alone do not prove which device libplacebo created. Its
// selected-device properties precede creation, and must identify hardware.
fn selected_vulkan_hardware(stdout: &str) -> bool {
    const PREFIX: &str = "[vo/gpu-next/libplacebo] ";
    let Some((candidates, selected)) =
        stdout.split_once("[vo/gpu-next/libplacebo] Vulkan device properties:")
    else {
        return false;
    };
    let Some((properties, _)) =
        selected.split_once("[vo/gpu-next/libplacebo] Creating vulkan device with extensions:")
    else {
        return false;
    };
    let name = properties
        .lines()
        .find_map(|line| line.strip_prefix("[vo/gpu-next/libplacebo]     Device Name: "))
        .filter(|value| !value.is_empty());
    let driver = properties
        .lines()
        .find_map(|line| line.strip_prefix("[vo/gpu-next/libplacebo]     Driver name: "))
        .filter(|value| !value.is_empty());
    name.is_some_and(|name| {
        driver.is_some()
            && candidates.lines().any(|line| {
                line.strip_prefix(PREFIX).is_some_and(|row| {
                    row.trim_start().starts_with("GPU ")
                        && row.contains(name)
                        && (row.ends_with("(discrete)") || row.ends_with("(integrated)"))
                })
            })
    })
}

fn has_final_frame_count(output: &str, expected: usize) -> bool {
    output
        .rsplit("frame=")
        .next()
        .and_then(|part| part.split_whitespace().next())
        .and_then(|count| count.parse::<usize>().ok())
        .is_some_and(|count| count >= expected)
}

fn has_volume_levels(output: &str) -> bool {
    let Some((_, levels)) = output.rsplit_once("n_samples:") else {
        return false;
    };
    levels
        .split_whitespace()
        .next()
        .and_then(|n| n.parse::<u64>().ok())
        .is_some_and(|n| n > 0)
        && levels.lines().any(|line| line.contains("mean_volume:"))
        && levels.lines().any(|line| line.contains("max_volume:"))
}

fn has_muxed_pulse_audio(output: &str) -> bool {
    output
        .lines()
        .filter(|line| line.contains("[out#0/pulse"))
        .any(|line| {
            line.split_whitespace()
                .find_map(|part| part.strip_prefix("audio:"))
                .and_then(|amount| amount.strip_suffix("KiB"))
                .and_then(|amount| amount.parse::<u64>().ok())
                .is_some_and(|amount| amount > 0)
        })
}

fn has_non_silent_max_volume(output: &str) -> bool {
    output
        .rsplit_once("max_volume:")
        .and_then(|(_, value)| value.split_whitespace().next())
        .and_then(|value| value.parse::<f64>().ok())
        // FFmpeg's s16 volumedetect reports digital silence at -91.0 dB.
        .is_some_and(|db| db > -91.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Selected real host-system CLI rows (2026-09-27), joined for parser
    // fixtures. Not full transcripts, not pinned-stack or hardware evidence.
    const SYSTEM_MPV_VF: &str = "Available video filters:\n  format           force output format\n  lavfi            libavfilter bridge\n  lavfi-bridge     libavfilter bridge (explicit options)\n\nAvailable libavfilter filters:\n  eq               Adjust brightness, contrast, gamma, and saturation.\n  hqdn3d           Apply a High Quality 3D Denoiser.\n  unsharp          Sharpen or blur the input video.\n  bwdif            Deinterlace the input image.\n";
    const SYSTEM_FFMPEG_FILTERS: &str = "Filters:\n  T.. = Timeline support\n  .S. = Slice threading\n  A = Audio input/output\n  V = Video input/output\n  N = Dynamic number and/or type of input/output\n  | = Source or sink filter\n  ------\n T. eq                V->V       Adjust brightness, contrast, gamma, and saturation.\n TS hqdn3d            V->V       Apply a High Quality 3D Denoiser.\n TS unsharp           V->V       Sharpen or blur the input video.\n TS bwdif             V->V       Deinterlace the input image.\n";
    const SYSTEM_FFMPEG_FORMATS: &str = "Formats:\n D.. = Demuxing supported\n .E. = Muxing supported\n ..d = Is a device\n ---\n D   png_pipe        piped png sequence\n DEd pulse           Pulse audio output\n DEd video4linux2,v4l2 Video4Linux2 output device\n";
    const SYSTEM_FFMPEG_PIXELS: &str = "Pixel formats:\nI.... = Supported Input  format for conversion\n.O... = Supported Output format for conversion\n..H.. = Hardware accelerated format\n...P. = Paletted format\n....B = Bitstream format\nFLAGS NAME            NB_COMPONENTS BITS_PER_PIXEL BIT_DEPTHS\n-----\nIO... yuyv422                3             16      8-8-8\nIO... nv12                   3             12      8-8-8\n";
    const SYSTEM_FFMPEG_DECODERS: &str = "Decoders:\n V..... = Video\n A..... = Audio\n S..... = Subtitle\n .F.... = Frame-level multithreading\n ..S... = Slice-level multithreading\n ...X.. = Codec is experimental\n ....B. = Supports draw_horiz_band\n .....D = Supports direct rendering method 1\n ------\n V....D mjpeg                MJPEG (Motion JPEG)\n";
    const SYSTEM_MPV_VO: &str = "Available video outputs:\n  gpu-next         Video output based on libplacebo\n  gpu              Shader-based GPU Renderer\n  null             Null video output\n\n";

    const FIXTURE_STACK: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const FIXTURE_MPV: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const FIXTURE_FFMPEG: &str = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";

    fn run(argv: &[&str], binary: &str, output: &str) -> ProbeRun {
        ProbeRun {
            argv: argv.iter().map(|s| (*s).to_owned()).collect(),
            executable_sha256: binary.to_owned(),
            ran: true,
            exit_code: Some(0),
            timed_out: false,
            stdout: output.as_bytes().to_vec(),
            stderr: Vec::new(),
        }
    }

    fn probes() -> ProbeCollection {
        ProbeCollection {
            stack_sha256: FIXTURE_STACK.to_owned(),
            mpv_sha256: FIXTURE_MPV.to_owned(),
            ffmpeg_sha256: FIXTURE_FFMPEG.to_owned(),
            ffmpeg_path: "/usr/bin/ffmpeg".to_owned(),
            commands: HashMap::new(),
        }
    }

    #[test]
    fn native_filter_and_lavfi_filter_remain_distinct() {
        let mut input = probes();
        input.commands.insert(
            "mpv_vf_list".into(),
            run(&["mpv", "--vf=help"], FIXTURE_MPV, SYSTEM_MPV_VF),
        );
        input.commands.insert(
            "ffmpeg_filters_list".into(),
            run(
                &["ffmpeg", "-filters"],
                FIXTURE_FFMPEG,
                SYSTEM_FFMPEG_FILTERS,
            ),
        );
        let facts = Capabilities::from_probes(&input).unwrap();
        assert!(
            facts
                .require(
                    FIXTURE_STACK,
                    Capability::ListedFilter(CatalogFilter::Format)
                )
                .is_ok()
        );
        assert!(
            facts
                .require(FIXTURE_STACK, Capability::ListedFilter(CatalogFilter::Eq))
                .is_ok()
        );
        assert_eq!(CatalogFilter::Format.origin(), FilterOrigin::MpvNative);
        assert_eq!(CatalogFilter::Eq.origin(), FilterOrigin::Lavfi);
        assert!(matches!(
            facts.require(
                FIXTURE_STACK,
                Capability::OperationalFilter(CatalogFilter::Eq)
            ),
            Err(CapabilityError::Unverified { .. })
        ));
    }

    #[test]
    fn ffmpeg_rows_prove_only_their_own_components() {
        let mut input = probes();
        input.commands.insert(
            "ffmpeg_formats_devices_list".into(),
            run(
                &["ffmpeg", "-formats"],
                FIXTURE_FFMPEG,
                SYSTEM_FFMPEG_FORMATS,
            ),
        );
        input.commands.insert(
            "ffmpeg_pix_fmts_list".into(),
            run(
                &["ffmpeg", "-pix_fmts"],
                FIXTURE_FFMPEG,
                SYSTEM_FFMPEG_PIXELS,
            ),
        );
        input.commands.insert(
            "ffmpeg_decoders_list".into(),
            run(
                &["ffmpeg", "-decoders"],
                FIXTURE_FFMPEG,
                SYSTEM_FFMPEG_DECODERS,
            ),
        );
        let facts = Capabilities::from_probes(&input).unwrap();
        assert!(
            facts
                .require(FIXTURE_STACK, Capability::Backend(Backend::V4l2))
                .is_ok()
        );
        assert!(
            facts
                .require(FIXTURE_STACK, Capability::Backend(Backend::Pulse))
                .is_ok()
        );
        assert!(
            facts
                .require(FIXTURE_STACK, Capability::PixelFormat(PixelFormat::Nv12))
                .is_ok()
        );
        assert!(
            facts
                .require(FIXTURE_STACK, Capability::PixelFormat(PixelFormat::Yuyv422))
                .is_ok()
        );
        assert!(
            facts
                .require(FIXTURE_STACK, Capability::Decoder(Decoder::Mjpeg))
                .is_ok()
        );
        assert!(matches!(
            facts.require(
                FIXTURE_STACK,
                Capability::LiveInput {
                    format: CaptureFormat::Nv12,
                    player: Player::Ffmpeg
                }
            ),
            Err(CapabilityError::Unverified { .. })
        ));
    }

    // FFmpeg 8.1.2 packs format flags into one token (e.g. "DEd"). Only a
    // demuxing device row proves an input-capable capture backend; mux-only,
    // non-device, and alias-mismatch rows must all be rejected.
    #[test]
    fn ffmpeg_backend_requires_input_device_flag_and_exact_alias() {
        let make = |rows: &str| {
            let stdout = format!(
                "Formats:\n D.. = Demuxing supported\n .E. = Muxing supported\n ..d = Is a device\n ---\n{rows}"
            );
            let mut input = probes();
            input.commands.insert(
                "ffmpeg_formats_devices_list".into(),
                run(&["ffmpeg", "-formats"], FIXTURE_FFMPEG, &stdout),
            );
            input
        };
        let missing = |input: ProbeCollection, backend: Backend| {
            let facts = Capabilities::from_probes(&input).unwrap();
            assert!(matches!(
                facts.require(FIXTURE_STACK, Capability::Backend(backend)),
                Err(CapabilityError::Missing {
                    component: Component::FfmpegInput,
                    ..
                })
            ));
        };
        // Mux-only (no D): the real "Video4Linux2 output device" description
        // wording must not bypass the flag check.
        missing(
            make(" DE  video4linux2,v4l2 Video4Linux2 output device\n"),
            Backend::V4l2,
        );
        // Demuxing but not a device (no d).
        missing(
            make(" DE  pulse           Pulse audio (no device flag)\n"),
            Backend::Pulse,
        );
        // Non-device demuxer sharing a name fragment.
        missing(
            make(" D   pulse_raw      raw pulse container\n"),
            Backend::Pulse,
        );
        // Wrong alias: FFmpeg lists the combined name, not "v4l2" alone.
        missing(
            make(" DEd v4l2           Video4Linux2 device grab\n"),
            Backend::V4l2,
        );
        // The real combined-token rows must pass.
        let input = make(
            " DEd pulse           Pulse audio output\n DEd video4linux2,v4l2 Video4Linux2 output device\n",
        );
        let facts = Capabilities::from_probes(&input).unwrap();
        assert!(
            facts
                .require(FIXTURE_STACK, Capability::Backend(Backend::V4l2))
                .is_ok()
        );
        assert!(
            facts
                .require(FIXTURE_STACK, Capability::Backend(Backend::Pulse))
                .is_ok()
        );
    }

    #[test]
    fn successful_listing_cannot_be_reused_as_different_category() {
        let mut input = probes();
        input.commands.insert(
            "mpv_vo_list".into(),
            run(&["mpv", "--vo=help"], FIXTURE_MPV, SYSTEM_MPV_VO),
        );
        let facts = Capabilities::from_probes(&input).unwrap();
        assert!(
            facts
                .require(FIXTURE_STACK, Capability::VideoOutput(VideoOutput::GpuNext))
                .is_ok()
        );
        assert_eq!(
            facts.mpv_row(
                "mpv_vo_list",
                "--vo=help",
                "Available video outputs:",
                "x11vk"
            ),
            Err(CapabilityError::Missing {
                component: Component::Mpv,
                probe: "mpv_vo_list"
            }),
        );
        assert!(matches!(
            facts.require(FIXTURE_STACK, Capability::GpuContext(GpuContext::X11Vk)),
            Err(CapabilityError::Unverified { .. })
        ));
        assert!(matches!(
            facts.require(FIXTURE_STACK, Capability::VulkanRender),
            Err(CapabilityError::Unverified { .. })
        ));
    }

    #[test]
    fn incomplete_failed_or_wrong_binary_probe_never_proves_a_listing() {
        let mut input = probes();
        let mut entry = run(&["mpv", "--vo=help"], FIXTURE_MPV, SYSTEM_MPV_VO);
        entry.ran = false;
        entry.exit_code = None;
        input.commands.insert("mpv_vo_list".into(), entry);
        let facts = Capabilities::from_probes(&input).unwrap();
        assert!(matches!(
            facts.require(FIXTURE_STACK, Capability::VideoOutput(VideoOutput::GpuNext)),
            Err(CapabilityError::Unverified { .. })
        ));
        let entry = input.commands.get_mut("mpv_vo_list").unwrap();
        entry.ran = true;
        entry.exit_code = Some(1);
        assert!(matches!(
            Capabilities::from_probes(&input)
                .unwrap()
                .require(FIXTURE_STACK, Capability::VideoOutput(VideoOutput::GpuNext)),
            Err(CapabilityError::Unverified { .. })
        ));
        let entry = input.commands.get_mut("mpv_vo_list").unwrap();
        entry.exit_code = Some(0);
        entry.executable_sha256 = FIXTURE_FFMPEG.to_owned();
        assert!(matches!(
            Capabilities::from_probes(&input)
                .unwrap()
                .require(FIXTURE_STACK, Capability::VideoOutput(VideoOutput::GpuNext)),
            Err(CapabilityError::Unverified { .. })
        ));
    }

    #[test]
    fn malformed_output_is_parse_error_and_stack_mismatch_precedes_lookup() {
        let mut input = probes();
        input.commands.insert(
            "mpv_vo_list".into(),
            run(&["mpv", "--vo=help"], FIXTURE_MPV, "gpu-next is great\n"),
        );
        let facts = Capabilities::from_probes(&input).unwrap();
        assert!(matches!(
            facts.require(FIXTURE_STACK, Capability::VideoOutput(VideoOutput::GpuNext)),
            Err(CapabilityError::Parse { .. })
        ));
        assert!(matches!(
            facts.require(
                FIXTURE_FFMPEG,
                Capability::VideoOutput(VideoOutput::GpuNext)
            ),
            Err(CapabilityError::StackMismatch { .. })
        ));
    }

    // Captured from host /usr/bin/mpv --vo=null --vf=<each collector value>
    // on 2026-09-27. This is parser evidence only, never pinned-stack evidence.
    const SYSTEM_MPV_FILTER_RUN: &str = "● Video  --vid=1  (wrapped_avframe 320x240 60 fps)\nVO: [null] 320x240 yuv420p\n[vo/null] reconfig to 320x240 yuv420p bt.601/bt.709/bt.1886/limited/display CL=mpeg2/4/h264 crop=320x240+0+0 A=none\nExiting... (End of file)\n";

    fn with_listings(input: &mut ProbeCollection) {
        input.commands.insert(
            "mpv_vf_list".into(),
            run(&["mpv", "--vf=help"], FIXTURE_MPV, SYSTEM_MPV_VF),
        );
        input.commands.insert(
            "ffmpeg_filters_list".into(),
            run(
                &["ffmpeg", "-filters"],
                FIXTURE_FFMPEG,
                SYSTEM_FFMPEG_FILTERS,
            ),
        );
        input.commands.insert(
            "ffmpeg_formats_devices_list".into(),
            run(
                &["ffmpeg", "-formats"],
                FIXTURE_FFMPEG,
                SYSTEM_FFMPEG_FORMATS,
            ),
        );
        input.commands.insert(
            "ffmpeg_pix_fmts_list".into(),
            run(
                &["ffmpeg", "-pix_fmts"],
                FIXTURE_FFMPEG,
                SYSTEM_FFMPEG_PIXELS,
            ),
        );
        input.commands.insert(
            "ffmpeg_decoders_list".into(),
            run(
                &["ffmpeg", "-decoders"],
                FIXTURE_FFMPEG,
                SYSTEM_FFMPEG_DECODERS,
            ),
        );
    }

    #[test]
    fn collector_values_qualify_all_five_operational_filters() {
        let mut input = probes();
        with_listings(&mut input);
        for (filter, id, value) in [
            (
                CatalogFilter::Format,
                "mpv_vf_native_format",
                "--vf=format=fmt=yuv420p",
            ),
            (
                CatalogFilter::Eq,
                "mpv_vf_lavfi_eq",
                "--vf=lavfi=[eq=contrast=1.06:saturation=1.04]",
            ),
            (
                CatalogFilter::Unsharp,
                "mpv_vf_lavfi_unsharp",
                "--vf=lavfi=[unsharp=5:5:0.6]",
            ),
            (
                CatalogFilter::Hqdn3d,
                "mpv_vf_lavfi_hqdn3d",
                "--vf=lavfi=[hqdn3d=1.5:1.5:6:6]",
            ),
            (
                CatalogFilter::Bwdif,
                "mpv_vf_lavfi_bwdif",
                "--vf=lavfi=[tinterlace=mode=interleave_top],lavfi=[bwdif=mode=send_field]",
            ),
        ] {
            input.commands.insert(
                id.into(),
                run(
                    &[
                        "mpv",
                        "--vo=null",
                        "--frames=90",
                        value,
                        "av://lavfi:testsrc2=size=320x240:rate=60:d=1",
                    ],
                    FIXTURE_MPV,
                    SYSTEM_MPV_FILTER_RUN,
                ),
            );
            assert_eq!(
                Capabilities::from_probes(&input)
                    .unwrap()
                    .require(FIXTURE_STACK, Capability::OperationalFilter(filter)),
                Ok(()),
                "{id}"
            );
        }
        {
            let entry = input.commands.get_mut("mpv_vf_lavfi_eq").unwrap();
            entry.argv.retain(|arg| !arg.starts_with("--vf="));
            entry.argv.push("--vf=lavfi=[unsharp=5:5:0.6]".into());
        }
        assert!(matches!(
            Capabilities::from_probes(&input).unwrap().require(
                FIXTURE_STACK,
                Capability::OperationalFilter(CatalogFilter::Eq)
            ),
            Err(CapabilityError::Parse { .. })
        ));
        {
            let entry = input.commands.get_mut("mpv_vf_lavfi_eq").unwrap();
            entry
                .argv
                .push("--vf=lavfi=[eq=contrast=1.06:saturation=1.04]".into());
            entry.exit_code = Some(1);
        }
        assert!(matches!(
            Capabilities::from_probes(&input).unwrap().require(
                FIXTURE_STACK,
                Capability::OperationalFilter(CatalogFilter::Eq)
            ),
            Err(CapabilityError::Unverified { .. })
        ));
    }

    // mpv v0.41: a `--vf-clr` or later `--vf` after the required filter
    // invalidates it, so the run must not qualify even when the VO still
    // reaches end of file with exit code 0.
    #[test]
    fn vf_clr_or_later_override_invalidates_required_filter() {
        for (id, mutation) in [
            ("--vf-clr", Some("--vf-clr")),
            ("later --vf", Some("--vf=lavfi=[unsharp=5:5:0.6]")),
            (
                "later --vf-add",
                Some("--vf-add=lavfi=[eq=contrast=1.06:saturation=1.04]"),
            ),
        ] {
            let mut input = probes();
            with_listings(&mut input);
            let mut argv = vec![
                "mpv",
                "--vo=null",
                "--frames=90",
                "--vf=lavfi=[eq=contrast=1.06:saturation=1.04]",
            ];
            if let Some(extra) = mutation {
                argv.push(extra);
            }
            argv.push("av://lavfi:testsrc2=size=320x240:rate=60:d=1");
            input.commands.insert(
                "mpv_vf_lavfi_eq".into(),
                run(&argv, FIXTURE_MPV, SYSTEM_MPV_FILTER_RUN),
            );
            assert!(
                matches!(
                    Capabilities::from_probes(&input).unwrap().require(
                        FIXTURE_STACK,
                        Capability::OperationalFilter(CatalogFilter::Eq)
                    ),
                    Err(CapabilityError::Parse { .. })
                ),
                "{id} must not qualify"
            );
        }
    }

    // Deliberately synthetic parser transcripts: no qualified pinned binary
    // plus live NV12/YUYV/MJPEG capture device was available to record these.
    // Shape follows FFmpeg's actual stderr / mpv's actual stdout; assertions
    // test parser boundaries, NOT successful capture on this host.
    const FIXTURE_V4L2_NV12: &str = "Input #0, video4linux2,v4l2, from '/dev/video9':\n  Stream #0:0: Video: rawvideo (NV12 / 0x3231564E), nv12, 2560x1440, 60 fps\nframe=    8 fps=8.0 q=-0.0 Lsize=N/A time=00:00:00.13 bitrate=N/A speed=1x\n";
    const FIXTURE_V4L2_YUYV: &str = "Input #0, video4linux2,v4l2, from '/dev/video9':\n  Stream #0:0: Video: rawvideo (YUYV / 0x56595559), yuyv422, 2560x1440, 60 fps\nframe=    8 fps=8.0 q=-0.0 Lsize=N/A time=00:00:00.13 bitrate=N/A speed=1x\n";
    const FIXTURE_V4L2_MJPEG: &str = "Input #0, video4linux2,v4l2, from '/dev/video9':\n  Stream #0:0: Video: mjpeg (Baseline), yuvj422p, 2560x1440, 60 fps\nframe=    8 fps=8.0 q=-0.0 Lsize=N/A time=00:00:00.13 bitrate=N/A speed=1x\n";
    const FIXTURE_MPV_LIVE: &str =
        "● Video  --vid=1  (rawvideo 2560x1440 60 fps)\nVO: [null] 2560x1440 nv12\n";

    #[test]
    fn collector_v4l2_commands_qualify_each_live_format_and_player() {
        let mut input = probes();
        with_listings(&mut input);
        for (format, id, name, output) in [
            (
                CaptureFormat::Nv12,
                "v4l2_open_nv12_ffmpeg",
                "nv12",
                FIXTURE_V4L2_NV12,
            ),
            (
                CaptureFormat::Yuyv422,
                "v4l2_open_yuyv_ffmpeg",
                "yuyv422",
                FIXTURE_V4L2_YUYV,
            ),
            (
                CaptureFormat::Mjpeg,
                "v4l2_open_mjpeg_ffmpeg",
                "mjpeg",
                FIXTURE_V4L2_MJPEG,
            ),
        ] {
            let mut entry = run(
                &[
                    "ffmpeg",
                    "-f",
                    "v4l2",
                    "-input_format",
                    name,
                    "-i",
                    "/dev/video9",
                    "-frames:v",
                    "8",
                ],
                FIXTURE_FFMPEG,
                "",
            );
            entry.stderr = output.as_bytes().to_vec();
            input.commands.insert(id.into(), entry);
            assert_eq!(
                Capabilities::from_probes(&input).unwrap().require(
                    FIXTURE_STACK,
                    Capability::LiveInput {
                        format,
                        player: Player::Ffmpeg
                    }
                ),
                Ok(()),
                "{id}"
            );
        }
        for (format, id, name) in [
            (
                CaptureFormat::Nv12,
                "v4l2_open_nv12_mpv",
                "input_format=nv12,video_size=2560x1440,framerate=60",
            ),
            (
                CaptureFormat::Yuyv422,
                "v4l2_open_yuyv_mpv",
                "input_format=yuyv422,video_size=2560x1440,framerate=60",
            ),
        ] {
            let entry = run(
                &[
                    "mpv",
                    "--demuxer-lavf-format=v4l2",
                    &format!("--demuxer-lavf-o={name}"),
                    "--frames=30",
                    "av://v4l2:/dev/video9",
                ],
                FIXTURE_MPV,
                FIXTURE_MPV_LIVE,
            );
            input.commands.insert(id.into(), entry);
            assert_eq!(
                Capabilities::from_probes(&input).unwrap().require(
                    FIXTURE_STACK,
                    Capability::LiveInput {
                        format,
                        player: Player::Mpv
                    }
                ),
                Ok(()),
                "{id}"
            );
        }
        input.commands.get_mut("v4l2_open_nv12_mpv").unwrap().argv[2] =
            "--demuxer-lavf-o=input_format=nv12:video_size=2560x1440:framerate=60".into();
        assert!(matches!(
            Capabilities::from_probes(&input).unwrap().require(
                FIXTURE_STACK,
                Capability::LiveInput {
                    format: CaptureFormat::Nv12,
                    player: Player::Mpv
                }
            ),
            Err(CapabilityError::Parse { .. })
        ));
        input
            .commands
            .get_mut("v4l2_open_nv12_ffmpeg")
            .unwrap()
            .argv[2] = "pulse".into();
        assert!(matches!(
            Capabilities::from_probes(&input).unwrap().require(
                FIXTURE_STACK,
                Capability::LiveInput {
                    format: CaptureFormat::Nv12,
                    player: Player::Ffmpeg
                }
            ),
            Err(CapabilityError::Parse { .. })
        ));
        {
            let entry = input.commands.get_mut("v4l2_open_nv12_ffmpeg").unwrap();
            entry.argv[2] = "v4l2".into();
            entry.stderr = b"Input #0, video4linux2,v4l2, from '/dev/video9':\nframe= 0\n".to_vec();
        }
        assert!(matches!(
            Capabilities::from_probes(&input).unwrap().require(
                FIXTURE_STACK,
                Capability::LiveInput {
                    format: CaptureFormat::Nv12,
                    player: Player::Ffmpeg
                }
            ),
            Err(CapabilityError::Unverified { .. })
        ));
        assert!(matches!(
            Capabilities::from_probes(&input).unwrap().require(
                FIXTURE_STACK,
                Capability::LiveInput {
                    format: CaptureFormat::Mjpeg,
                    player: Player::Mpv
                }
            ),
            Err(CapabilityError::Unverified { .. })
        ));
    }

    // Synthetic wrapper stdout: no pinned stack or live sink was exercised.
    const FIXTURE_SINK_WRAPPER: &str = r#""$1" -hide_banner -v info -f lavfi -i "sine=frequency=440:duration=2" -f pulse -device "$2" "furami-probe-tone" & tone=$!; sleep 1; "$1" -hide_banner -v info -f pulse -i "$2.monitor" -t 2 -af volumedetect -f null - 2>&1; echo "--- monitor volumedetect rc=$? ---"; kill "$tone" 2>/dev/null; wait "$tone" 2>/dev/null; echo "--- tone rc=$? (143 = our TERM) ---"; exit 0"#;
    const FIXTURE_SINK_STDOUT: &str = "Input #0, pulse, from 'alsa_output.test.monitor':\n  Stream #0:0: Audio: pcm_s16le, 48000 Hz, stereo, s16\n[Parsed_volumedetect_0 @ 0x1] n_samples: 0\n[Parsed_volumedetect_0 @ 0x2] n_samples: 96000\n[Parsed_volumedetect_0 @ 0x2] mean_volume: -37.0 dB\n[Parsed_volumedetect_0 @ 0x2] max_volume: -31.0 dB\n--- monitor volumedetect rc=0 ---\n--- tone rc=0 (143 = our TERM) ---\n";

    #[test]
    fn sink_monitor_requires_requested_sink_signal_and_both_child_statuses() {
        let mut input = probes();
        let mut entry = run(
            &[
                "sh",
                "-c",
                FIXTURE_SINK_WRAPPER,
                "sh",
                "/usr/bin/ffmpeg",
                "alsa_output.test",
            ],
            "shell-digest",
            FIXTURE_SINK_STDOUT,
        );
        entry.stderr = b"tone player diagnostics, not monitor volumedetect\n".to_vec();
        input
            .commands
            .insert("pulse_sink_tone_monitor".into(), entry);
        assert_eq!(
            Capabilities::from_probes(&input)
                .unwrap()
                .require(FIXTURE_STACK, Capability::PulseSinkMonitor),
            Ok(())
        );
        for output in [
            FIXTURE_SINK_STDOUT.replace("monitor volumedetect rc=0", "monitor volumedetect rc=1"),
            FIXTURE_SINK_STDOUT.replace("tone rc=0", "tone rc=1"),
            FIXTURE_SINK_STDOUT.replace("n_samples: 96000", "n_samples: 0"),
            FIXTURE_SINK_STDOUT.replace("max_volume: -31.0 dB", "max_volume: -91.0 dB"),
            FIXTURE_SINK_STDOUT.replace("alsa_output.test.monitor", "another_sink.monitor"),
        ] {
            input
                .commands
                .get_mut("pulse_sink_tone_monitor")
                .unwrap()
                .stdout = output.into_bytes();
            assert!(matches!(
                Capabilities::from_probes(&input)
                    .unwrap()
                    .require(FIXTURE_STACK, Capability::PulseSinkMonitor),
                Err(CapabilityError::Unverified { .. })
            ));
        }
        {
            let entry = input.commands.get_mut("pulse_sink_tone_monitor").unwrap();
            entry.stdout.clear();
            entry.stderr = FIXTURE_SINK_STDOUT.as_bytes().to_vec();
        }
        assert!(matches!(
            Capabilities::from_probes(&input)
                .unwrap()
                .require(FIXTURE_STACK, Capability::PulseSinkMonitor),
            Err(CapabilityError::Unverified { .. })
        ));
        {
            let entry = input.commands.get_mut("pulse_sink_tone_monitor").unwrap();
            entry.stdout = FIXTURE_SINK_STDOUT.as_bytes().to_vec();
            entry.argv[2] = FIXTURE_SINK_WRAPPER.replace("-device \"$2\"", "-device \"$3\"");
        }
        assert!(matches!(
            Capabilities::from_probes(&input)
                .unwrap()
                .require(FIXTURE_STACK, Capability::PulseSinkMonitor),
            Err(CapabilityError::Unverified { .. })
        ));
    }

    // Device-properties excerpt copied from host mpv x11vk/libplacebo output,
    // paired here with synthetic VO/EOF lines to test only parser decisions.
    // These rows do not verify pinned build or physical hardware for release.
    const FIXTURE_VULKAN_STDOUT: &str = "[vo/gpu-next/vulkan] Initializing GPU context 'x11vk'\n[vo/gpu-next/libplacebo] Probing for vulkan devices:\n[vo/gpu-next/libplacebo]     GPU 0: AMD Radeon RX 7900 XTX (RADV NAVI31) v1.4.354 (discrete)\n[vo/gpu-next/libplacebo]     GPU 1: llvmpipe (LLVM 22.1.8, 256 bits) v1.4.354 (software)\n[vo/gpu-next/libplacebo] Vulkan device properties:\n[vo/gpu-next/libplacebo]     Device Name: AMD Radeon RX 7900 XTX (RADV NAVI31)\n[vo/gpu-next/libplacebo]     Driver name: radv\n[vo/gpu-next/libplacebo] Creating vulkan device with extensions:\nVO: [gpu-next] 1280x720 yuv420p\nExiting... (End of file)\n";

    fn renderer_input() -> ProbeCollection {
        let mut input = probes();
        input.commands.insert(
            "mpv_vo_list".into(),
            run(&["mpv", "--vo=help"], FIXTURE_MPV, SYSTEM_MPV_VO),
        );
        input.commands.insert(
            "mpv_gpu_context_list".into(),
            run(
                &["mpv", "--gpu-context=help"],
                FIXTURE_MPV,
                "Available GPU contexts:\n  x11vk            X11 Vulkan context\n",
            ),
        );
        input.commands.insert(
            "mpv_render_gpu_next_x11vk".into(),
            run(
                &[
                    "mpv",
                    "--vo=gpu-next",
                    "--gpu-context=x11vk",
                    "--gpu-api=vulkan",
                    "--scale=ewa_lanczossharp",
                    "--cscale=ewa_lanczossharp",
                    "--dscale=mitchell",
                    "--scale-antiring=0.7",
                    "--dither=error-diffusion",
                    "--deband=yes",
                    "--deband-iterations=2",
                    "--deband-grain=16",
                    "--frames=90",
                    "av://lavfi:testsrc2=size=1280x720:rate=30:d=2",
                ],
                FIXTURE_MPV,
                FIXTURE_VULKAN_STDOUT,
            ),
        );
        input
    }

    #[test]
    fn renderer_values_match_effective_arguments_and_selected_physical_device() {
        let mut input = renderer_input();
        let facts = Capabilities::from_probes(&input).unwrap();
        assert_eq!(
            facts.require(FIXTURE_STACK, Capability::VulkanRender),
            Ok(())
        );
        for (option, value) in [
            (RendererOption::Scale, "ewa_lanczossharp"),
            (RendererOption::Cscale, "ewa_lanczossharp"),
            (RendererOption::Dscale, "mitchell"),
            (RendererOption::ScaleAntiring, "0.7"),
            (RendererOption::Dither, "error-diffusion"),
            (RendererOption::Deband, "yes"),
            (RendererOption::DebandIterations, "2"),
            (RendererOption::DebandGrain, "16"),
        ] {
            assert_eq!(
                facts.require(FIXTURE_STACK, Capability::RendererValue(option, value)),
                Ok(())
            );
        }
        input
            .commands
            .get_mut("mpv_render_gpu_next_x11vk")
            .unwrap()
            .argv
            .push("--scale=bilinear".into());
        assert!(matches!(
            Capabilities::from_probes(&input).unwrap().require(
                FIXTURE_STACK,
                Capability::RendererValue(RendererOption::Scale, "ewa_lanczossharp")
            ),
            Err(CapabilityError::Unverified { .. })
        ));
        input
            .commands
            .get_mut("mpv_render_gpu_next_x11vk")
            .unwrap()
            .argv
            .pop();
        for output in [
            FIXTURE_VULKAN_STDOUT.replace(
                "[vo/gpu-next/libplacebo] Vulkan device properties:",
                "Physical GPU was not selected:",
            ),
            FIXTURE_VULKAN_STDOUT.replace(
                "Device Name: AMD Radeon RX 7900 XTX (RADV NAVI31)",
                "Device Name: llvmpipe (LLVM 22.1.8, 256 bits)",
            ),
            FIXTURE_VULKAN_STDOUT.replace(
                "[vo/gpu-next/libplacebo] Creating vulkan device with extensions:",
                "device creation not recorded:",
            ),
            FIXTURE_VULKAN_STDOUT.replace(
                "[vo/gpu-next/vulkan] Initializing GPU context 'x11vk'",
                "GPU context not recorded",
            ),
        ] {
            input
                .commands
                .get_mut("mpv_render_gpu_next_x11vk")
                .unwrap()
                .stdout = output.into_bytes();
            assert!(matches!(
                Capabilities::from_probes(&input)
                    .unwrap()
                    .require(FIXTURE_STACK, Capability::VulkanRender),
                Err(CapabilityError::Unverified { .. })
            ));
        }
    }

    #[test]
    fn pulse_source_silence_is_valid_input_but_empty_capture_is_not() {
        let mut input = probes();
        with_listings(&mut input);
        let mut entry = run(
            &[
                "ffmpeg",
                "-f",
                "pulse",
                "-i",
                "alsa_input.test",
                "-af",
                "volumedetect",
            ],
            FIXTURE_FFMPEG,
            "",
        );
        // Synthetic pulse header; volumedetect lines copied from actual host
        // FFmpeg anullsrc -t 0.2 run (20,480 silent samples).
        entry.stderr = b"Input #0, pulse, from 'alsa_input.test':\n[Parsed_volumedetect_0] n_samples: 0\n[Parsed_volumedetect_0] n_samples: 20480\n[Parsed_volumedetect_0] mean_volume: -91.0 dB\n[Parsed_volumedetect_0] max_volume: -91.0 dB\n".to_vec();
        input.commands.insert("pulse_source_capture".into(), entry);
        assert_eq!(
            Capabilities::from_probes(&input)
                .unwrap()
                .require(FIXTURE_STACK, Capability::PulseSourceCapture),
            Ok(())
        );
        let entry = input.commands.get_mut("pulse_source_capture").unwrap();
        entry.stderr =
            b"Input #0, pulse, from 'alsa_input.test':\n[Parsed_volumedetect_0] n_samples: 0\n"
                .to_vec();
        assert!(matches!(
            Capabilities::from_probes(&input)
                .unwrap()
                .require(FIXTURE_STACK, Capability::PulseSourceCapture),
            Err(CapabilityError::Unverified { .. })
        ));
    }
    #[test]
    fn direct_pulse_playback_requires_live_source_monitor_and_identified_route() {
        const SOURCE: &str = "alsa_input.test";
        const SINK: &str = "alsa_output.test";
        const RELAY_SCRIPT: &str = "/probes/pulse-direct.sh";
        let output = "--- relay stream=furami-direct-source-to-sink-123 ---\n--- sinks ---\n42 alsa_output.test PipeWire\n--- sinks rc=0 ---\n--- sink-inputs ---\nSink Input #99\n    Sink: 42\n    Properties:\n        media.name = \"furami-direct-source-to-sink-123\"\n--- sink-inputs rc=0 ---\n--- pw-link ---\nfurami-direct-source-to-sink-123:output_FL\n  alsa_output.test:playback_FL\n--- pw-link rc=0 ---\n--- sink monitor ---\nInput #0, pulse, from 'alsa_output.test.monitor':\nn_samples: 20000\nmean_volume: -31.0 dB\nmax_volume: -22.0 dB\n--- monitor rc=0 ---\n--- relay transcript ---\nInput #0, pulse, from 'alsa_input.test':\nOutput #0, pulse, to 'furami-direct-source-to-sink-123':\nn_samples: 50000\nmean_volume: -38.0 dB\nmax_volume: -23.0 dB\n--- relay rc=0 ---\n";
        let output = format!(
            "{}--- ffmpeg sha256 ---\n{FIXTURE_FFMPEG}  /usr/bin/ffmpeg\n--- ffmpeg sha256 rc=0 ---\n",
            output
                .replace(
                    "  alsa_output.test:playback_FL",
                    "  |-> alsa_output.test:playback_FL"
                )
                .replace(
                    "n_samples: 50000",
                    "n_samples: 0\n[out#0/pulse] video:0KiB audio:1500KiB subtitle:0KiB"
                )
        );
        let mut input = probes();
        input.commands.insert(
            "pulse_direct_source_to_sink".into(),
            run(
                &[RELAY_SCRIPT, "/usr/bin/ffmpeg", SOURCE, SINK],
                FIXTURE_FFMPEG,
                &output,
            ),
        );
        let verify = |input: &ProbeCollection| {
            Capabilities::from_probes(input).unwrap().require(
                FIXTURE_STACK,
                Capability::PulseSourcePlayback {
                    source: SOURCE,
                    sink: SINK,
                },
            )
        };
        assert_eq!(verify(&input), Ok(()));
        assert!(matches!(
            Capabilities::from_probes(&input).unwrap().require(
                "wrong-stack",
                Capability::PulseSourcePlayback {
                    source: SOURCE,
                    sink: SINK
                }
            ),
            Err(CapabilityError::StackMismatch { .. })
        ));
        for changed in [
            output.replace("Sink: 42", "Sink: 7"),
            output.replace(
                "alsa_output.test:playback_FL",
                "easyeffects_sink:playback_FL",
            ),
            output.replace("max_volume: -23.0 dB", "max_volume: -91.0 dB"),
            output.replace("max_volume: -22.0 dB", "max_volume: -91.0 dB"),
            output.replace("--- relay rc=0 ---", "--- relay rc=1 ---"),
            output.replace("--- pw-link rc=0 ---", "--- pw-link rc=1 ---"),
            output.replace("audio:1500KiB", "audio:0KiB"),
            output.replace(FIXTURE_FFMPEG, FIXTURE_MPV),
        ] {
            input
                .commands
                .get_mut("pulse_direct_source_to_sink")
                .unwrap()
                .stdout = changed.into_bytes();
            assert!(verify(&input).is_err());
        }
        input
            .commands
            .get_mut("pulse_direct_source_to_sink")
            .unwrap()
            .stdout = output.into();
        assert!(
            Capabilities::from_probes(&input)
                .unwrap()
                .require(
                    FIXTURE_STACK,
                    Capability::PulseSourcePlayback {
                        source: SOURCE,
                        sink: "other_sink"
                    }
                )
                .is_err()
        );
        input
            .commands
            .get_mut("pulse_direct_source_to_sink")
            .unwrap()
            .argv[1] = "/usr/bin/other".into();
        assert!(verify(&input).is_err());
    }
    #[test]
    fn effective_output_accepts_own_sink_input_without_matching_pipewire_node_name() {
        // Selected verbatim rows from qualified-G-effective-output Pulse stdout.
        // PipeWire did not name the node after Pulse media.name, hence an empty
        // scoped pw-link view, but sink-input #4414 identifies the actual route.
        const SOURCE: &str = "alsa_input.usb-GENKI_ShadowCast_3_KT044001-02.analog-stereo";
        const SINK: &str = "easyeffects_sink";
        const FFMPEG: &str = "/home/anthodev/.cache/furami/FUR-002/build-G/prefix/bin/ffmpeg";
        const SHA: &str = "1c38d09555005425ea30f80e03bfb45c4a65a3880013ed72d56658e2d416744e";
        let stdout = concat!(
            "--- relay stream=furami-direct-source-to-sink-1572551 ---\n",
            "--- sinks ---\n121\teasyeffects_sink\tPipeWire\tfloat32le 2ch 48000Hz\tRUNNING\n",
            "--- sinks rc=0 ---\n--- sink-inputs ---\nSink Input #4414\n",
            "\tSink: 121\n        media.name = \"furami-direct-source-to-sink-1572551\"\n",
            "--- sink-inputs rc=0 ---\n--- pw-link ---\n--- pw-link rc=0 ---\n",
            "--- sink monitor ---\nInput #0, pulse, from 'easyeffects_sink.monitor':\n",
            "[Parsed_volumedetect_0 @ 0x7ff1f4002100] n_samples: 196800\n",
            "[Parsed_volumedetect_0 @ 0x7ff1f4002100] mean_volume: -25.0 dB\n",
            "[Parsed_volumedetect_0 @ 0x7ff1f4002100] max_volume: -9.6 dB\n",
            "--- monitor rc=0 ---\n--- relay transcript ---\n",
            "Input #0, pulse, from 'alsa_input.usb-GENKI_ShadowCast_3_KT044001-02.analog-stereo':\n",
            "Output #0, pulse, to 'furami-direct-source-to-sink-1572551':\n",
            "[Parsed_volumedetect_0 @ 0x562d7e46a180] n_samples: 0\n",
            "[Parsed_volumedetect_0 @ 0x7f79fc002100] mean_volume: -37.9 dB\n",
            "[Parsed_volumedetect_0 @ 0x7f79fc002100] max_volume: -23.6 dB\n",
            "[out#0/pulse @ 0x562d7e468c00] video:0KiB audio:1500KiB subtitle:0KiB other streams:0KiB global headers:0KiB muxing overhead: unknown\n",
            "--- relay rc=0 ---\n--- ffmpeg sha256 ---\n",
            "1c38d09555005425ea30f80e03bfb45c4a65a3880013ed72d56658e2d416744e  /home/anthodev/.cache/furami/FUR-002/build-G/prefix/bin/ffmpeg\n",
            "--- ffmpeg sha256 rc=0 ---\n",
        );
        let mut input = probes();
        input.ffmpeg_path = FFMPEG.into();
        input.ffmpeg_sha256 = SHA.into();
        input.commands.insert(
            "pulse_direct_source_to_sink".into(),
            run(
                &["/probes/pulse-direct.sh", FFMPEG, SOURCE, SINK],
                FIXTURE_MPV,
                stdout,
            ),
        );
        let verify = |input: &ProbeCollection| {
            Capabilities::from_probes(input).unwrap().require(
                FIXTURE_STACK,
                Capability::PulseSourcePlayback {
                    source: SOURCE,
                    sink: SINK,
                },
            )
        };
        assert_eq!(verify(&input), Ok(()));
        for changed in [
            stdout.replace("\tSink: 121", "\tSink: 81"),
            stdout.replace(
                "media.name = \"furami-direct-source-to-sink-1572551\"",
                "media.name = \"unrelated-audio\"",
            ),
            stdout.replace("max_volume: -23.6 dB", "max_volume: -91.0 dB"),
            stdout.replace("--- relay rc=0 ---", "--- relay rc=1 ---"),
        ] {
            input
                .commands
                .get_mut("pulse_direct_source_to_sink")
                .unwrap()
                .stdout = changed.into_bytes();
            assert!(verify(&input).is_err());
        }
    }
}
