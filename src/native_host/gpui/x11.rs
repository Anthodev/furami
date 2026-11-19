//! Responsibility: Checked X11 requests for the GPUI native host.
//! Allowed dependencies: x11rb only.
//!
//! The adapter owns a second connection to the same X server as GPUI. It
//! validates the GPUI client window and parents exactly one borderless video
//! child under it; every mutation is checked so the host can tell a server
//! rejection from a transport failure that leaves the outcome uncertain.

use std::cell::Cell;

use x11rb::{
    COPY_DEPTH_FROM_PARENT, COPY_FROM_PARENT,
    connection::Connection as _,
    errors::{ConnectError, ConnectionError, ReplyError, ReplyOrIdError},
    properties::WmSizeHints,
    protocol::{
        ErrorKind, Event as XEvent,
        shape::{ConnectionExt as _, SK, SO},
        xproto::{
            ChangeWindowAttributesAux, ClipOrdering, ConfigureWindowAux, ConnectionExt as _,
            CreateWindowAux, EventMask, MapState, Rectangle, WindowClass,
        },
    },
    reexports::x11rb_protocol::parse_display::parse_display,
    rust_connection::RustConnection,
};

/// A native window that the checked destroy request proved gone.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum DestroyOutcome {
    /// The server destroyed the window for us.
    Destroyed,
    /// The window was already gone: `BadWindow` or an observed `DestroyNotify`.
    Absent,
}

/// One relevant native event drained from the parent/child pair.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Event {
    /// The GPUI parent or the video child changed rectangle.
    GeometryChanged,
    /// The GPUI client window was destroyed.
    ParentDestroyed,
    /// The video child was destroyed by someone else.
    ChildDestroyed,
    /// The video child now has a parent other than the bound GPUI client.
    ChildReparented,
}

/// A failed checked X11 request.
#[derive(Debug)]
pub(super) enum Error {
    /// The display could not be opened at all.
    Connect(ConnectError),
    /// The server does not provide the X Shape extension.
    ShapeUnsupported,
    /// The server's Shape extension predates input shapes.
    ShapeTooOld { major: u16, minor: u16 },
    /// The request was refused before it reached the server.
    State(&'static str),
    /// A geometry value could not be represented as an X11 request.
    Geometry(&'static str),
    /// The server has no free window identifiers for this connection.
    IdsExhausted,
    /// The connection or request transport failed. The request may have been
    /// executed by the server even though no answer arrived.
    Transport(ConnectionError),
    /// The server answered with an X error; the request had no effect.
    Rejected(x11rb::x11_utils::X11Error),
    /// `create` allocated the window but failed to finish its native setup.
    /// The XID exists and must still be released.
    Incomplete(x11rb::x11_utils::X11Error),
}

impl Error {
    /// Whether the X11 connection itself was lost, as opposed to an unsupported
    /// precondition, a server rejection or a usable-but-refused request.
    pub(super) fn is_connection_loss(&self) -> bool {
        matches!(self, Self::Transport(ConnectionError::IoError(_)))
    }

    /// Whether the server definitively rejected the request.
    ///
    /// For [`X11::create`], a rejection proves the requested XID was never
    /// allocated; any other failure (notably a transport error) leaves creation
    /// uncertain, so the host must retain the XID for the release path.
    pub(super) fn is_rejected(&self) -> bool {
        matches!(self, Self::Rejected(_))
    }

    fn from_reply(error: ReplyError) -> Self {
        match error {
            ReplyError::ConnectionError(error) => Self::Transport(error),
            ReplyError::X11Error(error) => Self::Rejected(error),
        }
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Connect(error) => write!(formatter, "cannot open the X11 display: {error}"),
            Self::ShapeUnsupported => {
                formatter.write_str("the X server does not provide the X Shape extension")
            }
            Self::ShapeTooOld { major, minor } => write!(
                formatter,
                "the X server offers Shape {major}.{minor}; 1.1 is required for input shapes"
            ),
            Self::State(detail) | Self::Geometry(detail) => formatter.write_str(detail),
            Self::IdsExhausted => {
                formatter.write_str("the X server has no free window identifiers")
            }
            Self::Transport(error) => write!(formatter, "the X11 connection failed: {error}"),
            Self::Rejected(error) => {
                write!(formatter, "the X server rejected a request: {error:?}")
            }
            Self::Incomplete(error) => write!(
                formatter,
                "the native window was created but its input shape failed: {error:?}"
            ),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Connect(error) => Some(error),
            Self::Transport(error) => Some(error),
            _ => None,
        }
    }
}

/// The X11 keysym for F11.
const F11_KEYSYM: u32 = 0xffc8;

/// The keycodes whose mapping contains `keysym`.
///
/// `keysyms` is the flat core mapping reply: `keysyms_per_keycode` entries per
/// keycode starting at `min_keycode`, column-major.
fn keysym_keycodes(
    keysyms: &[u32],
    keysyms_per_keycode: u8,
    min_keycode: u8,
    keysym: u32,
) -> Vec<u8> {
    let per_keycode = usize::from(keysyms_per_keycode);
    if per_keycode == 0 {
        return Vec::new();
    }
    keysyms
        .chunks_exact(per_keycode)
        .enumerate()
        .filter(|(_, slots)| slots.contains(&keysym))
        .filter_map(|(index, _)| u8::try_from(usize::from(min_keycode) + index).ok())
        .collect()
}

/// Whether the core keyboard bitmap reports `keycode` as down.
fn keymap_down(keys: &[u8; 32], keycode: u8) -> bool {
    let index = usize::from(keycode);
    keys.get(index / 8)
        .is_some_and(|byte| byte & (1 << (index % 8)) != 0)
}

/// The opaque over-video control, in physical device pixels.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct Rect {
    /// Left edge, inclusive.
    pub(super) left: u16,
    /// Top edge, inclusive.
    pub(super) top: u16,
    /// Right edge, exclusive.
    pub(super) right: u16,
    /// Bottom edge, exclusive.
    pub(super) bottom: u16,
}

