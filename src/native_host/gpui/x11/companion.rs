//! Checked placement, input shape and focus for the GPUI-owned chrome XID.

use std::cell::RefCell;

use x11rb::{
    connection::Connection as _,
    cookie::VoidCookie,
    errors::ConnectionError,
    protocol::{
        Event,
        render::{ConnectionExt as _, PictType},
        shape::{ConnectionExt as _, SK, SO},
        xproto::{
            AtomEnum, ChangeWindowAttributesAux, ClipOrdering, ConfigureWindowAux,
            ConnectionExt as _, EventMask, GetPropertyReply, InputFocus, MapState, PropMode,
            Property, Rectangle, StackMode, WindowClass,
        },
    },
    rust_connection::RustConnection,
    wrapper::ConnectionExt as _,
};

use super::{Error, X11};

x11rb::atom_manager! {
    Atoms: AtomsCookie {
        _NET_WM_WINDOW_TYPE,
        _NET_WM_WINDOW_OPACITY,
        _NET_WM_PID,
        _NET_WM_STATE,
        _NET_WM_STATE_HIDDEN,
        _NET_WM_DESKTOP,
        _NET_CURRENT_DESKTOP,
        _NET_ACTIVE_WINDOW,
        _FURAMI_CHROME_FOCUS_TIME,
    }
}

pub(super) fn owned_events() -> EventMask {
    // Core keyboard events are shared; XI2 key selection would suppress
    // delivery to GPUI. GPUI alone owns XI2 pointer subscription and dispatch.
    EventMask::STRUCTURE_NOTIFY
        | EventMask::FOCUS_CHANGE
        | EventMask::KEY_PRESS
        | EventMask::KEY_RELEASE
        | EventMask::PROPERTY_CHANGE
}

/// Exact X-root position of the client's origin and its physical extent.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::native_host::gpui) struct CompanionGeometry {
    pub(in crate::native_host::gpui) x: i16,
    pub(in crate::native_host::gpui) y: i16,
    pub(in crate::native_host::gpui) width: u16,
    pub(in crate::native_host::gpui) height: u16,
}

impl CompanionGeometry {
    fn extent(self) -> (u16, u16) {
        (self.width, self.height)
    }
}

/// A committed hit rectangle in logical GPUI coordinates, relative to chrome.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(in crate::native_host::gpui) struct HitRect {
    pub(in crate::native_host::gpui) left: f32,
    pub(in crate::native_host::gpui) top: f32,
    pub(in crate::native_host::gpui) right: f32,
    pub(in crate::native_host::gpui) bottom: f32,
}

/// The two direct, application-owned keyboard focus targets.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::native_host::gpui) enum FocusTarget {
    Root,
    Companion,
}

/// Facts sampled from X11 after draining notifications, not GPUI occlusion.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::native_host::gpui) struct CompanionFacts {
    pub(in crate::native_host::gpui) root_viewable: bool,
    pub(in crate::native_host::gpui) root_mapped: bool,
    pub(in crate::native_host::gpui) companion_viewable: bool,
    pub(in crate::native_host::gpui) focus: Option<FocusTarget>,
}

#[derive(Default)]
struct Presentation {
    placed: Option<CompanionGeometry>,
    visible: bool,
    hits: Vec<Rectangle>,
    scratch: Vec<Rectangle>,
}

pub(super) struct Companion {
    xid: u32,
    atoms: Atoms,
    presentation: RefCell<Presentation>,
}

