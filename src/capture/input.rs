//! Owned, explicit V4L2 request. Options request a tuple; they do not certify it.

use super::linux::{self, CaptureSnapshot, ValidatedCapture};
use crate::domain::capture::{
    CaptureBufferType, CaptureDataError, CaptureMode, CapturedFourCc, DeviceIdentity, FrameRate,
    FrameSize, ModeRequest,
};
use serde::Serialize;
use std::{
    ffi::{CStr, CString, OsString},
    fmt,
    path::{Path, PathBuf},
};

#[derive(Debug, thiserror::Error)]
pub enum CaptureArgumentError {
    #[error(
        "capture selection requires --capture-node, --capture-fourcc, --capture-size and --capture-rate together"
    )]
    IncompleteSelection,
    #[error("invalid capture argument: {0}")]
    InvalidArgument(String),
    #[error(transparent)]
    Data(#[from] CaptureDataError),
}

#[derive(Debug)]
pub struct CaptureArguments {
    pub node: PathBuf,
    pub mode: CaptureMode,
}
impl fmt::Display for CaptureArguments {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let bytes = self.mode.captured_fourcc.bytes();
        write!(
            f,
            "{} {} {}x{} {}/{}",
            self.node.display(),
            String::from_utf8_lossy(&bytes),
            self.mode.size.width(),
            self.mode.size.height(),
            self.mode.rate.numerator(),
            self.mode.rate.denominator()
        )
    }
}
impl CaptureArguments {
    pub fn parse(
        args: impl IntoIterator<Item = OsString>,
    ) -> Result<Option<Self>, CaptureArgumentError> {
        let mut args = args.into_iter();
        let (mut node, mut fourcc, mut size, mut rate) = (None, None, None, None);
        while let Some(flag) = args.next() {
            let flag = flag.to_str().ok_or_else(|| {
                CaptureArgumentError::InvalidArgument("flag must be UTF-8".into())
            })?;
            let slot = match flag {
                "--capture-node" => &mut node,
                "--capture-fourcc" => &mut fourcc,
                "--capture-size" => &mut size,
                "--capture-rate" => &mut rate,
                _ => {
                    return Err(CaptureArgumentError::InvalidArgument(format!(
                        "unknown option {flag:?}"
                    )));
                }
            };
            if slot.is_some() {
                return Err(CaptureArgumentError::InvalidArgument(format!(
                    "duplicate {flag}"
                )));
            }
            *slot = Some(args.next().ok_or_else(|| {
                CaptureArgumentError::InvalidArgument(format!("missing value for {flag}"))
            })?);
        }
        if node.is_none() && fourcc.is_none() && size.is_none() && rate.is_none() {
            return Ok(None);
        }
        let (Some(node), Some(fourcc), Some(size), Some(rate)) = (node, fourcc, size, rate) else {
            return Err(CaptureArgumentError::IncompleteSelection);
        };
        let text = |value: OsString, name: &str| {
            value
                .into_string()
                .map_err(|_| CaptureArgumentError::InvalidArgument(format!("{name} must be UTF-8")))
        };
        let fourcc = text(fourcc, "FourCC")?;
        let bytes: [u8; 4] = fourcc.as_bytes().try_into().map_err(|_| {
            CaptureArgumentError::InvalidArgument(
                "FourCC must contain exactly four bytes, e.g. NV12".into(),
            )
        })?;
        let size = text(size, "size")?;
        let (width, height) = size.split_once('x').ok_or_else(|| {
            CaptureArgumentError::InvalidArgument("size must be WIDTHxHEIGHT".into())
        })?;
        let dimension = |value: &str| {
            if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
                return Err(CaptureArgumentError::InvalidArgument(
                    "size must be positive unsigned WIDTHxHEIGHT".into(),
                ));
            }
            value.parse::<u32>().map_err(|_| {
                CaptureArgumentError::InvalidArgument("size dimension exceeds u32".into())
            })
        };
        Ok(Some(Self {
            node: node.into(),
            mode: CaptureMode {
                captured_fourcc: CapturedFourCc::from_bytes(bytes),
                size: FrameSize::new(dimension(width)?, dimension(height)?)?,
                rate: FrameRate::parse_exact(&text(rate, "rate")?)?,
            },
        }))
    }
}