impl Rect {
    /// The rectangle height in device pixels.
    pub(super) fn height(self) -> u16 {
        self.bottom - self.top
    }
}

/// The control size in logical pixels.
const BUTTON_WIDTH: f64 = 160.0;
/// The control height in logical pixels.
const BUTTON_HEIGHT: f64 = 48.0;
/// The control inset from the client right edge in logical pixels.
const BUTTON_RIGHT_INSET: f64 = 16.0;

/// One committed native layout: the child extent and the control cutout.
///
/// Both are snapped to physical device pixels so the opaque GPUI paint and the
/// X11 bounding shape name the same rectangle. The control rectangle is always
/// nonempty and lies inside `extent`; a viewport too small to place it keeps a
/// minimal device-pixel rectangle rather than an unrepresentable one.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Layout {
    /// The video client extent in device pixels.
    pub(super) extent: (u16, u16),
    /// The control rectangle to cut out, always inside `extent`.
    pub(super) button: Rect,
}

impl Layout {
    /// Snap the layout produced by an observed GPUI scale to device pixels.
    ///
    /// Derived from the actual client extent, never from a separate viewport:
    /// the left/top edges round down and the right/bottom edges round up so the
    /// cutout covers every pixel the control paints. A zero extent has no
    /// representable geometry and yields `Ok(None)` without any server request;
    /// extents and scales outside the checked X11 request range are rejected.
    pub(super) fn new(extent: (u16, u16), scale: f32) -> Result<Option<Self>, Error> {
        let (width, height) = extent;
        if width == 0 || height == 0 {
            return Ok(None);
        }
        let coordinate_limit = i16::MAX as u16;
        if width > coordinate_limit || height > coordinate_limit {
            return Err(Error::Geometry(
                "the video extent exceeds the X11 coordinate range",
            ));
        }
        if !scale.is_finite() || scale <= 0.0 {
            return Err(Error::Geometry(
                "the GPUI scale factor must be finite and positive",
            ));
        }
        let scale = f64::from(scale);
        let top_edge = (f64::from(height) - BUTTON_HEIGHT * scale) / 2.0;
        let mut button = Rect {
            left: snap_down(
                f64::from(width) - (BUTTON_RIGHT_INSET + BUTTON_WIDTH) * scale,
                width,
            ),
            top: snap_down(top_edge, height),
            right: snap_up(f64::from(width) - BUTTON_RIGHT_INSET * scale, width),
            bottom: snap_up(top_edge + BUTTON_HEIGHT * scale, height),
        };
        // A viewport narrower or shorter than the control insets cannot place
        // the full rectangle; keep a representable pixel rather than none.
        button.left = button.left.min(width - 1);
        button.top = button.top.min(height - 1);
        if button.right <= button.left || button.bottom <= button.top {
            button.right = button.left + 1;
            button.bottom = button.top + 1;
        }
        Ok(Some(Self { extent, button }))
    }