impl X11 {
    /// Quarantine GPUI's unconditionally mapped popup before any on-screen
    /// placement. This connection observes it but never owns or destroys it.
    pub(in crate::native_host::gpui) fn bind_companion(&mut self, xid: u32) -> Result<(), Error> {
        let parent = self.bound_parent()?;
        if self.companion.is_some()
            || self.reserved_xid(xid, parent)
            || self
                .last_extent
                .get()
                .is_some_and(|(child, _, _)| child == xid)
        {
            return Err(Error::State(
                "invalid or duplicate GPUI chrome companion XID",
            ));
        }
        let connection = &self.connection;
        let tree = connection.query_tree(xid).map_err(Error::Transport)?;
        let attributes = connection
            .get_window_attributes(xid)
            .map_err(Error::Transport)?;
        let geometry = connection.get_geometry(xid).map_err(Error::Transport)?;
        let atoms = Atoms::new(connection).map_err(Error::Transport)?;
        let tree = tree.reply().map_err(Error::from_reply)?;
        if tree.root != self.root || tree.parent != self.root {
            return Err(Error::State(
                "the GPUI companion is not a direct child of the X11 screen root",
            ));
        }
        let attributes = attributes.reply().map_err(Error::from_reply)?;
        if !attributes.override_redirect || attributes.class != WindowClass::INPUT_OUTPUT {
            return Err(Error::State(
                "the GPUI companion is not an override-redirect drawable",
            ));
        }
        let geometry = geometry.reply().map_err(Error::from_reply)?;
        // Once identity is verified, unmap even if the geometry or visual is
        // invalid. Never reposition an unsupported window into the viewport.
        check(connection.unmap_window(xid))?;
        if geometry.root != self.root
            || geometry.border_width != 0
            || !offscreen(geometry.x, geometry.y, geometry.width, geometry.height)
        {
            return Err(Error::Geometry(
                "GPUI initially placed its companion inside the X11 root",
            ));
        }
        if geometry.depth != 32 || !self.is_argb_visual(attributes.visual)? {
            return Err(Error::State("the GPUI companion has no ARGB32 visual"));
        }
        let atoms = atoms.reply().map_err(Error::from_reply)?;
        self.input_shape(xid, &[])?;
        self.set_companion_opacity(xid, &atoms, 0)?;
        check(connection.change_property32(
            PropMode::REPLACE,
            xid,
            AtomEnum::WM_TRANSIENT_FOR,
            AtomEnum::WINDOW,
            &[parent],
        ))?;
        // GPUI identifies PopUp as a notification. Retain only transient
        // identity; the managed client remains the one application window.
        check(connection.delete_property(xid, atoms._NET_WM_WINDOW_TYPE))?;
        check(connection.change_window_attributes(
            xid,
            &ChangeWindowAttributesAux::new().event_mask(owned_events()),
        ))?;
        check(connection.change_window_attributes(
            self.root,
            &ChangeWindowAttributesAux::new().event_mask(EventMask::PROPERTY_CHANGE),
        ))?;
        self.refresh_tab_keycodes()?;
        self.companion = Some(Companion {
            xid,
            atoms,
            presentation: RefCell::default(),
        });
        Ok(())
    }

    pub(in crate::native_host::gpui) fn companion_xid(&self) -> Option<u32> {
        self.companion.as_ref().map(|companion| companion.xid)
    }