#[derive(Debug, thiserror::Error)]
pub enum InputError {
    #[error("capture route must be selected explicitly and unambiguously")]
    RouteAmbiguous,
    #[error("selected capture node {0:?} has no proven route for this tuple")]
    RouteNotProven(PathBuf),
    #[error("captured FourCC {0:?} has no pinned lavf mapping")]
    UnmappedFourCc(CapturedFourCc),
    #[error("capture node path cannot be represented as a NUL-free UTF-8 mpv argument")]
    UnrepresentablePath,
    #[error("malformed or unrepresentable capture specification: {0}")]
    MalformedSpec(&'static str),
}

#[derive(Debug, thiserror::Error)]
pub enum SelectionError {
    #[error("selected node {0:?} is not an eligible discovered USB UVC capture node")]
    NodeNotFound(PathBuf),
    #[error("selected node {0:?} belongs to multiple discovered routes")]
    NodeAmbiguous(PathBuf),
    #[error(transparent)]
    Capture(#[from] linux::CaptureError),
    #[error(transparent)]
    Input(#[from] InputError),
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct RequestedFacts {
    pub identity: DeviceIdentity,
    pub node: PathBuf,
    pub buffer_type: CaptureBufferType,
    pub mode: CaptureMode,
}
impl fmt::Display for RequestedFacts {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let bytes = self.mode.captured_fourcc.bytes();
        write!(
            f,
            "{} {} {}x{} {}/{}",
            self.node.display(),
            String::from_utf8_lossy(&bytes),
            self.mode.size.width(),
            self.mode.size.height(),
            self.mode.rate.numerator(),
            self.mode.rate.denominator()
        )
    }
}

/// Startup resolves node to physical identity once. Every open resolves that
/// identity anew, and must still prove the explicitly selected node.
#[derive(Clone, Debug)]
pub struct CaptureSelection {
    request: ModeRequest,
    node: PathBuf,
}
impl CaptureSelection {
    pub fn from_snapshot(
        snapshot: &CaptureSnapshot,
        node: &Path,
        mode: CaptureMode,
    ) -> Result<Self, SelectionError> {
        node_text(node)?;
        let mut candidates = snapshot.devices().iter().flat_map(|device| {
            device
                .nodes()
                .iter()
                .filter(move |route| route.devnode() == node)
                .map(move |_| device.identity())
        });
        let identity = candidates
            .next()
            .ok_or_else(|| SelectionError::NodeNotFound(node.to_owned()))?;
        if candidates.next().is_some() {
            return Err(SelectionError::NodeAmbiguous(node.to_owned()));
        }
        Ok(Self {
            request: ModeRequest {
                identity: identity.clone(),
                mode,
            },
            node: node.to_owned(),
        })
    }

    pub fn requested(&self) -> RequestedFacts {
        RequestedFacts {
            identity: self.request.identity.clone(),
            node: self.node.clone(),
            buffer_type: CaptureBufferType::SinglePlanar,
            mode: self.request.mode,
        }
    }

    /// Pure snapshot validation is also the stateful-test boundary. No backend.
    pub fn validate_snapshot(
        &self,
        snapshot: &CaptureSnapshot,
    ) -> Result<InputSpec, SelectionError> {
        let validated = linux::validate(snapshot, &self.request)?;
        Ok(InputSpec::from_validated(&validated, Some(&self.node))?)
    }

    /// Run only on the media owner worker, before constructing the backend.
    pub(crate) fn revalidate(&self) -> Result<InputSpec, SelectionError> {
        let snapshot = linux::discover()?;
        let validated = linux::validate(&snapshot, &self.request)?;
        let input = InputSpec::from_validated(&validated, Some(&self.node))?;
        let route = validated
            .routes()
            .iter()
            .find(|route| {
                route.node().devnode() == self.node
                    && route.buffer_type() == input.requested.buffer_type
            })
            .ok_or_else(|| InputError::RouteNotProven(self.node.clone()))?;
        linux::revalidate_route(validated.identity(), route)?;
        Ok(input)
    }
}

#[derive(Debug)]
pub struct InputSpec {
    requested: RequestedFacts,
    url: CString,
    lavf_options: CString,
}
impl InputSpec {
    pub fn from_validated(
        validated: &ValidatedCapture<'_>,
        selected_node: Option<&Path>,
    ) -> Result<Self, InputError> {
        let selected_node = selected_node.ok_or(InputError::RouteAmbiguous)?;
        let mut routes = validated
            .routes()
            .iter()
            .filter(|route| route.node().devnode() == selected_node);
        let route = routes
            .next()
            .ok_or_else(|| InputError::RouteNotProven(selected_node.to_owned()))?;
        if routes.next().is_some() {
            return Err(InputError::RouteAmbiguous);
        }
        if route.buffer_type() != CaptureBufferType::SinglePlanar {
            return Err(InputError::MalformedSpec(
                "pinned lavf V4L2 input requires a single-planar capture queue",
            ));
        }
        let mode = validated.mode();
        let format = match mode.captured_fourcc.bytes() {
            [b'N', b'V', b'1', b'2'] => "nv12",
            [b'Y', b'U', b'Y', b'V'] => "yuyv422",
            [b'M', b'J', b'P', b'G'] => "mjpeg",
            [b'B', b'G', b'R', b'3'] => "bgr24",
            _ => return Err(InputError::UnmappedFourCc(mode.captured_fourcc)),
        };
        let lavf_options = CString::new(format!(
            "input_format={format},video_size={}x{},framerate={}/{}",
            mode.size.width(),
            mode.size.height(),
            mode.rate.numerator(),
            mode.rate.denominator(),
        ))
        .map_err(|_| InputError::MalformedSpec("NUL in generated lavf options"))?;
        Ok(Self {
            requested: RequestedFacts {
                identity: validated.identity().clone(),
                node: selected_node.to_owned(),
                buffer_type: route.buffer_type(),
                mode,
            },
            url: node_argument(selected_node)?,
            lavf_options,
        })
    }
    pub fn requested(&self) -> &RequestedFacts {
        &self.requested
    }
    pub fn node(&self) -> &Path {
        &self.requested.node
    }
    pub fn url(&self) -> &CStr {
        &self.url
    }
    pub fn lavf_options(&self) -> &CStr {
        &self.lavf_options
    }
}

fn node_text(node: &Path) -> Result<&str, InputError> {
    let value = node.to_str().ok_or(InputError::UnrepresentablePath)?;
    if value.contains('\0') {
        return Err(InputError::UnrepresentablePath);
    }
    Ok(value)
}
fn node_argument(node: &Path) -> Result<CString, InputError> {
    CString::new(format!("av://v4l2:{}", node_text(node)?))
        .map_err(|_| InputError::UnrepresentablePath)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        capture::linux,
        domain::capture::{CapturedFourCc, FrameRate, FrameSize},
    };
    use std::os::unix::ffi::OsStringExt;

    pub(crate) fn mode() -> CaptureMode {
        CaptureMode {
            captured_fourcc: CapturedFourCc::from_bytes(*b"NV12"),
            size: FrameSize::new(2560, 1440).unwrap(),
            rate: FrameRate::new(60, 1).unwrap(),
        }
    }

    #[test]
    fn explicit_route_preserves_exact_options_and_separate_node() {
        let mut mode = mode();
        mode.rate = FrameRate::new(60000, 1001).unwrap();
        let snapshot = linux::session_fixture(&["/dev/video,a=b:c"], mode);
        let selection =
            CaptureSelection::from_snapshot(&snapshot, Path::new("/dev/video,a=b:c"), mode)
                .unwrap();
        let input = selection.validate_snapshot(&snapshot).unwrap();
        assert_eq!(
            input.node().as_os_str().as_encoded_bytes(),
            b"/dev/video,a=b:c"
        );
        assert_eq!(input.url().to_bytes(), b"av://v4l2:/dev/video,a=b:c");
        assert_eq!(
            input.lavf_options().to_bytes(),
            b"input_format=nv12,video_size=2560x1440,framerate=60000/1001"
        );
        assert_eq!(input.requested().mode, mode);
        assert_eq!(input.requested().node, Path::new("/dev/video,a=b:c"));
    }

    #[test]
    fn explicitly_selected_second_route_is_not_replaced_by_first_route() {
        let mode = mode();
        let snapshot = linux::session_fixture(&["/dev/video-first", "/dev/video-selected"], mode);
        let selection =
            CaptureSelection::from_snapshot(&snapshot, Path::new("/dev/video-selected"), mode)
                .unwrap();
        let input = selection.validate_snapshot(&snapshot).unwrap();
        assert_eq!(input.node(), Path::new("/dev/video-selected"));
        assert_eq!(input.url().to_bytes(), b"av://v4l2:/dev/video-selected");
    }

    #[test]
    fn no_implicit_route_even_when_only_one_route_exists() {
        let mode = mode();
        let snapshot = linux::session_fixture(&["/dev/video0"], mode);
        let request = ModeRequest {
            identity: snapshot.devices()[0].identity().clone(),
            mode,
        };
        let validated = linux::validate(&snapshot, &request).unwrap();
        assert!(matches!(
            InputSpec::from_validated(&validated, None),
            Err(InputError::RouteAmbiguous)
        ));
    }

    #[test]
    fn all_pinned_fourcc_mappings_are_explicit_and_unknown_rejected() {
        for (fourcc, name) in [
            (b"NV12", "nv12"),
            (b"YUYV", "yuyv422"),
            (b"MJPG", "mjpeg"),
            (b"BGR3", "bgr24"),
        ] {
            let mode = CaptureMode {
                captured_fourcc: CapturedFourCc::from_bytes(*fourcc),
                ..mode()
            };
            let snapshot = linux::session_fixture(&["/dev/video0"], mode);
            let selection =
                CaptureSelection::from_snapshot(&snapshot, Path::new("/dev/video0"), mode).unwrap();
            assert!(
                selection
                    .validate_snapshot(&snapshot)
                    .unwrap()
                    .lavf_options()
                    .to_str()
                    .unwrap()
                    .starts_with(&format!("input_format={name},"))
            );
        }
        let mode = CaptureMode {
            captured_fourcc: CapturedFourCc::from_bytes(*b"ZZZZ"),
            ..mode()
        };
        let snapshot = linux::session_fixture(&["/dev/video0"], mode);
        let selection =
            CaptureSelection::from_snapshot(&snapshot, Path::new("/dev/video0"), mode).unwrap();
        assert!(matches!(
            selection.validate_snapshot(&snapshot),
            Err(SelectionError::Input(InputError::UnmappedFourCc(_)))
        ));
    }

    #[test]
    fn unrepresentable_and_nul_paths_are_rejected_not_punctuation() {
        assert!(matches!(
            node_argument(Path::new("/dev/video\0bad")),
            Err(InputError::UnrepresentablePath)
        ));
        let invalid = PathBuf::from(std::ffi::OsString::from_vec(vec![b'/', 0xff]));
        assert!(matches!(
            node_argument(&invalid),
            Err(InputError::UnrepresentablePath)
        ));
        assert!(node_argument(Path::new("/dev/a,b=c:d")).is_ok());
    }

    #[test]
    fn reopening_revalidates_identity_not_reused_node_name() {
        let mode = mode();
        let original = linux::session_fixture(&["/dev/video0"], mode);
        let selection =
            CaptureSelection::from_snapshot(&original, Path::new("/dev/video0"), mode).unwrap();
        let other = linux::session_fixture(&["/dev/video0"], mode);
        let changed = DeviceIdentity::new(
            0x1234,
            0x5678,
            other.devices()[0].identity().topology().clone(),
            Some("other".into()),
        )
        .unwrap();
        let request = CaptureSelection {
            request: ModeRequest {
                identity: changed,
                mode,
            },
            node: selection.node.clone(),
        };
        assert!(matches!(
            request.validate_snapshot(&original),
            Err(SelectionError::Capture(linux::CaptureError::Identity(_)))
        ));
        let moved = linux::session_fixture(&["/dev/video2"], mode);
        assert!(matches!(
            selection.validate_snapshot(&moved),
            Err(SelectionError::Input(InputError::RouteNotProven(_)))
        ));
    }

    #[test]
    fn startup_node_absence_and_duplicate_routes_are_typed() {
        let mode = mode();
        let snapshot = linux::session_fixture(&["/dev/video0"], mode);
        assert!(matches!(
            CaptureSelection::from_snapshot(&snapshot, Path::new("/dev/missing"), mode),
            Err(SelectionError::NodeNotFound(_))
        ));
        let duplicate = linux::session_fixture(&["/dev/video0", "/dev/video0"], mode);
        assert!(matches!(
            CaptureSelection::from_snapshot(&duplicate, Path::new("/dev/video0"), mode),
            Err(SelectionError::NodeAmbiguous(_))
        ));
    }
}