    /// The nonempty bounding rectangles of the full extent minus the control.
    ///
    /// Descendants of the shaped child are clipped to this region, so the full
    /// extent must be covered exactly once around the cutout.
    fn bounding_rectangles(self) -> ([Rectangle; 4], usize) {
        let (width, height) = self.extent;
        let button = self.button;
        let mut rectangles = [Rectangle::default(); 4];
        let mut count = 0;
        let button_height = button.height();
        push_rectangle(&mut rectangles, &mut count, 0, 0, width, button.top);
        push_rectangle(
            &mut rectangles,
            &mut count,
            0,
            button.bottom,
            width,
            height - button.bottom,
        );
        push_rectangle(
            &mut rectangles,
            &mut count,
            0,
            button.top,
            button.left,
            button_height,
        );
        push_rectangle(
            &mut rectangles,
            &mut count,
            button.right,
            button.top,
            width - button.right,
            button_height,
        );
        (rectangles, count)
    }
}

fn check_display_address(display: &str) -> Result<(), Error> {
    let parsed =
        parse_display(Some(display)).map_err(|_| Error::State("invalid X11 display address"))?;
    // x11rb 0.13.2 adds the base TCP port with unchecked u16 arithmetic.
    // A local display with no protocol also constructs this fallback.
    if parsed.display > u16::MAX - 6000 && parsed.protocol.as_deref() != Some("unix") {
        return Err(Error::State(
            "X11 display number exceeds the TCP port range",
        ));
    }
    Ok(())
}

fn minimum_device_extent(scale: f32) -> Result<(i32, i32), Error> {
    if !scale.is_finite() || scale <= 0.0 || 540.0 * scale > f32::from(i16::MAX) {
        return Err(Error::Geometry(
            "invalid scale for the X11 client size hints",
        ));
    }
    Ok(((480.0 * scale).ceil() as i32, (540.0 * scale).ceil() as i32))
}

/// Round a device-pixel edge down and clamp it into the client extent.
fn snap_down(value: f64, limit: u16) -> u16 {
    value.floor().clamp(0.0, f64::from(limit)) as u16
}

/// Round a device-pixel edge up and clamp it into the client extent.
fn snap_up(value: f64, limit: u16) -> u16 {
    value.ceil().clamp(0.0, f64::from(limit)) as u16
}

/// Append one nonempty bounding rectangle.
fn push_rectangle(
    rectangles: &mut [Rectangle; 4],
    count: &mut usize,
    x: u16,
    y: u16,
    width: u16,
    height: u16,
) {
    if width == 0 || height == 0 {
        return;
    }
    rectangles[*count] = Rectangle {
        x: i16::try_from(x).expect("layout validation bounds coordinates"),
        y: i16::try_from(y).expect("layout validation bounds coordinates"),
        width,
        height,
    };
    *count += 1;
}

/// The checked X11 side of the GPUI native host.
///
/// `parent` stays `0` until [`X11::bind`] validated the GPUI client window on
/// this connection's server.
pub(super) struct X11 {
    connection: RustConnection,
    root: u32,
    parent: u32,
    parent_mapped: Cell<bool>,
    last_extent: Cell<Option<(u32, u16, u16)>>,
    last_layout: Cell<Option<(u32, Layout)>>,
    destroyed: Cell<Option<u32>>,
    min_keycode: u8,
    max_keycode: u8,
}