    /// The GPUI connection, not this observer, owns chrome. After losing the
    /// observer, try only to disable input and unmap the same app-owned XID.
    pub(in crate::native_host::gpui) fn quarantine_companion_after_connection_loss(
        &self,
        display: &str,
    ) -> Result<(), Error> {
        let parent = self.bound_parent()?;
        let xid = self.bound_companion()?.xid;
        if display != self.display.as_ref() {
            return Err(Error::State(
                "refusing to quarantine chrome on a different X11 display",
            ));
        }
        let auxiliary = Self::connect(display)?;
        if auxiliary.root != self.root {
            return Err(Error::State("the reconnected X11 screen root changed"));
        }
        let connection = &auxiliary.connection;
        let atoms = Atoms::new(connection)
            .map_err(Error::Transport)?
            .reply()
            .map_err(Error::from_reply)?;
        let managed_tree = connection.query_tree(parent).map_err(Error::Transport)?;
        let chrome_tree = connection.query_tree(xid).map_err(Error::Transport)?;
        let managed = connection
            .get_window_attributes(parent)
            .map_err(Error::Transport)?;
        let chrome = connection
            .get_window_attributes(xid)
            .map_err(Error::Transport)?;
        let managed_pid = connection
            .get_property(false, parent, atoms._NET_WM_PID, AtomEnum::CARDINAL, 0, 1)
            .map_err(Error::Transport)?;
        let chrome_pid = connection
            .get_property(false, xid, atoms._NET_WM_PID, AtomEnum::CARDINAL, 0, 1)
            .map_err(Error::Transport)?;
        let transient = connection
            .get_property(
                false,
                xid,
                AtomEnum::WM_TRANSIENT_FOR,
                AtomEnum::WINDOW,
                0,
                1,
            )
            .map_err(Error::Transport)?;

        let managed_tree = managed_tree.reply().map_err(Error::from_reply)?;
        let chrome_tree = chrome_tree.reply().map_err(Error::from_reply)?;
        let managed = managed.reply().map_err(Error::from_reply)?;
        let chrome = chrome.reply().map_err(Error::from_reply)?;
        let managed_pid = managed_pid.reply().map_err(Error::from_reply)?;
        let chrome_pid = chrome_pid.reply().map_err(Error::from_reply)?;
        let transient = transient.reply().map_err(Error::from_reply)?;
        let pid = Some(std::process::id());
        if managed_tree.root != self.root
            || chrome_tree.root != self.root
            || chrome_tree.parent != self.root
            || managed.override_redirect
            || !chrome.override_redirect
            || managed.class != WindowClass::INPUT_OUTPUT
            || chrome.class != WindowClass::INPUT_OUTPUT
            || cardinal(&managed_pid) != pid
            || cardinal(&chrome_pid) != pid
            || single_property(&transient, AtomEnum::WINDOW) != Some(parent)
        {
            return Err(Error::State(
                "the reconnected XIDs are not Furami's GPUI chrome family",
            ));
        }

        let input = auxiliary.input_shape(xid, &[]);
        let unmap = check(connection.unmap_window(xid));
        input.and(unmap)
    }

    pub(super) fn is_companion(&self, xid: u32) -> bool {
        self.companion_xid() == Some(xid)
    }

    pub(super) fn focus_target(&self, xid: u32) -> Option<FocusTarget> {
        if self.is_companion(xid) {
            Some(FocusTarget::Companion)
        } else if xid != 0 && xid == self.parent {
            Some(FocusTarget::Root)
        } else {
            None
        }
    }

    pub(super) fn is_visibility_property(&self, xid: u32, atom: u32) -> bool {
        self.companion.as_ref().is_some_and(|companion| {
            let atoms = &companion.atoms;
            (xid == self.root
                && (atom == atoms._NET_CURRENT_DESKTOP || atom == atoms._NET_ACTIVE_WINDOW))
                || (xid == self.parent
                    && (atom == atoms._NET_WM_STATE || atom == atoms._NET_WM_DESKTOP))
        })
    }

    /// Derive device placement from the actual client, not the WM frame.
    pub(in crate::native_host::gpui) fn companion_geometry(
        &self,
    ) -> Result<CompanionGeometry, Error> {
        let parent = self.bound_parent()?;
        let geometry = self
            .connection
            .get_geometry(parent)
            .map_err(Error::Transport)?;
        let position = self
            .connection
            .translate_coordinates(parent, self.root, 0, 0)
            .map_err(Error::Transport)?;
        let geometry = geometry.reply().map_err(Error::from_reply)?;
        let position = position.reply().map_err(Error::from_reply)?;
        if geometry.root != self.root || !position.same_screen {
            return Err(Error::State(
                "the managed client is not on the companion's X11 screen",
            ));
        }
        if geometry.width == 0
            || geometry.height == 0
            || geometry.width > i16::MAX as u16
            || geometry.height > i16::MAX as u16
        {
            return Err(Error::Geometry("invalid X11 companion extent"));
        }
        Ok(CompanionGeometry {
            x: position.dst_x,
            y: position.dst_y,
            width: geometry.width,
            height: geometry.height,
        })
    }