impl X11 {
    /// Open the captured display and prove that X Shape 1.1 is available.
    pub(super) fn connect(display: &str) -> Result<Self, Error> {
        check_display_address(display)?;
        let (connection, screen) = x11rb::connect(Some(display)).map_err(Error::Connect)?;
        let root = connection
            .setup()
            .roots
            .get(screen)
            .map(|screen| screen.root)
            .ok_or(Error::State("the X11 display has no usable screen"))?;
        let (min_keycode, max_keycode) = {
            let setup = connection.setup();
            (setup.min_keycode, setup.max_keycode)
        };
        let cookie = connection
            .shape_query_version()
            .map_err(|error| match error {
                ConnectionError::UnsupportedExtension => Error::ShapeUnsupported,
                error => Error::Transport(error),
            })?;
        let reply = cookie.reply().map_err(shape_preflight_error)?;
        if reply.major_version < 1 || (reply.major_version == 1 && reply.minor_version < 1) {
            return Err(Error::ShapeTooOld {
                major: reply.major_version,
                minor: reply.minor_version,
            });
        }
        Ok(Self {
            connection,
            root,
            parent: 0,
            parent_mapped: Cell::new(false),
            last_extent: Cell::new(None),
            last_layout: Cell::new(None),
            destroyed: Cell::new(None),
            min_keycode,
            max_keycode,
        })
    }

    /// Validate a GPUI client window and subscribe to its structure events.
    pub(super) fn bind(&mut self, parent: u32) -> Result<(), Error> {
        if self.parent != 0 {
            return Err(Error::State(
                "the X11 connection is already bound to a parent window",
            ));
        }
        if parent == 0 || parent == self.root {
            return Err(Error::State(
                "the GPUI window handle is not a usable X11 parent",
            ));
        }
        let tree = self
            .connection
            .query_tree(parent)
            .map_err(Error::Transport)?
            .reply()
            .map_err(Error::from_reply)?;
        if tree.root != self.root {
            return Err(Error::State(
                "the GPUI window belongs to another X11 server or screen",
            ));
        }
        // Select structure events first, then read the current map state: a
        // change after the reply is guaranteed to produce a notification,
        // whereas a change between a stale reply and the mask would be lost.
        // The device-pixel extent is re-read per layout; the attributes reply
        // also proves the XID is a live drawable on this connection's server.
        self.connection
            .change_window_attributes(
                parent,
                &ChangeWindowAttributesAux::default().event_mask(EventMask::STRUCTURE_NOTIFY),
            )
            .map_err(Error::Transport)?
            .check()
            .map_err(Error::from_reply)?;
        let attributes = self
            .connection
            .get_window_attributes(parent)
            .map_err(Error::Transport)?
            .reply()
            .map_err(Error::from_reply)?;
        self.parent_mapped
            .set(attributes.map_state != MapState::UNMAPPED);
        self.parent = parent;
        Ok(())
    }

    /// Whether the bound GPUI client window is currently mapped.
    ///
    /// Seeded from the checked bind-time attributes and kept current by the
    /// server's `MapNotify`/`UnmapNotify` notifications.
    pub(super) fn parent_mapped(&self) -> bool {
        self.parent_mapped.get()
    }

    /// The bound GPUI client XID, or `0` before [`X11::bind`].
    pub(super) fn parent(&self) -> u32 {
        self.parent
    }

    pub(super) fn set_minimum_scale(&self, scale: f32) -> Result<(), Error> {
        let parent = self.bound_parent()?;
        let minimum = minimum_device_extent(scale)?;
        // Pinned GPUI writes its logical minimum directly to WM_NORMAL_HINTS,
        // whose units are device pixels. Preserve its GPU texture-size cap.
        let mut hints = WmSizeHints::get_normal_hints(&self.connection, parent)
            .map_err(Error::Transport)?
            .reply()
            .map_err(Error::from_reply)?
            .ok_or(Error::State("missing GPUI window size hints"))?;
        if hints.min_size != Some(minimum) {
            hints.min_size = Some(minimum);
            hints
                .set_normal_hints(&self.connection, parent)
                .map_err(Error::Transport)?
                .check()
                .map_err(Error::from_reply)?;
        }
        Ok(())
    }

    /// Whether a keycode mapped to F11 is physically down right now.
    ///
    /// GPUI's key-up callback may be synthesized from an autorepeat release
    /// that is immediately followed by another press, so a key-up alone does
    /// not prove the physical key was released. Read the server's mapping
    /// and bitmap on key-up, never from a tick.
    pub(super) fn f11_down(&self) -> Result<bool, Error> {
        let keycodes = self.f11_keycodes()?;
        if keycodes.is_empty() {
            return Ok(false);
        }
        let keys = self
            .connection
            .query_keymap()
            .map_err(Error::Transport)?
            .reply()
            .map_err(Error::from_reply)?
            .keys;
        Ok(keycodes.iter().any(|&keycode| keymap_down(&keys, keycode)))
    }

    fn f11_keycodes(&self) -> Result<Vec<u8>, Error> {
        let count = self
            .max_keycode
            .saturating_sub(self.min_keycode)
            .saturating_add(1);
        let reply = self
            .connection
            .get_keyboard_mapping(self.min_keycode, count)
            .map_err(Error::Transport)?
            .reply()
            .map_err(Error::from_reply)?;
        Ok(keysym_keycodes(
            &reply.keysyms,
            reply.keysyms_per_keycode,
            self.min_keycode,
            F11_KEYSYM,
        ))
    }

    /// The current device-pixel extent of the bound GPUI client window.
    pub(super) fn extent(&self) -> Result<(u16, u16), Error> {
        let parent = self.bound_parent()?;
        let geometry = self
            .connection
            .get_geometry(parent)
            .map_err(Error::Transport)?
            .reply()
            .map_err(Error::from_reply)?;
        Ok((geometry.width, geometry.height))
    }

    /// Allocate a server-issued XID for a future native window.
    pub(super) fn allocate(&self) -> Result<u32, Error> {
        self.bound_parent()?;
        self.connection.generate_id().map_err(|error| match error {
            ReplyOrIdError::IdsExhausted => Error::IdsExhausted,
            ReplyOrIdError::ConnectionError(error) => Error::Transport(error),
            ReplyOrIdError::X11Error(error) => Error::Rejected(error),
        })
    }

    /// Create the borderless video child under the GPUI client window.
    ///
    /// The checked request is followed by an empty input shape so the child
    /// never takes pointer input. A transport failure after the create request
    /// may still have created the XID; see [`Error::is_rejected`].
    pub(super) fn create(&self, xid: u32, extent: (u16, u16)) -> Result<(), Error> {
        let parent = self.bound_parent()?;
        if xid == 0 || xid == parent || xid == self.root {
            return Err(Error::State(
                "refusing to create a native window with a reserved XID",
            ));
        }
        if self.destroyed.get() == Some(xid) {
            return Err(Error::State("refusing to recreate a retired XID"));
        }
        let (width, height) = extent;
        if width == 0 || height == 0 {
            return Err(Error::Geometry("the native video extent must be nonzero"));
        }
        let attributes = CreateWindowAux::new()
            .background_pixel(0u32)
            .border_pixel(0u32)
            .event_mask(EventMask::STRUCTURE_NOTIFY);
        match self.connection.create_window(
            COPY_DEPTH_FROM_PARENT,
            xid,
            parent,
            0,
            0,
            width,
            height,
            0,
            WindowClass::INPUT_OUTPUT,
            COPY_FROM_PARENT,
            &attributes,
        ) {
            Ok(cookie) => cookie.check().map_err(Error::from_reply)?,
            Err(error) => return Err(Error::Transport(error)),
        }
        // The XID exists from here on: report setup failures as incomplete
        // rather than as a rejected create.
        match self.connection.shape_rectangles(
            SO::SET,
            SK::INPUT,
            ClipOrdering::UNSORTED,
            xid,
            0,
            0,
            &[],
        ) {
            Ok(cookie) => match cookie.check() {
                Ok(()) => {}
                Err(ReplyError::X11Error(error)) => return Err(Error::Incomplete(error)),
                Err(ReplyError::ConnectionError(error)) => return Err(Error::Transport(error)),
            },
            Err(error) => return Err(Error::Transport(error)),
        }
        self.last_extent.set(Some((xid, width, height)));
        self.last_layout.set(None);
        Ok(())
    }