    /// Move without changing focus, and restack only inside the active family.
    /// Return whether committed hits/paint were invalidated by an extent
    /// change or an unexpected external geometry change. A normal move and
    /// the configure notification echo from our own request preserve the frame.
    pub(in crate::native_host::gpui) fn place_companion(
        &self,
        geometry: CompanionGeometry,
    ) -> Result<bool, Error> {
        let companion = self.bound_companion()?;
        let actual = self.read_companion_geometry(companion)?;
        let mut presentation = companion.presentation.borrow_mut();
        let invalidated = placement_invalidated(presentation.placed, actual, geometry);
        if invalidated {
            self.conceal(companion, &mut presentation)?;
        }
        if actual != geometry {
            let mut configure = ConfigureWindowAux::new()
                .x(i32::from(geometry.x))
                .y(i32::from(geometry.y))
                .width(u32::from(geometry.width))
                .height(u32::from(geometry.height));
            if presentation.visible {
                let facts = self.companion_facts()?;
                if facts.root_viewable && facts.companion_viewable && facts.focus.is_some() {
                    configure = configure.stack_mode(StackMode::ABOVE);
                } else {
                    self.conceal(companion, &mut presentation)?;
                    check(self.connection.unmap_window(companion.xid))?;
                }
            }
            check(self.connection.configure_window(companion.xid, &configure))?;
        }
        presentation.placed = Some(geometry);
        Ok(invalidated)
    }

    /// Root availability includes minimize and an EWMH workspace mismatch.
    /// Missing workspace properties are valid without an EWMH window manager.
    pub(in crate::native_host::gpui) fn companion_facts(&self) -> Result<CompanionFacts, Error> {
        let companion = self.bound_companion()?;
        let connection = &self.connection;
        let atoms = &companion.atoms;
        let managed = connection
            .get_window_attributes(self.parent)
            .map_err(Error::Transport)?;
        let chrome = connection
            .get_window_attributes(companion.xid)
            .map_err(Error::Transport)?;
        let state = connection
            .get_property(
                false,
                self.parent,
                atoms._NET_WM_STATE,
                AtomEnum::ATOM,
                0,
                64,
            )
            .map_err(Error::Transport)?;
        let desktop = connection
            .get_property(
                false,
                self.parent,
                atoms._NET_WM_DESKTOP,
                AtomEnum::CARDINAL,
                0,
                1,
            )
            .map_err(Error::Transport)?;
        let current = connection
            .get_property(
                false,
                self.root,
                atoms._NET_CURRENT_DESKTOP,
                AtomEnum::CARDINAL,
                0,
                1,
            )
            .map_err(Error::Transport)?;
        let focus = connection.get_input_focus().map_err(Error::Transport)?;
        let managed = managed.reply().map_err(Error::from_reply)?;
        let chrome = chrome.reply().map_err(Error::from_reply)?;
        let state = state.reply().map_err(Error::from_reply)?;
        let desktop = cardinal(&desktop.reply().map_err(Error::from_reply)?);
        let current = cardinal(&current.reply().map_err(Error::from_reply)?);
        let focus = self.focus_target(focus.reply().map_err(Error::from_reply)?.focus);
        let hidden = state.bytes_after != 0
            || state
                .value32()
                .is_some_and(|mut values| values.any(|atom| atom == atoms._NET_WM_STATE_HIDDEN));
        let root_mapped = managed.map_state == MapState::VIEWABLE && !hidden;
        let wrong_desktop =
            matches!((desktop, current), (Some(a), Some(b)) if a != b && a != u32::MAX);
        Ok(CompanionFacts {
            root_viewable: root_mapped && !wrong_desktop,
            root_mapped,
            companion_viewable: chrome.map_state == MapState::VIEWABLE,
            focus,
        })
    }