    pub(super) fn verify_parent(&self, xid: u32) -> Result<(), Error> {
        let parent = self.bound_parent()?;
        let tree = self
            .connection
            .query_tree(xid)
            .map_err(Error::Transport)?
            .reply()
            .map_err(Error::from_reply)?;
        if tree.root != self.root || tree.parent != parent {
            return Err(Error::State(
                "the video window is not a direct child of GPUI",
            ));
        }
        Ok(())
    }

    /// Apply a committed layout to the native child.
    ///
    /// A checked resize and the checked bounding shape go out together; the
    /// empty input shape set by [`X11::create`] is left untouched so pointer
    /// hit testing stays with the GPUI parent. Unchanged layouts and zero
    /// extents are skipped without a request.
    pub(super) fn apply_layout(&self, xid: u32, layout: Layout) -> Result<(), Error> {
        let parent = self.bound_parent()?;
        if xid == 0 || xid == parent || xid == self.root {
            return Err(Error::State("refusing to lay out a reserved XID"));
        }
        if self.destroyed.get() == Some(xid) {
            return Err(Error::State("refusing to lay out a retired XID"));
        }
        let (width, height) = layout.extent;
        if width == 0 || height == 0 {
            return Ok(());
        }
        if self.last_layout.get() == Some((xid, layout)) {
            return Ok(());
        }
        if self.last_extent.get() != Some((xid, width, height)) {
            let configure = ConfigureWindowAux::new()
                .width(u32::from(width))
                .height(u32::from(height));
            self.connection
                .configure_window(xid, &configure)
                .map_err(Error::Transport)?
                .check()
                .map_err(Error::from_reply)?;
            self.last_extent.set(Some((xid, width, height)));
        }
        let (rectangles, count) = layout.bounding_rectangles();
        self.connection
            .shape_rectangles(
                SO::SET,
                SK::BOUNDING,
                ClipOrdering::UNSORTED,
                xid,
                0,
                0,
                &rectangles[..count],
            )
            .map_err(Error::Transport)?
            .check()
            .map_err(Error::from_reply)?;
        self.last_layout.set(Some((xid, layout)));
        Ok(())
    }

    /// Map or unmap the native child through a checked request.
    pub(super) fn set_mapped(&self, xid: u32, mapped: bool) -> Result<(), Error> {
        let parent = self.bound_parent()?;
        if xid == 0 || xid == parent || xid == self.root {
            return Err(Error::State("refusing to map or unmap a reserved XID"));
        }
        let cookie = if mapped {
            self.connection.map_window(xid)
        } else {
            self.connection.unmap_window(xid)
        };
        cookie
            .map_err(Error::Transport)?
            .check()
            .map_err(Error::from_reply)?;
        Ok(())
    }

    /// Destroy the native child through a checked request.
    ///
    /// A recorded `DestroyNotify` or a `BadWindow` reply proves the window is
    /// externally absent; a generic connection failure does not.
    pub(super) fn destroy(&self, xid: u32) -> Result<DestroyOutcome, Error> {
        let parent = self.bound_parent()?;
        if xid == 0 {
            return Ok(DestroyOutcome::Absent);
        }
        if xid == parent || xid == self.root {
            return Err(Error::State("refusing to destroy the GPUI parent window"));
        }
        if self.destroyed.get() == Some(xid) {
            return Ok(DestroyOutcome::Absent);
        }
        if self
            .last_extent
            .get()
            .is_some_and(|(current, _, _)| current == xid)
        {
            self.last_extent.set(None);
        }
        if self
            .last_layout
            .get()
            .is_some_and(|(current, _)| current == xid)
        {
            self.last_layout.set(None);
        }
        match self
            .connection
            .destroy_window(xid)
            .map_err(Error::Transport)?
            .check()
        {
            Ok(()) => {
                self.destroyed.set(Some(xid));
                Ok(DestroyOutcome::Destroyed)
            }
            Err(ReplyError::X11Error(error)) if error.error_kind == ErrorKind::Window => {
                self.destroyed.set(Some(xid));
                Ok(DestroyOutcome::Absent)
            }
            Err(error) => Err(Error::from_reply(error)),
        }
    }

    /// Drain one relevant event for the bound parent and `child`.
    ///
    /// Notifications about retired or unrelated XIDs are discarded; `Ok(None)`
    /// means the queue is empty.
    pub(super) fn next_event(&self, child: Option<u32>) -> Result<Option<Event>, Error> {
        let parent = self.bound_parent()?;
        loop {
            let event = match self.connection.poll_for_event() {
                Ok(Some(event)) => event,
                Ok(None) => return Ok(None),
                Err(error) => return Err(Error::Transport(error)),
            };
            if let Some(event) = self.interpret(parent, child, event) {
                return Ok(Some(event));
            }
        }
    }

    fn bound_parent(&self) -> Result<u32, Error> {
        if self.parent == 0 {
            Err(Error::State(
                "the X11 connection is not bound to a GPUI parent window",
            ))
        } else {
            Ok(self.parent)
        }
    }

    fn interpret(&self, parent: u32, child: Option<u32>, event: XEvent) -> Option<Event> {
        let server_generated = event.server_generated();
        match event {
            XEvent::DestroyNotify(notify) if server_generated => {
                if notify.window == parent {
                    Some(Event::ParentDestroyed)
                } else if child == Some(notify.window) {
                    self.destroyed.set(child);
                    Some(Event::ChildDestroyed)
                } else {
                    None
                }
            }
            XEvent::ReparentNotify(notify) if server_generated => {
                // Only the child moving away from its bound parent is a loss;
                // a reparent onto it again is not.
                if child == Some(notify.window) && notify.parent != parent {
                    Some(Event::ChildReparented)
                } else {
                    None
                }
            }
            XEvent::MapNotify(notify) if server_generated && notify.window == parent => {
                self.parent_mapped.set(true);
                Some(Event::GeometryChanged)
            }
            XEvent::UnmapNotify(notify) if server_generated && notify.window == parent => {
                self.parent_mapped.set(false);
                Some(Event::GeometryChanged)
            }
            XEvent::ConfigureNotify(notify) => {
                // Only the GPUI client's rectangle can change the committed
                // layout. The child's own ConfigureNotify is an echo of our
                // checked resize and must not trigger a parent GetGeometry.
                if notify.window == parent {
                    Some(Event::GeometryChanged)
                } else {
                    None
                }
            }
            _ => None,
        }
    }
}