    /// Map at zero compositor opacity with no input while GPUI prepares a
    /// transparent frame. Never map or raise outside the settled app family.
    pub(in crate::native_host::gpui) fn map_companion(&self) -> Result<bool, Error> {
        let companion = self.bound_companion()?;
        let placed = companion.presentation.borrow().placed;
        if !self.placement_matches(companion, placed)? {
            return Ok(false);
        }
        let facts = self.companion_facts()?;
        if !facts.root_viewable || facts.focus.is_none() {
            return Ok(false);
        }
        let mut presentation = companion.presentation.borrow_mut();
        self.conceal(companion, &mut presentation)?;
        drop(presentation);
        check(self.connection.configure_window(
            companion.xid,
            &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE),
        ))?;
        if !facts.companion_viewable {
            check(self.connection.map_window(companion.xid))?;
        }
        let after = self.companion_facts()?;
        if !after.root_viewable || !after.companion_viewable || after.focus.is_none() {
            self.hide_companion()?;
            return Ok(false);
        }
        Ok(true)
    }

    /// Commit only the hit regions rendered at the confirmed size and scale.
    /// Repeated frames with identical hit geometry perform no Shape mutation.
    pub(in crate::native_host::gpui) fn present_companion(
        &self,
        scale: f32,
        hits: &[HitRect],
        modal: bool,
    ) -> Result<bool, Error> {
        let companion = self.bound_companion()?;
        let mut presentation = companion.presentation.borrow_mut();
        let Some(placed) = presentation.placed else {
            return Ok(false);
        };
        hit_rectangles(
            &mut presentation.scratch,
            placed.extent(),
            scale,
            hits,
            modal,
        )?;
        let same_hits = presentation
            .hits
            .iter()
            .map(rectangle_edges)
            .eq(presentation.scratch.iter().map(rectangle_edges));
        if presentation.visible && same_hits {
            return Ok(true);
        }
        if !self.placement_matches(companion, Some(placed))? {
            self.conceal(companion, &mut presentation)?;
            return Ok(false);
        }
        let facts = self.companion_facts()?;
        if !facts.root_viewable || !facts.companion_viewable || facts.focus.is_none() {
            drop(presentation);
            self.hide_companion()?;
            return Ok(false);
        }
        let Presentation {
            visible,
            hits: previous,
            scratch,
            ..
        } = &mut *presentation;
        if !same_hits {
            self.input_shape(companion.xid, scratch)?;
            std::mem::swap(previous, scratch);
        }
        if !*visible {
            if let Err(error) =
                self.set_companion_opacity(companion.xid, &companion.atoms, u32::MAX)
            {
                // Roll back the input surface if first presentation failed.
                let _ = self.input_shape(companion.xid, &[]);
                previous.clear();
                return Err(error);
            }
            *visible = true;
        }
        Ok(true)
    }

    /// Immediately remove input and opacity, then unmap without moving focus.
    pub(in crate::native_host::gpui) fn hide_companion(&self) -> Result<(), Error> {
        let companion = self.bound_companion()?;
        let conceal = self.conceal(companion, &mut companion.presentation.borrow_mut());
        let unmap = check(self.connection.unmap_window(companion.xid));
        conceal.and(unmap)
    }

    /// Call only for a real GPUI press or an explicit accessible focus intent.
    /// The fresh server timestamp belongs to this intent and is not retained.
    pub(in crate::native_host::gpui) fn focus_companion(&self) -> Result<bool, Error> {
        self.focus_with_property(FocusTarget::Root, FocusTarget::Companion)
    }

    /// Return focus only from the still-viewable companion to its managed root.
    pub(in crate::native_host::gpui) fn return_focus(&self) -> Result<bool, Error> {
        self.focus_with_property(FocusTarget::Companion, FocusTarget::Root)
    }

    /// Consume a Core Tab press delivered to the opposite focus target.
    pub(in crate::native_host::gpui) fn focus_with_key(
        &self,
        target: FocusTarget,
        time: u32,
    ) -> Result<bool, Error> {
        let source = match target {
            FocusTarget::Root => FocusTarget::Companion,
            FocusTarget::Companion => FocusTarget::Root,
        };
        if time == 0 || self.last_press.take() != Some((source, time)) {
            return Ok(false);
        }
        self.transfer_focus(source, target, time)
    }

    fn focus_with_property(&self, source: FocusTarget, target: FocusTarget) -> Result<bool, Error> {
        let facts = self.companion_facts()?;
        if !facts.root_viewable || !facts.companion_viewable {
            return Ok(false);
        }
        if facts.focus == Some(target) {
            return Ok(true);
        }
        if facts.focus != Some(source) {
            return Ok(false);
        }
        let time = self.focus_timestamp()?;
        self.transfer_focus(source, target, time)
    }

    fn transfer_focus(
        &self,
        source: FocusTarget,
        target: FocusTarget,
        time: u32,
    ) -> Result<bool, Error> {
        let companion = self.bound_companion()?;
        if target == FocusTarget::Companion {
            let presentation = companion.presentation.borrow();
            if !presentation.visible || !self.placement_matches(companion, presentation.placed)? {
                return Ok(false);
            }
        }
        // Read again *after* obtaining the timestamp. A later X11 focus change
        // also invalidates this time via SetInputFocus's server-side ordering.
        let settled = self.companion_facts()?;
        if !settled.root_viewable || !settled.companion_viewable || settled.focus != Some(source) {
            return Ok(false);
        }
        let xid = match target {
            FocusTarget::Root => self.parent,
            FocusTarget::Companion => companion.xid,
        };
        check(self.connection.set_input_focus(InputFocus::NONE, xid, time))?;
        let after = self.companion_facts()?;
        Ok(after.root_viewable && after.companion_viewable && after.focus == Some(target))
    }

    fn focus_timestamp(&self) -> Result<u32, Error> {
        let companion = self.bound_companion()?;
        let atom = companion.atoms._FURAMI_CHROME_FOCUS_TIME;
        let request = self
            .connection
            .change_property8(
                PropMode::REPLACE,
                companion.xid,
                atom,
                AtomEnum::INTEGER,
                &[0],
            )
            .map_err(Error::Transport)?;
        let sequence = request.sequence_number();
        // `.check()` synchronizes with the server; its matching PropertyNotify
        // precedes that round trip's reply. Never block waiting for another
        // event, or lose an unrelated key, focus or media loss in this drain.
        request.check().map_err(Error::from_reply)?;
        while let Some((event, event_sequence)) = self
            .connection
            .poll_for_event_with_sequence()
            .map_err(Error::Transport)?
        {
            if let Event::PropertyNotify(notify) = &event
                && event.server_generated()
                && event_sequence == sequence
                && notify.window == companion.xid
                && notify.atom == atom
                && notify.state == Property::NEW_VALUE
                && notify.time != 0
            {
                return Ok(notify.time);
            }
            self.pending.borrow_mut().push_back(event);
        }
        Err(Error::State(
            "missing server timestamp for GPUI chrome focus intent",
        ))
    }

    fn bound_companion(&self) -> Result<&Companion, Error> {
        self.companion
            .as_ref()
            .ok_or(Error::State("the X11 chrome companion is not bound"))
    }

    fn is_argb_visual(&self, visual: u32) -> Result<bool, Error> {
        let Some(screen) = self
            .connection
            .setup()
            .roots
            .iter()
            .position(|screen| screen.root == self.root)
        else {
            return Ok(false);
        };
        let formats = self
            .connection
            .render_query_pict_formats()
            .map_err(Error::Transport)?
            .reply()
            .map_err(Error::from_reply)?;
        let format = formats.screens.get(screen).and_then(|screen| {
            screen
                .depths
                .iter()
                .filter(|depth| depth.depth == 32)
                .flat_map(|depth| &depth.visuals)
                .find(|entry| entry.visual == visual)
        });
        Ok(format.is_some_and(|visual| {
            formats.formats.iter().any(|format| {
                format.id == visual.format
                    && format.type_ == PictType::DIRECT
                    && format.depth == 32
                    && format.direct.alpha_mask == 0xff
                    && format.direct.alpha_shift == 24
                    && format.direct.red_mask == 0xff
                    && format.direct.red_shift == 16
                    && format.direct.green_mask == 0xff
                    && format.direct.green_shift == 8
                    && format.direct.blue_mask == 0xff
                    && format.direct.blue_shift == 0
            })
        }))
    }

    fn placement_matches(
        &self,
        companion: &Companion,
        placed: Option<CompanionGeometry>,
    ) -> Result<bool, Error> {
        let Some(placed) = placed else {
            return Ok(false);
        };
        Ok(self.companion_geometry()? == placed
            && self.read_companion_geometry(companion)? == placed)
    }

    fn read_companion_geometry(&self, companion: &Companion) -> Result<CompanionGeometry, Error> {
        let reply = self
            .connection
            .get_geometry(companion.xid)
            .map_err(Error::Transport)?
            .reply()
            .map_err(Error::from_reply)?;
        if reply.root != self.root || reply.border_width != 0 {
            return Err(Error::State(
                "the chrome companion no longer matches its X11 root",
            ));
        }
        Ok(CompanionGeometry {
            x: reply.x,
            y: reply.y,
            width: reply.width,
            height: reply.height,
        })
    }

    fn conceal(&self, companion: &Companion, presentation: &mut Presentation) -> Result<(), Error> {
        if !presentation.hits.is_empty() {
            self.input_shape(companion.xid, &[])?;
            presentation.hits.clear();
        }
        if presentation.visible {
            self.set_companion_opacity(companion.xid, &companion.atoms, 0)?;
            presentation.visible = false;
        }
        Ok(())
    }

    fn input_shape(&self, xid: u32, rects: &[Rectangle]) -> Result<(), Error> {
        check(self.connection.shape_rectangles(
            SO::SET,
            SK::INPUT,
            ClipOrdering::UNSORTED,
            xid,
            0,
            0,
            rects,
        ))
    }

    fn set_companion_opacity(&self, xid: u32, atoms: &Atoms, opacity: u32) -> Result<(), Error> {
        check(self.connection.change_property32(
            PropMode::REPLACE,
            xid,
            atoms._NET_WM_WINDOW_OPACITY,
            AtomEnum::CARDINAL,
            &[opacity],
        ))
    }
}

fn check(request: Result<VoidCookie<'_, RustConnection>, ConnectionError>) -> Result<(), Error> {
    request
        .map_err(Error::Transport)?
        .check()
        .map_err(Error::from_reply)
}

fn cardinal(reply: &GetPropertyReply) -> Option<u32> {
    single_property(reply, AtomEnum::CARDINAL)
}

fn single_property(reply: &GetPropertyReply, type_: AtomEnum) -> Option<u32> {
    if reply.type_ != u32::from(type_) || reply.bytes_after != 0 || reply.value_len != 1 {
        return None;
    }
    reply.value32()?.next()
}

fn placement_invalidated(
    previous: Option<CompanionGeometry>,
    actual: CompanionGeometry,
    desired: CompanionGeometry,
) -> bool {
    previous.is_some_and(|previous| previous != actual || previous.extent() != desired.extent())
}

fn offscreen(x: i16, y: i16, width: u16, height: u16) -> bool {
    width != 0
        && height != 0
        && i32::from(x) + i32::from(width) <= 0
        && i32::from(y) + i32::from(height) <= 0
}

fn rectangle_edges(rectangle: &Rectangle) -> (i16, i16, u16, u16) {
    (rectangle.x, rectangle.y, rectangle.width, rectangle.height)
}

fn hit_rectangles(
    output: &mut Vec<Rectangle>,
    (width, height): (u16, u16),
    scale: f32,
    hits: &[HitRect],
    modal: bool,
) -> Result<(), Error> {
    if !scale.is_finite() || scale <= 0.0 {
        return Err(Error::Geometry(
            "invalid scale for the X11 companion input shape",
        ));
    }
    output.clear();
    if modal {
        output.push(Rectangle {
            x: 0,
            y: 0,
            width,
            height,
        });
        return Ok(());
    }
    let scale = f64::from(scale);
    for rect in hits {
        if ![rect.left, rect.top, rect.right, rect.bottom]
            .into_iter()
            .all(f32::is_finite)
        {
            return Err(Error::Geometry("nonfinite GPUI chrome hit rectangle"));
        }
        if rect.left >= rect.right || rect.top >= rect.bottom {
            continue;
        }
        let left = (f64::from(rect.left) * scale)
            .floor()
            .clamp(0.0, f64::from(width)) as u16;
        let top = (f64::from(rect.top) * scale)
            .floor()
            .clamp(0.0, f64::from(height)) as u16;
        let right = (f64::from(rect.right) * scale)
            .ceil()
            .clamp(0.0, f64::from(width)) as u16;
        let bottom = (f64::from(rect.bottom) * scale)
            .ceil()
            .clamp(0.0, f64::from(height)) as u16;
        if right > left && bottom > top {
            output.push(Rectangle {
                x: i16::try_from(left).map_err(|_| Error::Geometry("X11 hit x overflow"))?,
                y: i16::try_from(top).map_err(|_| Error::Geometry("X11 hit y overflow"))?,
                width: right - left,
                height: bottom - top,
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gpui_host_companion_restages_for_resize_or_external_drift_not_ordinary_move() {
        let old = CompanionGeometry {
            x: 100,
            y: 120,
            width: 960,
            height: 600,
        };
        let moved = CompanionGeometry {
            x: 140,
            y: 130,
            ..old
        };
        let smaller = CompanionGeometry {
            width: 480,
            height: 540,
            ..moved
        };
        assert!(!placement_invalidated(None, old, moved));
        assert!(!placement_invalidated(Some(old), old, moved));
        assert!(!placement_invalidated(Some(moved), moved, moved));
        assert!(placement_invalidated(Some(old), old, smaller));
        assert!(placement_invalidated(Some(old), moved, old));
        assert!(placement_invalidated(Some(old), smaller, old));
    }

    #[test]
    fn gpui_host_companion_initial_extent_must_be_entirely_negative_on_both_axes() {
        assert!(offscreen(-32, -40, 32, 32));
        assert!(!offscreen(-31, -40, 32, 32));
        assert!(!offscreen(-40, -31, 32, 32));
        assert!(!offscreen(-40, 0, 32, 32));
        assert!(!offscreen(-40, -40, 0, 32));
    }

    #[test]
    fn gpui_host_companion_input_rounds_outward_and_clips_without_bridging_holes() {
        let mut output = Vec::new();
        hit_rectangles(
            &mut output,
            (40, 30),
            1.5,
            &[
                HitRect {
                    left: -5.0,
                    top: 1.5,
                    right: 8.5,
                    bottom: 6.5,
                },
                HitRect {
                    left: 20.0,
                    top: 17.0,
                    right: 37.0,
                    bottom: 25.0,
                },
                HitRect {
                    left: 40.0,
                    top: 0.0,
                    right: 48.0,
                    bottom: 3.0,
                },
                HitRect {
                    left: 10.1,
                    top: 0.0,
                    right: 10.1,
                    bottom: 3.0,
                },
            ],
            false,
        )
        .unwrap();
        assert_eq!(
            output.iter().map(rectangle_edges).collect::<Vec<_>>(),
            [(0, 2, 13, 8), (30, 25, 10, 5)],
        );
    }

    #[test]
    fn gpui_host_companion_full_input_is_reserved_for_modal_backdrop() {
        let mut output = Vec::new();
        hit_rectangles(&mut output, (40, 30), 1.0, &[], false).unwrap();
        assert!(output.is_empty());
        hit_rectangles(&mut output, (40, 30), 1.0, &[], true).unwrap();
        assert!(output.iter().map(rectangle_edges).eq([(0, 0, 40, 30)]));
        hit_rectangles(&mut output, (40, 30), 1.0, &[], false).unwrap();
        assert!(output.is_empty());
        assert!(hit_rectangles(&mut output, (40, 30), f32::NAN, &[], true).is_err());
    }
}