fn shape_preflight_error(error: ReplyError) -> Error {
    match error {
        ReplyError::ConnectionError(ConnectionError::UnsupportedExtension) => {
            Error::ShapeUnsupported
        }
        ReplyError::ConnectionError(error) => Error::Transport(error),
        ReplyError::X11Error(error) => Error::Rejected(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use x11rb::protocol::xproto::Rectangle;

    fn committed(extent: (u16, u16), scale: f32) -> Layout {
        Layout::new(extent, scale)
            .expect("valid layout arguments")
            .expect("nonzero extent")
    }

    fn as_tuple(rectangle: Rectangle) -> (i16, i16, u16, u16) {
        (rectangle.x, rectangle.y, rectangle.width, rectangle.height)
    }

    fn corners(rect: Rect) -> (u16, u16, u16, u16) {
        (rect.left, rect.top, rect.right, rect.bottom)
    }

    fn is_geometry_error(result: Result<Option<Layout>, Error>) -> bool {
        matches!(result, Err(Error::Geometry(_)))
    }

    #[test]
    fn gpui_host_minimum_uses_device_pixels() {
        assert_eq!(minimum_device_extent(1.5).unwrap(), (720, 810));
        assert_eq!(minimum_device_extent(1.333).unwrap(), (640, 720));
        assert!(minimum_device_extent(f32::NAN).is_err());
    }

    #[test]
    fn gpui_host_rejects_display_number_with_overflowing_port() {
        assert!(check_display_address(":59535").is_ok());
        assert!(check_display_address(":59536").is_err());
        assert!(check_display_address(":65533").is_err());
    }

    #[test]
    fn gpui_host_layout_scale_150_snaps_to_device_pixels() {
        let layout = committed((960, 540), 1.5);
        assert_eq!(layout.extent, (960, 540));
        assert_eq!(corners(layout.button), (696, 234, 936, 306));
        assert_eq!(layout.button.right - layout.button.left, 240);
        assert_eq!(layout.button.height(), 72);
    }

    #[test]
    fn gpui_host_layout_rounds_fractional_edges_outward() {
        let layout = committed((999, 541), 1.3);
        assert_eq!(layout.extent, (999, 541));
        assert_eq!(corners(layout.button), (770, 239, 979, 302));
    }

    #[test]
    fn gpui_host_layout_clamps_control_into_small_viewport() {
        let layout = committed((100, 40), 1.0);
        assert_eq!(corners(layout.button), (0, 0, 84, 40));
        let scaled = committed((200, 60), 2.0);
        assert_eq!(corners(scaled.button), (0, 0, 168, 60));
    }

    #[test]
    fn gpui_host_layout_keeps_minimal_control_when_insets_overflow() {
        let layout = committed((10, 10), 1.0);
        assert_eq!(layout.extent, (10, 10));
        assert_eq!(corners(layout.button), (0, 0, 1, 1));
        let (rectangles, count) = layout.bounding_rectangles();
        assert_eq!(count, 2);
        assert_eq!(as_tuple(rectangles[0]), (0, 1, 10, 9));
        assert_eq!(as_tuple(rectangles[1]), (1, 0, 9, 1));
    }

    #[test]
    fn gpui_host_layout_rejects_empty_and_oversized_geometry() {
        assert_eq!(Layout::new((0, 540), 1.0).unwrap(), None);
        assert_eq!(Layout::new((960, 0), 1.0).unwrap(), None);
        assert!(Layout::new((32767, 32767), 1.0).unwrap().is_some());
        assert!(is_geometry_error(Layout::new((32768, 540), 1.0)));
        assert!(is_geometry_error(Layout::new((960, 32768), 1.0)));
        assert!(is_geometry_error(Layout::new((960, 540), 0.0)));
        assert!(is_geometry_error(Layout::new((960, 540), -1.0)));
        assert!(is_geometry_error(Layout::new((960, 540), f32::NAN)));
        assert!(is_geometry_error(Layout::new((960, 540), f32::INFINITY)));
    }

    #[test]
    fn gpui_host_layout_shape_covers_extent_minus_control() {
        let layout = committed((960, 540), 1.5);
        let (rectangles, count) = layout.bounding_rectangles();
        assert_eq!(count, 4);
        assert_eq!(as_tuple(rectangles[0]), (0, 0, 960, 234));
        assert_eq!(as_tuple(rectangles[1]), (0, 306, 960, 234));
        assert_eq!(as_tuple(rectangles[2]), (0, 234, 696, 72));
        assert_eq!(as_tuple(rectangles[3]), (936, 234, 24, 72));
        let covered: u32 = rectangles[..count]
            .iter()
            .map(|rectangle| u32::from(rectangle.width) * u32::from(rectangle.height))
            .sum();
        let button = layout.button;
        assert_eq!(
            covered,
            u32::from(layout.extent.0) * u32::from(layout.extent.1)
                - u32::from(button.right - button.left) * u32::from(button.height())
        );
    }

    #[test]
    fn gpui_host_f11_keycodes_scan_every_mapping_slot() {
        let keysyms = [
            0xffc8, 0xffc8, 0, // keycode 10
            0, 0xffc8, 0, // keycode 11
            0x0020, 0, 0, // keycode 12
        ];
        assert_eq!(keysym_keycodes(&keysyms, 3, 10, F11_KEYSYM), vec![10, 11]);
        assert!(keysym_keycodes(&keysyms, 0, 10, F11_KEYSYM).is_empty());
        assert!(keysym_keycodes(&[0x0020, 0x00], 2, 10, F11_KEYSYM).is_empty());
    }

    #[test]
    fn gpui_host_f11_keymap_reads_core_bitmap_bits() {
        let mut keys = [0u8; 32];
        assert!(!keymap_down(&keys, 95));
        assert!(!keymap_down(&keys, 255));
        keys[95 / 8] |= 1 << (95 % 8);
        assert!(keymap_down(&keys, 95));
        assert!(!keymap_down(&keys, 94));
        let mut keys = [0u8; 32];
        keys[0] = 1 << 5;
        assert!(keymap_down(&keys, 5));
        assert!(!keymap_down(&keys, 13));
    }
}
