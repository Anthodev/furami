//! Private libpulse ABI for source/output observation and owned loopback controls.

use std::{
    cell::{Cell, OnceCell, RefCell},
    collections::BTreeSet,
    ffi::{CStr, c_char, c_int, c_void},
    ptr::{self, NonNull},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use crate::{
    capture::audio::AudioSource,
    domain::{
        capture::{AudioError, AudioSourceIdentity},
        output::{LiveSinkTarget, SinkCatalog, SinkIdentity, SinkObservation},
    },
};

const INVALID_INDEX: u32 = u32::MAX;
const READY: c_int = 4;
const FAILED: c_int = 5;
const TERMINATED: c_int = 6;
const NO_ENTITY: c_int = 5;
const SINK_EVENT: c_int = 0;
const SINK_INPUT_EVENT: c_int = 2;
const SERVER_EVENT: c_int = 7;
const SOURCE_EVENT: c_int = 1;
const SOURCE_OUTPUT_EVENT: c_int = 3;
const REMOVE_EVENT: c_int = 0x20;
const FACILITY_MASK: c_int = 0x0f;
const TYPE_MASK: c_int = 0x30;
const CONTROL_TIMEOUT: Duration = Duration::from_secs(5);

// Stable libpulse ABI prefixes. Introspection structures append fields between
// releases; only these documented leading members are read. Their pointers and
// strings remain borrowed for the callback only.
#[repr(C)]
struct SampleSpec {
    format: c_int,
    rate: u32,
    channels: u8,
}

#[repr(C)]
struct ChannelMap {
    channels: u8,
    map: [c_int; 32],
}

#[repr(C)]
struct Volume {
    channels: u8,
    values: [u32; 32],
}

#[repr(C)]
struct SourceInfo {
    name: *const c_char,
    index: u32,
    description: *const c_char,
    sample_spec: SampleSpec,
    channel_map: ChannelMap,
    owner_module: u32,
    volume: Volume,
    mute: c_int,
    monitor_of_sink: u32,
    monitor_of_sink_name: *const c_char,
    latency: u64,
    driver: *const c_char,
    flags: c_int,
    proplist: *mut c_void,
}

#[repr(C)]
struct SourceOutputInfo {
    index: u32,
    name: *const c_char,
    owner_module: u32,
    client: u32,
    source: u32,
    sample_spec: SampleSpec,
    channel_map: ChannelMap,
    buffer_usec: u64,
    source_usec: u64,
    resample_method: *const c_char,
    driver: *const c_char,
    proplist: *mut c_void,
}

#[repr(C)]
struct SinkInputInfo {
    index: u32,
    name: *const c_char,
    owner_module: u32,
    client: u32,
    sink: u32,
    sample_spec: SampleSpec,
    channel_map: ChannelMap,
    volume: Volume,
    buffer_usec: u64,
    sink_usec: u64,
    resample_method: *const c_char,
    driver: *const c_char,
    mute: c_int,
    proplist: *mut c_void,
    corked: c_int,
    has_volume: c_int,
    volume_writable: c_int,
}

// Public PulseAudio v17.0 introspect.h prefixes, not PipeWire private ABI.
#[repr(C)]
struct SinkInfo {
    name: *const c_char,
    index: u32,
    description: *const c_char,
    sample_spec: SampleSpec,
    channel_map: ChannelMap,
    owner_module: u32,
    volume: Volume,
    mute: c_int,
    monitor_source: u32,
    monitor_source_name: *const c_char,
    latency: u64,
    driver: *const c_char,
    flags: c_int,
    proplist: *mut c_void,
    configured_latency: u64,
    base_volume: u32,
    state: c_int,
    n_volume_steps: u32,
    card: u32,
    n_ports: u32,
    ports: *mut *mut SinkPortInfo,
    active_port: *mut SinkPortInfo,
}

#[repr(C)]
struct SinkPortInfo {
    name: *const c_char,
    description: *const c_char,
    priority: u32,
    available: c_int,
}

#[repr(C)]
struct ServerInfo {
    user_name: *const c_char,
    host_name: *const c_char,
    server_version: *const c_char,
    server_name: *const c_char,
    sample_spec: SampleSpec,
    default_sink_name: *const c_char,
}

type SinkCallback = unsafe extern "C" fn(*mut c_void, *const SinkInfo, c_int, *mut c_void);
type ServerCallback = unsafe extern "C" fn(*mut c_void, *const ServerInfo, *mut c_void);

type SourceCallback = unsafe extern "C" fn(*mut c_void, *const SourceInfo, c_int, *mut c_void);
type OutputCallback =
    unsafe extern "C" fn(*mut c_void, *const SourceOutputInfo, c_int, *mut c_void);
type SinkInputCallback =
    unsafe extern "C" fn(*mut c_void, *const SinkInputInfo, c_int, *mut c_void);
type SuccessCallback = unsafe extern "C" fn(*mut c_void, c_int, *mut c_void);
type SubscribeCallback = unsafe extern "C" fn(*mut c_void, c_int, u32, *mut c_void);

// Same runtime-SONAME pattern as udev.rs: no host header or development symlink.
#[link(name = "libpulse.so.0", kind = "dylib", modifiers = "+verbatim")]
unsafe extern "C" {
    fn pa_mainloop_new() -> *mut c_void;
    fn pa_mainloop_free(mainloop: *mut c_void);
    fn pa_mainloop_get_api(mainloop: *mut c_void) -> *mut c_void;
    fn pa_mainloop_prepare(mainloop: *mut c_void, timeout: c_int) -> c_int;
    fn pa_mainloop_poll(mainloop: *mut c_void) -> c_int;
    fn pa_mainloop_dispatch(mainloop: *mut c_void) -> c_int;
    fn pa_context_new(api: *mut c_void, name: *const c_char) -> *mut c_void;
    fn pa_context_unref(context: *mut c_void);
    fn pa_context_connect(
        context: *mut c_void,
        server: *const c_char,
        flags: c_int,
        spawn_api: *const c_void,
    ) -> c_int;
    fn pa_context_disconnect(context: *mut c_void);
    fn pa_context_get_state(context: *const c_void) -> c_int;
    fn pa_context_errno(context: *const c_void) -> c_int;
    fn pa_strerror(error: c_int) -> *const c_char;
    fn pa_proplist_gets(proplist: *const c_void, key: *const c_char) -> *const c_char;
    fn pa_context_get_source_info_list(
        context: *mut c_void,
        callback: SourceCallback,
        userdata: *mut c_void,
    ) -> *mut c_void;
    fn pa_context_get_sink_input_info_list(
        context: *mut c_void,
        callback: SinkInputCallback,
        userdata: *mut c_void,
    ) -> *mut c_void;
    fn pa_context_get_source_output_info_list(
        context: *mut c_void,
        callback: OutputCallback,
        userdata: *mut c_void,
    ) -> *mut c_void;
    fn pa_context_get_source_output_info(
        context: *mut c_void,
        index: u32,
        callback: OutputCallback,
        userdata: *mut c_void,
    ) -> *mut c_void;
    fn pa_context_subscribe(
        context: *mut c_void,
        mask: c_int,
        callback: SuccessCallback,
        userdata: *mut c_void,
    ) -> *mut c_void;
    fn pa_context_set_subscribe_callback(
        context: *mut c_void,
        callback: Option<SubscribeCallback>,
        userdata: *mut c_void,
    );
    fn pa_context_get_sink_info_list(
        context: *mut c_void,
        callback: SinkCallback,
        userdata: *mut c_void,
    ) -> *mut c_void;
    fn pa_context_get_server_info(
        context: *mut c_void,
        callback: ServerCallback,
        userdata: *mut c_void,
    ) -> *mut c_void;
    fn pa_context_get_sink_input_info(
        context: *mut c_void,
        index: u32,
        callback: SinkInputCallback,
        userdata: *mut c_void,
    ) -> *mut c_void;
    fn pa_context_set_sink_input_mute(
        context: *mut c_void,
        index: u32,
        mute: c_int,
        callback: SuccessCallback,
        userdata: *mut c_void,
    ) -> *mut c_void;
    fn pa_context_set_sink_input_volume(
        context: *mut c_void,
        index: u32,
        volume: *const Volume,
        callback: SuccessCallback,
        userdata: *mut c_void,
    ) -> *mut c_void;
    fn pa_operation_get_state(operation: *const c_void) -> c_int;
    fn pa_operation_cancel(operation: *mut c_void);
    fn pa_operation_unref(operation: *mut c_void);
}

struct Operation(NonNull<c_void>);

impl Drop for Operation {
    fn drop(&mut self) {
        // SAFETY: Cancel suppresses all future callbacks before userdata can be
        // freed. Server-side controls already queued may still complete.
        unsafe {
            pa_operation_cancel(self.0.as_ptr());
            pa_operation_unref(self.0.as_ptr());
        }
    }
}

#[derive(Default)]
struct Notifications {
    source_dirty: Cell<bool>,
    selected_index: Cell<u32>,
    inventory: Cell<bool>,
    selected_removed: Cell<bool>,
    native_invalidation: OnceCell<Box<dyn Fn(bool) -> u64>>,
    native_revision: Cell<u64>,
    sink_dirty: Cell<bool>,
    removed_sinks: RefCell<BTreeSet<u32>>,
    playback_dirty: Cell<bool>,
    capture_dirty: Cell<bool>,
    loopback_capture: Cell<Option<u32>>,
    loopback_playback: Cell<Option<u32>>,
    loopback_removed: Cell<bool>,
}

struct Connection {
    mainloop: NonNull<c_void>,
    context: NonNull<c_void>,
    notifications: Box<Notifications>,
    external_cancel: Option<Arc<AtomicBool>>,
    operation_deadline: Option<Instant>,
}

impl Connection {
    fn connect(shutdown: Option<&AtomicBool>) -> Result<Self, AudioError> {
        Self::connect_with(shutdown, None)
    }

    fn connect_with(
        shutdown: Option<&AtomicBool>,
        external_cancel: Option<Arc<AtomicBool>>,
    ) -> Result<Self, AudioError> {
        // SAFETY: Official constructor, one owned reference.
        let mainloop = NonNull::new(unsafe { pa_mainloop_new() })
            .ok_or_else(|| AudioError::Unavailable("pa_mainloop_new returned null".into()))?;
        // SAFETY: API remains owned by mainloop; static name lives through call.
        let raw = unsafe {
            pa_context_new(
                pa_mainloop_get_api(mainloop.as_ptr()),
                c"Furami audio control".as_ptr(),
            )
        };
        let Some(context) = NonNull::new(raw) else {
            // SAFETY: Context construction failed, no remaining dependents.
            unsafe { pa_mainloop_free(mainloop.as_ptr()) };
            return Err(AudioError::Unavailable(
                "pa_context_new returned null".into(),
            ));
        };
        let mut connection = Self {
            mainloop,
            context,
            notifications: Box::new(Notifications::default()),
            external_cancel,
            operation_deadline: None,
        };
        // SAFETY: Live context; NOAUTOSPAWN=1 prevents launching any daemon.
        if unsafe { pa_context_connect(context.as_ptr(), ptr::null(), 1, ptr::null()) } < 0 {
            return Err(connection.error("connect"));
        }
        let deadline = Instant::now() + CONTROL_TIMEOUT;
        loop {
            // SAFETY: Owned context is live on this thread.
            match unsafe { pa_context_get_state(context.as_ptr()) } {
                READY => return Ok(connection),
                FAILED | TERMINATED => return Err(connection.error("connect")),
                _ => {}
            }
            connection.step(deadline, shutdown)?;
        }
    }

    fn error(&self, operation: &str) -> AudioError {
        // SAFETY: Live context; static error string copied before returning.
        unsafe { context_error(self.context.as_ptr(), operation) }
    }

    fn ready(&self) -> bool {
        // SAFETY: Live context only accessed by its owning thread.
        unsafe { pa_context_get_state(self.context.as_ptr()) == READY }
    }

    fn step(&mut self, deadline: Instant, shutdown: Option<&AtomicBool>) -> Result<(), AudioError> {
        if shutdown.is_some_and(|flag| flag.load(Ordering::Acquire))
            || self
                .external_cancel
                .as_ref()
                .is_some_and(|flag| flag.load(Ordering::Acquire))
        {
            return Err(AudioError::Cancelled);
        }
        if Instant::now() >= deadline {
            return Err(AudioError::Unavailable(
                "Pulse control operation timed out".into(),
            ));
        }
        // SAFETY: Prepare/poll/dispatch sequence on exclusively owned mainloop.
        // Bounded poll observes cancellation without an mpv/owner roundtrip.
        unsafe {
            if pa_mainloop_prepare(self.mainloop.as_ptr(), 10_000) < 0
                || pa_mainloop_poll(self.mainloop.as_ptr()) < 0
                || pa_mainloop_dispatch(self.mainloop.as_ptr()) < 0
            {
                return Err(self.error("mainloop"));
            }
        }
        Ok(())
    }

    fn drain(&mut self) -> Result<(), AudioError> {
        if !self.ready() {
            return Err(self.error("subscription connection"));
        }
        // SAFETY: Exclusive owner and zero-timeout prepare/poll/dispatch.
        unsafe {
            if pa_mainloop_prepare(self.mainloop.as_ptr(), 0) < 0
                || pa_mainloop_poll(self.mainloop.as_ptr()) < 0
                || pa_mainloop_dispatch(self.mainloop.as_ptr()) < 0
            {
                return Err(self.error("subscription drain"));
            }
        }
        if !self.ready() {
            return Err(self.error("subscription connection"));
        }
        Ok(())
    }

    fn wait(
        &mut self,
        raw: *mut c_void,
        done: &Cell<bool>,
        shutdown: Option<&AtomicBool>,
    ) -> Result<(), AudioError> {
        let operation = Operation(NonNull::new(raw).ok_or_else(|| self.error("queue operation"))?);
        let deadline = self
            .operation_deadline
            .unwrap_or_else(|| Instant::now() + CONTROL_TIMEOUT);
        while !done.get() {
            if !self.ready() {
                return Err(self.error("operation connection"));
            }
            // SAFETY: Operation reference retained through wait.
            if unsafe { pa_operation_get_state(operation.0.as_ptr()) } != 0 {
                return Err(self.error("operation ended without callback"));
            }
            self.step(deadline, shutdown)?;
        }
        Ok(())
    }

    fn sources(
        &mut self,
        shutdown: Option<&AtomicBool>,
    ) -> Result<Vec<ObservedSource>, AudioError> {
        let reply = Reply::<ObservedSource>::default();
        // SAFETY: Callback copies borrowed fields into reply. Operation wait
        // cancels/unrefs before reply leaves scope, including every error path.
        let raw = unsafe {
            pa_context_get_source_info_list(
                self.context.as_ptr(),
                source_callback,
                reply.userdata(),
            )
        };
        self.wait(raw, &reply.done, shutdown)?;
        reply.finish()
    }

    fn subscribe(&mut self, index: u32, shutdown: &AtomicBool) -> Result<(), AudioError> {
        self.notifications.selected_index.set(index);
        let userdata = ptr::from_ref(self.notifications.as_ref()).cast_mut().cast();
        // SAFETY: Box address stable until callback cleared in Drop, before
        // disconnect/unref/free. Callback executes only on this mainloop thread.
        unsafe {
            pa_context_set_subscribe_callback(
                self.context.as_ptr(),
                Some(subscription_callback),
                userdata,
            )
        };
        let reply = Success::default();
        // Observe selected-source and owned-loopback stream changes.
        // SAFETY: Callback lifetime bounded by wait, as above.
        let raw = unsafe {
            pa_context_subscribe(
                self.context.as_ptr(),
                0x02 | 0x04 | 0x08,
                success_callback,
                reply.userdata(),
            )
        };
        self.wait(raw, &reply.done, Some(shutdown))?;
        reply.finish().map_err(|_| self.error("subscribe"))
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        // SAFETY: Every operation has already cancelled its callback. Clearing
        // subscription callback precedes freeing its userdata; context precedes
        // mainloop API destruction. No reconnect can reuse stale indices.
        unsafe {
            pa_context_set_subscribe_callback(self.context.as_ptr(), None, ptr::null_mut());
            pa_context_disconnect(self.context.as_ptr());
            pa_context_unref(self.context.as_ptr());
            pa_mainloop_free(self.mainloop.as_ptr());
        }
    }
}

struct Reply<T> {
    done: Cell<bool>,
    rows: RefCell<Vec<T>>,
    error: RefCell<Option<AudioError>>,
    allow_missing: Cell<bool>,
}

impl<T> Default for Reply<T> {
    fn default() -> Self {
        Self {
            done: Cell::new(false),
            rows: RefCell::new(vec![]),
            error: RefCell::new(None),
            allow_missing: Cell::new(false),
        }
    }
}

impl<T> Reply<T> {
    fn userdata(&self) -> *mut c_void {
        ptr::from_ref(self).cast_mut().cast()
    }

    unsafe fn end_error(&self, context: *mut c_void, operation: &str) {
        // SAFETY: Error is read inside this correlated negative-eol callback,
        // never inferred from stale context errno after timeout/other failures.
        let code = unsafe { pa_context_errno(context) };
        let ready = unsafe { pa_context_get_state(context) == READY };
        if !missing_lookup(
            self.allow_missing.get(),
            self.rows.borrow().is_empty(),
            ready,
            code,
        ) {
            self.row(Err(unsafe { context_error(context, operation) }));
        }
    }

    fn row(&self, row: Result<Option<T>, AudioError>) {
        match row {
            Ok(Some(row)) => self.rows.borrow_mut().push(row),
            Ok(None) => {}
            Err(error) => {
                if self.error.borrow().is_none() {
                    *self.error.borrow_mut() = Some(error);
                }
            }
        }
    }

    fn finish(self) -> Result<Vec<T>, AudioError> {
        match self.error.into_inner() {
            Some(error) => Err(error),
            None => Ok(self.rows.into_inner()),
        }
    }
}

#[derive(Default)]
struct Success {
    done: Cell<bool>,
    succeeded: Cell<bool>,
}

impl Success {
    fn userdata(&self) -> *mut c_void {
        ptr::from_ref(self).cast_mut().cast()
    }

    fn finish(self) -> Result<(), AudioError> {
        if self.succeeded.get() {
            Ok(())
        } else {
            Err(AudioError::Control(
                "Pulse server rejected operation".into(),
            ))
        }
    }
}

unsafe extern "C" fn success_callback(
    _context: *mut c_void,
    success: c_int,
    userdata: *mut c_void,
) {
    // SAFETY: Success remains live until operation cancelled/unreferenced.
    let reply = unsafe { &*userdata.cast::<Success>() };
    reply.succeeded.set(success != 0);
    reply.done.set(true);
}

fn missing_lookup(targeted: bool, empty: bool, ready: bool, code: c_int) -> bool {
    targeted && empty && ready && code == NO_ENTITY
}

unsafe extern "C" fn subscription_callback(
    _context: *mut c_void,
    event: c_int,
    index: u32,
    userdata: *mut c_void,
) {
    // SAFETY: Box owned by Connection, callback cleared before Box freed.
    let notifications = unsafe { &*userdata.cast::<Notifications>() };
    match event & FACILITY_MASK {
        SINK_EVENT => {
            notifications.sink_dirty.set(true);
            if event & TYPE_MASK == REMOVE_EVENT {
                notifications.removed_sinks.borrow_mut().insert(index);
            }
        }
        SERVER_EVENT => notifications.sink_dirty.set(true),
        SINK_INPUT_EVENT => {
            notifications.playback_dirty.set(true);
            if event & TYPE_MASK == REMOVE_EVENT
                && notifications.loopback_playback.get() == Some(index)
            {
                notifications.loopback_removed.set(true);
            }
        }
        SOURCE_EVENT => {
            if notifications.inventory.get() || index == notifications.selected_index.get() {
                notifications.source_dirty.set(true);
                if let Some(invalidate) = notifications.native_invalidation.get() {
                    let removed = index == notifications.selected_index.get()
                        && event & TYPE_MASK == REMOVE_EVENT;
                    notifications.native_revision.set(invalidate(removed));
                }
            }
            if index == notifications.selected_index.get() && event & TYPE_MASK == REMOVE_EVENT {
                notifications.selected_removed.set(true);
            }
        }
        SOURCE_OUTPUT_EVENT if notifications.loopback_capture.get() == Some(index) => {
            notifications.capture_dirty.set(true);
            if event & TYPE_MASK == REMOVE_EVENT {
                notifications.loopback_removed.set(true);
            }
        }
        _ => {}
    }
}

unsafe fn context_error(context: *mut c_void, operation: &str) -> AudioError {
    // SAFETY: Called only with live callback/owned context. strerror is static.
    let code = unsafe { pa_context_errno(context) };
    let raw = unsafe { pa_strerror(code) };
    let detail = if raw.is_null() {
        format!("Pulse error {code}")
    } else {
        // SAFETY: Pulse API guarantees NUL termination for its error string.
        unsafe { CStr::from_ptr(raw) }
            .to_string_lossy()
            .into_owned()
    };
    AudioError::Unavailable(format!("{operation}: {detail} ({code})"))
}

unsafe fn copied_string(raw: *const c_char) -> Result<Option<String>, AudioError> {
    if raw.is_null() {
        return Ok(None);
    }
    // SAFETY: Caller passes a callback-scoped, NUL-terminated libpulse string.
    unsafe { CStr::from_ptr(raw) }
        .to_str()
        .map(|value| Some(value.to_owned()))
        .map_err(|_| AudioError::Unavailable("non-UTF-8 Pulse identity property".into()))
}

unsafe fn property(proplist: *mut c_void, key: &CStr) -> Result<Option<String>, AudioError> {
    if proplist.is_null() {
        return Ok(None);
    }
    // SAFETY: Borrowed callback proplist is live; getter string copied at once.
    unsafe { copied_string(pa_proplist_gets(proplist, key.as_ptr())) }
}

unsafe fn native_object_serial(proplist: *mut c_void) -> Option<u64> {
    if proplist.is_null() {
        return None;
    }
    // SAFETY: Callback-scoped proplist, copied/parsed before returning. Invalid
    // native metadata never changes the existing stable catalog's semantics.
    let raw = unsafe { pa_proplist_gets(proplist, c"object.serial".as_ptr()) };
    if raw.is_null() {
        return None;
    }
    let bytes = unsafe { CStr::from_ptr(raw) }.to_bytes();
    if bytes.is_empty() || !bytes.iter().all(u8::is_ascii_digit) {
        return None;
    }
    std::str::from_utf8(bytes)
        .ok()?
        .parse::<u64>()
        .ok()
        .filter(|value| *value != 0 && *value != u64::MAX)
}

#[derive(Clone)]
struct ObservedSource {
    source: AudioSource,
    index: u32,
    native_serial: Option<u64>,
    rate: u32,
    channels: u8,
    channel_positions: [i32; 32],
}

unsafe extern "C" fn source_callback(
    context: *mut c_void,
    info: *const SourceInfo,
    eol: c_int,
    userdata: *mut c_void,
) {
    // SAFETY: Matching Reply type and bounded operation lifetime.
    let reply = unsafe { &*userdata.cast::<Reply<ObservedSource>>() };
    if eol != 0 {
        if eol < 0 {
            reply.row(Err(unsafe { context_error(context, "source inventory") }));
        }
        reply.done.set(true);
        return;
    }
    if info.is_null() {
        reply.row(Err(AudioError::Unavailable(
            "null source inventory row".into(),
        )));
        return;
    }
    // SAFETY: Info prefix and all pointed-to data valid during callback only.
    let info = unsafe { &*info };
    if !source_is_capture(info.monitor_of_sink) {
        return;
    }
    let row = (|| {
        let name = unsafe { copied_string(info.name) }?
            .ok_or_else(|| AudioError::Unavailable("source without name".into()))?;
        let description =
            unsafe { copied_string(info.description) }?.unwrap_or_else(|| name.clone());
        let mut properties = vec![];
        for key in [
            c"device.serial",
            c"device.bus",
            c"device.bus_path",
            c"device.bus-id",
            c"device.vendor.id",
            c"device.product.id",
            c"device.api",
        ] {
            if let Some(value) =
                unsafe { property(info.proplist, key) }?.filter(|value| !value.is_empty())
            {
                properties.push((key.to_str().expect("static ASCII key").into(), value));
            }
        }
        if info.index == INVALID_INDEX {
            return Err(AudioError::Unavailable("source has invalid index".into()));
        }
        Ok(Some(ObservedSource {
            source: AudioSource {
                identity: AudioSourceIdentity::new(name, properties)?,
                description,
            },
            index: info.index,
            native_serial: unsafe { native_object_serial(info.proplist) },
            rate: info.sample_spec.rate,
            channels: if info.channel_map.channels == info.sample_spec.channels
                && (1..=32).contains(&info.channel_map.channels)
            {
                info.channel_map.channels
            } else {
                0
            },
            channel_positions: info.channel_map.map,
        }))
    })();
    reply.row(row);
}

fn source_is_capture(monitor_of_sink: u32) -> bool {
    monitor_of_sink == INVALID_INDEX
}

pub(crate) fn discover() -> Result<Vec<AudioSource>, AudioError> {
    let mut connection = Connection::connect(None)?;
    Ok(connection
        .sources(None)?
        .into_iter()
        .map(|row| row.source)
        .collect())
}

fn checked_native_source(
    selected: &AudioSourceIdentity,
    rows: &[ObservedSource],
) -> Result<super::super::audio::NativeSourceSnapshot, AudioError> {
    let mut matches = rows
        .iter()
        .filter(|row| row.source.identity.name() == selected.name());
    let row = matches.next().ok_or_else(|| AudioError::SourceMissing {
        name: selected.name().into(),
    })?;
    if matches.next().is_some() {
        return Err(AudioError::Ambiguous(format!(
            "multiple sources named `{}`",
            selected.name()
        )));
    }
    if !selected.compatible_with(&row.source.identity) {
        return Err(AudioError::SourceChanged {
            name: selected.name().into(),
        });
    }
    let serial = row.native_serial.ok_or_else(|| {
        AudioError::Unavailable("source has no usable PipeWire object.serial".into())
    })?;
    if rows
        .iter()
        .filter(|other| other.native_serial == Some(serial))
        .count()
        != 1
    {
        return Err(AudioError::Ambiguous(
            "PipeWire source serial is not unique".into(),
        ));
    }
    if row.rate == 0
        || row.channels == 0
        || row.channel_positions[..usize::from(row.channels)]
            .iter()
            .any(|position| *position < 0)
    {
        return Err(AudioError::Unavailable(
            "source has no positional PCM channel map".into(),
        ));
    }
    Ok(super::super::audio::NativeSourceSnapshot {
        identity: row.source.identity.clone(),
        serial,
        pulse_index: row.index,
        observation_revision: 0,
        rate: row.rate,
        channel_positions: row.channel_positions[..usize::from(row.channels)].to_vec(),
    })
}

/// Lifetime watcher adapter; its context never crosses the observation worker.
pub(crate) struct SourceInventorySubscription {
    connection: Connection,
    shutdown: Arc<AtomicBool>,
}

impl SourceInventorySubscription {
    pub(crate) fn connect(shutdown: Arc<AtomicBool>) -> Result<Self, AudioError> {
        let mut connection = Connection::connect(Some(&shutdown))?;
        connection.notifications.inventory.set(true);
        connection.subscribe(INVALID_INDEX, &shutdown)?;
        connection.notifications.source_dirty.set(true);
        Ok(Self {
            connection,
            shutdown,
        })
    }

    pub(crate) fn poll_dirty(&mut self) -> Result<bool, AudioError> {
        if self.shutdown.load(Ordering::Acquire) {
            return Err(AudioError::Cancelled);
        }
        self.connection.drain()?;
        Ok(self.connection.notifications.source_dirty.replace(false))
    }

    pub(crate) fn snapshot(&mut self) -> Result<Vec<AudioSource>, AudioError> {
        Ok(self
            .connection
            .sources(Some(&self.shutdown))?
            .into_iter()
            .map(|row| row.source)
            .collect())
    }
}

/// Values copied inside public Pulse callbacks. Neither client index nor a
/// process-name match grants ownership of native virtual streams.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LoopStream {
    pub index: u32,
    pub serial: Option<u64>,
    pub global_id: Option<u32>,
    pub node: Option<String>,
    pub app: Option<String>,
    pub media: Option<String>,
    pub pid: Option<String>,
    pub target: u32,
    pub volumes: Vec<u32>,
    pub muted: bool,
    pub volume_writable: bool,
}

unsafe fn loop_stream(
    index: u32,
    target: u32,
    proplist: *mut c_void,
) -> Result<LoopStream, AudioError> {
    Ok(LoopStream {
        index,
        serial: unsafe { native_object_serial(proplist) },
        global_id: unsafe { property(proplist, c"object.id") }?
            .filter(|value| !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()))
            .and_then(|value| value.parse::<u32>().ok())
            .filter(|id| *id != 0 && *id != INVALID_INDEX),
        node: unsafe { property(proplist, c"node.name") }?,
        app: unsafe { property(proplist, c"application.name") }?,
        media: unsafe { property(proplist, c"media.name") }?,
        pid: unsafe { property(proplist, c"application.process.id") }?,
        target,
        volumes: vec![],
        muted: false,
        volume_writable: false,
    })
}

unsafe extern "C" fn loop_capture_callback(
    context: *mut c_void,
    info: *const SourceOutputInfo,
    eol: c_int,
    userdata: *mut c_void,
) {
    // SAFETY: Reply and borrowed public ABI prefix live through this callback.
    let reply = unsafe { &*userdata.cast::<Reply<LoopStream>>() };
    if eol != 0 {
        if eol < 0 {
            unsafe { reply.end_error(context, "loopback capture") };
        }
        reply.done.set(true);
    } else if info.is_null() {
        reply.row(Err(AudioError::Unavailable(
            "null loopback capture row".into(),
        )));
    } else {
        let info = unsafe { &*info };
        reply.row(unsafe { loop_stream(info.index, info.source, info.proplist) }.map(Some));
    }
}

unsafe extern "C" fn loop_playback_callback(
    context: *mut c_void,
    info: *const SinkInputInfo,
    eol: c_int,
    userdata: *mut c_void,
) {
    // SAFETY: Reply and borrowed public ABI prefix live through this callback.
    let reply = unsafe { &*userdata.cast::<Reply<LoopStream>>() };
    if eol != 0 {
        if eol < 0 {
            unsafe { reply.end_error(context, "loopback playback") };
        }
        reply.done.set(true);
    } else if info.is_null() {
        reply.row(Err(AudioError::Unavailable(
            "null loopback playback row".into(),
        )));
    } else {
        let info = unsafe { &*info };
        reply.row((|| {
            let mut row = unsafe { loop_stream(info.index, info.sink, info.proplist) }?;
            if (1..=32).contains(&info.volume.channels) {
                row.volumes = info.volume.values[..usize::from(info.volume.channels)].to_vec();
            }
            row.muted = info.mute != 0;
            row.volume_writable = info.has_volume != 0 && info.volume_writable != 0;
            Ok(Some(row))
        })());
    }
}

unsafe extern "C" fn sink_callback(
    context: *mut c_void,
    info: *const SinkInfo,
    eol: c_int,
    userdata: *mut c_void,
) {
    // SAFETY: Reply owns every copied value; the callback retains no PA pointers.
    let reply = unsafe { &*userdata.cast::<Reply<SinkObservation>>() };
    if eol != 0 {
        if eol < 0 {
            unsafe { reply.end_error(context, "output catalog") };
        }
        reply.done.set(true);
        return;
    }
    if info.is_null() {
        reply.row(Err(AudioError::Unavailable(
            "null sink inventory row".into(),
        )));
        return;
    }
    let info = unsafe { &*info };
    reply.row((|| {
        let name = unsafe { copied_string(info.name) }?
            .ok_or_else(|| AudioError::Unavailable("sink without name".into()))?;
        let mut properties = vec![];
        for key in [
            c"device.serial",
            c"device.bus",
            c"device.bus_path",
            c"device.bus-id",
            c"device.vendor.id",
            c"device.product.id",
            c"device.api",
        ] {
            if let Some(value) =
                unsafe { property(info.proplist, key) }?.filter(|value| !value.is_empty())
            {
                properties.push((key.to_str().expect("static ASCII").into(), value));
            }
        }
        let identity = SinkIdentity::new(name.clone(), properties)
            .map_err(|error| AudioError::Unavailable(format!("sink identity: {error}")))?;
        let serial = unsafe { native_object_serial(info.proplist) }
            .and_then(std::num::NonZeroU64::new)
            .ok_or_else(|| AudioError::Unavailable(format!("sink `{name}` lacks object.serial")))?;
        let target = LiveSinkTarget::new(identity, serial, info.index)
            .map_err(|error| AudioError::Unavailable(format!("sink target: {error}")))?;
        Ok(Some(SinkObservation {
            target,
            description: unsafe { copied_string(info.description) }?.unwrap_or(name),
            eligible: (0..=2).contains(&info.state)
                && (info.active_port.is_null() || unsafe { (*info.active_port).available } != 1),
        }))
    })());
}

unsafe extern "C" fn server_callback(
    context: *mut c_void,
    info: *const ServerInfo,
    userdata: *mut c_void,
) {
    // SAFETY: Server info has one completion callback, without an eol argument.
    let reply = unsafe { &*userdata.cast::<Reply<String>>() };
    if info.is_null() {
        reply.row(Err(unsafe { context_error(context, "server defaults") }));
    } else {
        reply.row(unsafe { copied_string((*info).default_sink_name) });
    }
    reply.done.set(true);
}

impl Connection {
    fn subscribe_outputs(&mut self, shutdown: &AtomicBool, source: bool) -> Result<(), AudioError> {
        let userdata = ptr::from_ref(self.notifications.as_ref()).cast_mut().cast();
        // SAFETY: Stable Box, callback cleared before the connection is freed.
        unsafe {
            pa_context_set_subscribe_callback(
                self.context.as_ptr(),
                Some(subscription_callback),
                userdata,
            );
        }
        let reply = Success::default();
        let mask = 0x01 | 0x80 | if source { 0x02 | 0x04 | 0x08 } else { 0 };
        // SAFETY: Bounded operation owns callback userdata until cancellation.
        let raw = unsafe {
            pa_context_subscribe(
                self.context.as_ptr(),
                mask,
                success_callback,
                reply.userdata(),
            )
        };
        self.wait(raw, &reply.done, Some(shutdown))?;
        reply.finish()
    }

    fn sinks(&mut self, shutdown: &AtomicBool) -> Result<Vec<SinkObservation>, AudioError> {
        let reply = Reply::<SinkObservation>::default();
        // SAFETY: Bounded operation; all borrowed values copied by callback.
        let raw = unsafe {
            pa_context_get_sink_info_list(self.context.as_ptr(), sink_callback, reply.userdata())
        };
        self.wait(raw, &reply.done, Some(shutdown))?;
        reply.finish()
    }

    fn sink_catalog(
        &mut self,
        shutdown: &AtomicBool,
        revision: u64,
    ) -> Result<SinkCatalog, AudioError> {
        let sinks = self.sinks(shutdown)?;
        let reply = Reply::<String>::default();
        // SAFETY: Public API and callback lifetime bounded by wait.
        let raw = unsafe {
            pa_context_get_server_info(self.context.as_ptr(), server_callback, reply.userdata())
        };
        self.wait(raw, &reply.done, Some(shutdown))?;
        let names = reply.finish()?;
        let default_sink = names.first().and_then(|name| {
            let mut matches = sinks
                .iter()
                .filter(|row| row.target.identity.name() == name);
            let first = matches.next()?;
            matches
                .next()
                .is_none()
                .then(|| first.target.identity.clone())
        });
        Ok(SinkCatalog {
            revision,
            sinks,
            default_sink,
        })
    }

    fn loop_streams(
        &mut self,
        capture: bool,
        index: Option<u32>,
        shutdown: &AtomicBool,
    ) -> Result<Vec<LoopStream>, AudioError> {
        let reply = Reply::<LoopStream>::default();
        reply.allow_missing.set(index.is_some());
        // SAFETY: Callback type exactly matches each public API; Reply lives
        // until the operation has been cancelled/unreferenced on every path.
        let raw = unsafe {
            match (capture, index) {
                (true, Some(index)) => pa_context_get_source_output_info(
                    self.context.as_ptr(),
                    index,
                    loop_capture_callback,
                    reply.userdata(),
                ),
                (true, None) => pa_context_get_source_output_info_list(
                    self.context.as_ptr(),
                    loop_capture_callback,
                    reply.userdata(),
                ),
                (false, Some(index)) => pa_context_get_sink_input_info(
                    self.context.as_ptr(),
                    index,
                    loop_playback_callback,
                    reply.userdata(),
                ),
                (false, None) => pa_context_get_sink_input_info_list(
                    self.context.as_ptr(),
                    loop_playback_callback,
                    reply.userdata(),
                ),
            }
        };
        self.wait(raw, &reply.done, Some(shutdown))?;
        reply.finish()
    }
}

/// Lifetime-app output connection, constructed/accessed only by its worker.
pub(crate) struct OutputSubscription {
    connection: Connection,
    shutdown: Arc<AtomicBool>,
}

impl OutputSubscription {
    pub(crate) fn connect(shutdown: Arc<AtomicBool>) -> Result<Self, AudioError> {
        let mut connection = Connection::connect(Some(&shutdown))?;
        connection.subscribe_outputs(&shutdown, false)?;
        connection.notifications.sink_dirty.set(true);
        Ok(Self {
            connection,
            shutdown,
        })
    }

    pub(crate) fn poll_dirty(&mut self) -> Result<bool, AudioError> {
        self.connection.drain()?;
        Ok(self.connection.notifications.sink_dirty.replace(false))
    }

    pub(crate) fn take_removed(&self) -> BTreeSet<u32> {
        std::mem::take(&mut *self.connection.notifications.removed_sinks.borrow_mut())
    }

    pub(crate) fn snapshot(&mut self, revision: u64) -> Result<SinkCatalog, AudioError> {
        let catalog = self.connection.sink_catalog(&self.shutdown, revision)?;
        self.connection.drain()?;
        if self.connection.notifications.sink_dirty.get() {
            return Err(AudioError::Control(
                "output catalog changed during snapshot".into(),
            ));
        }
        Ok(catalog)
    }
}

fn checked_loopback_lifetime(
    cancelled: bool,
    source_removed: bool,
    owned_removed: bool,
) -> Result<(), AudioError> {
    if cancelled {
        return Err(AudioError::Cancelled);
    }
    if source_removed || owned_removed {
        return Err(AudioError::RecordingLost {
            detail: "terminal source/owned-stream removal delivered during control lookup".into(),
        });
    }
    Ok(())
}

/// One owner-local public PA connection. It never reconnects or restores indices.
pub(crate) struct LoopbackPulse {
    connection: Connection,
    shutdown: Arc<AtomicBool>,
}

impl LoopbackPulse {
    pub(crate) fn connect(
        shutdown: Arc<AtomicBool>,
        owner_cancel: Arc<AtomicBool>,
    ) -> Result<Self, AudioError> {
        let mut connection = Connection::connect_with(Some(&shutdown), Some(owner_cancel))?;
        connection.notifications.inventory.set(true);
        connection.notifications.selected_index.set(INVALID_INDEX);
        connection.subscribe_outputs(&shutdown, true)?;
        Ok(Self {
            connection,
            shutdown,
        })
    }

    pub(crate) fn deadline(&mut self, deadline: Instant) {
        self.connection.operation_deadline = Some(deadline);
    }
    pub(crate) fn clear_deadline(&mut self) {
        self.connection.operation_deadline = None;
    }

    pub(crate) fn drain(&mut self) -> Result<(), AudioError> {
        self.connection.drain()
    }
    pub(crate) fn take_dirty(&self) -> bool {
        let n = &self.connection.notifications;
        n.source_dirty.replace(false)
            | n.sink_dirty.replace(false)
            | n.capture_dirty.replace(false)
            | n.playback_dirty.replace(false)
    }
    pub(crate) fn source_removed(&self) -> bool {
        self.connection.notifications.selected_removed.get()
    }
    pub(crate) fn owned_removed(&self) -> bool {
        self.connection.notifications.loopback_removed.get()
    }
    pub(crate) fn check_lifetime(&self) -> Result<(), AudioError> {
        checked_loopback_lifetime(
            self.shutdown.load(Ordering::Acquire)
                || self
                    .connection
                    .external_cancel
                    .as_ref()
                    .is_some_and(|flag| flag.load(Ordering::Acquire)),
            self.source_removed(),
            self.owned_removed(),
        )
    }
    pub(crate) fn sink_removed(&self, index: u32) -> bool {
        self.connection
            .notifications
            .removed_sinks
            .borrow()
            .contains(&index)
    }
    pub(crate) fn consume_sink_removals(&self) {
        self.connection
            .notifications
            .removed_sinks
            .borrow_mut()
            .clear();
    }

    pub(crate) fn source(
        &mut self,
        selected: &AudioSourceIdentity,
    ) -> Result<super::super::audio::NativeSourceSnapshot, AudioError> {
        self.connection.drain()?;
        if self.source_removed() {
            return Err(AudioError::SourceMissing {
                name: selected.name().into(),
            });
        }
        let rows = self.connection.sources(Some(&self.shutdown))?;
        let mapping = checked_native_source(selected, &rows)?;
        let old = self.connection.notifications.selected_index.get();
        if old != INVALID_INDEX && old != mapping.pulse_index {
            return Err(AudioError::SourceChanged {
                name: selected.name().into(),
            });
        }
        self.connection
            .notifications
            .selected_index
            .set(mapping.pulse_index);
        self.connection.notifications.inventory.set(false);
        self.connection.drain()?;
        if self.source_removed() {
            return Err(AudioError::SourceMissing {
                name: selected.name().into(),
            });
        }
        Ok(mapping)
    }

    pub(crate) fn sinks(&mut self) -> Result<Vec<SinkObservation>, AudioError> {
        self.connection.sinks(&self.shutdown)
    }

    pub(crate) fn streams(
        &mut self,
        capture: bool,
        index: Option<u32>,
    ) -> Result<Vec<LoopStream>, AudioError> {
        self.connection.loop_streams(capture, index, &self.shutdown)
    }

    pub(crate) fn bind_streams(&self, capture: u32, playback: u32) {
        self.connection
            .notifications
            .loopback_capture
            .set(Some(capture));
        self.connection
            .notifications
            .loopback_playback
            .set(Some(playback));
    }
    fn check_control(&self) -> Result<(), AudioError> {
        if self.shutdown.load(Ordering::Acquire)
            || self
                .connection
                .external_cancel
                .as_ref()
                .is_some_and(|flag| flag.load(Ordering::Acquire))
        {
            return Err(AudioError::Cancelled);
        }
        if self
            .connection
            .operation_deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            return Err(AudioError::Control(
                "owned playback operation deadline expired".into(),
            ));
        }
        if !self.connection.ready() {
            return Err(self.connection.error("owned playback control"));
        }
        Ok(())
    }

    pub(crate) fn mute(&mut self, index: u32, muted: bool) -> Result<(), AudioError> {
        self.mute_admitted(index, muted, || Some(())).map(|_| ())
    }

    /// The guard is retained only through async request enqueue, never through
    /// a PA mainloop wait. A superseded intent queues no unmute at all.
    pub(crate) fn mute_admitted<G>(
        &mut self,
        index: u32,
        muted: bool,
        admission: impl FnOnce() -> Option<G>,
    ) -> Result<bool, AudioError> {
        self.check_control()?;
        let reply = Success::default();
        let Some(guard) = admission() else {
            return Ok(false);
        };
        // SAFETY: Caller freshly attributed the stream; the admission guard
        // prevents a newer intent being accepted between authorization/enqueue.
        let raw = unsafe {
            pa_context_set_sink_input_mute(
                self.connection.context.as_ptr(),
                index,
                i32::from(muted),
                success_callback,
                reply.userdata(),
            )
        };
        drop(guard);
        self.connection
            .wait(raw, &reply.done, Some(&self.shutdown))?;
        reply.finish()?;
        Ok(true)
    }

    pub(crate) fn volume(
        &mut self,
        index: u32,
        channels: usize,
        value: u32,
    ) -> Result<(), AudioError> {
        self.check_control()?;
        let channels = u8::try_from(channels)
            .ok()
            .filter(|channels| (1..=32).contains(channels))
            .ok_or_else(|| AudioError::Control("invalid owned playback channels".into()))?;
        let volume = Volume {
            channels,
            values: [value; 32],
        };
        let reply = Success::default();
        // SAFETY: Public cvolume, borrowed only while queueing; it is copied by
        // libpulse. Caller has freshly attributed the playback stream.
        let raw = unsafe {
            pa_context_set_sink_input_volume(
                self.connection.context.as_ptr(),
                index,
                &volume,
                success_callback,
                reply.userdata(),
            )
        };
        self.connection
            .wait(raw, &reply.done, Some(&self.shutdown))?;
        reply.finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_subscription_preserves_output_and_exact_loopback_capture_lifecycle() {
        let notifications = Notifications::default();
        notifications.loopback_capture.set(Some(8));
        let userdata = ptr::from_ref(&notifications).cast_mut().cast();
        // SAFETY: Live local userdata; this callback never dereferences context.
        unsafe {
            subscription_callback(
                ptr::null_mut(),
                SOURCE_OUTPUT_EVENT | REMOVE_EVENT,
                9,
                userdata,
            );
            subscription_callback(ptr::null_mut(), SOURCE_OUTPUT_EVENT | 0x10, 9, userdata);
            subscription_callback(ptr::null_mut(), SINK_EVENT | REMOVE_EVENT, 12, userdata);
            subscription_callback(ptr::null_mut(), SERVER_EVENT, 0, userdata);
            subscription_callback(
                ptr::null_mut(),
                SINK_INPUT_EVENT | REMOVE_EVENT,
                13,
                userdata,
            );
        }
        assert!(!notifications.loopback_removed.get());
        assert!(!notifications.capture_dirty.get());
        assert!(notifications.sink_dirty.get());
        assert!(notifications.playback_dirty.get());
        assert_eq!(*notifications.removed_sinks.borrow(), BTreeSet::from([12]));
        // An exact owned capture CHANGE must leave the clean owner fast path.
        // SAFETY: Same live callback userdata; no native API call is performed.
        unsafe {
            subscription_callback(ptr::null_mut(), SOURCE_OUTPUT_EVENT | 0x10, 8, userdata);
        }
        assert!(notifications.capture_dirty.replace(false));
        let source = crate::capture::audio::NativeSourceSnapshot {
            identity: AudioSourceIdentity::new("capture".into(), vec![]).unwrap(),
            serial: 71,
            pulse_index: 55,
            observation_revision: 1,
            rate: 48000,
            channel_positions: vec![1, 2],
        };
        let mut capture = LoopStream {
            index: 8,
            serial: Some(72),
            global_id: Some(73),
            node: Some("owned-capture".into()),
            app: None,
            media: None,
            pid: None,
            target: 55,
            volumes: vec![],
            muted: false,
            volume_writable: false,
        };
        assert!(crate::media::loopback::check_capture_target(&source, &capture).is_ok());
        capture.target = 56;
        assert!(matches!(
            crate::media::loopback::check_capture_target(&source, &capture),
            Err(AudioError::SourceChanged { .. })
        ));
        // SAFETY: Same live userdata; only the exact owned capture removal latches.
        unsafe {
            subscription_callback(
                ptr::null_mut(),
                SOURCE_OUTPUT_EVENT | REMOVE_EVENT,
                8,
                userdata,
            );
            subscription_callback(ptr::null_mut(), SOURCE_OUTPUT_EVENT, 8, userdata);
        }
        assert!(notifications.loopback_removed.get());
        assert!(notifications.capture_dirty.replace(false));
    }

    #[test]
    fn copied_owned_row_then_removal_during_lookup_denies_writer_launch_precondition() {
        let notifications = Notifications::default();
        notifications.loopback_playback.set(Some(8));
        let reply = Reply::<u32>::default();
        reply.row(Ok(Some(55))); // The info callback already copied object.id.
        assert!(
            checked_loopback_lifetime(false, false, notifications.loopback_removed.get()).is_ok()
        );
        // SAFETY: Actual subscription callback touches only this live stack
        // Notifications; its ignored context is not dereferenced.
        unsafe {
            subscription_callback(
                ptr::null_mut(),
                SINK_INPUT_EVENT | REMOVE_EVENT,
                8,
                ptr::from_ref(&notifications).cast_mut().cast(),
            );
        }
        assert_eq!(reply.finish().unwrap(), vec![55]);
        // The copied row still exists, but the SAME production guard used at
        // post-query/spawn rejects it before any metadata command can launch.
        assert!(matches!(
            checked_loopback_lifetime(
                false,
                notifications.selected_removed.get(),
                notifications.loopback_removed.get()
            ),
            Err(AudioError::RecordingLost { .. })
        ));
        assert_eq!(
            checked_loopback_lifetime(true, false, false),
            Err(AudioError::Cancelled)
        );
        assert!(checked_loopback_lifetime(false, true, false).is_err());
    }

    #[test]
    fn missing_lookup_is_allowed_only_for_targeted_live_empty_result() {
        assert!(missing_lookup(true, true, true, NO_ENTITY));
        assert!(!missing_lookup(false, true, true, NO_ENTITY));
        assert!(!missing_lookup(true, false, true, NO_ENTITY));
        assert!(!missing_lookup(true, true, false, NO_ENTITY));
        assert!(!missing_lookup(true, true, true, 1));
    }

    #[test]
    fn independent_open_revalidates_identity_and_uses_fresh_ephemeral_source_index() {
        let selected = AudioSourceIdentity::new(
            "capture".into(),
            vec![("device.serial".into(), "serial".into())],
        )
        .unwrap();
        let observed = |index| ObservedSource {
            source: AudioSource {
                identity: selected.clone(),
                description: "Capture".into(),
            },
            index,
            native_serial: Some(71),
            rate: 48000,
            channels: 2,
            channel_positions: std::array::from_fn(|index| if index == 0 { 1 } else { 2 }),
        };

        let native = checked_native_source(&selected, &[observed(7)]).unwrap();
        assert_eq!(native.serial, 71);
        let mut absent_serial = observed(7);
        absent_serial.native_serial = None;
        assert!(checked_native_source(&selected, &[absent_serial]).is_err());
        let mut other_serial = observed(8);
        other_serial.native_serial = Some(72);
        let returned = checked_native_source(&selected, &[other_serial]).unwrap();
        assert_ne!(native.serial, returned.serial);
        assert!(checked_native_source(&selected, &[]).is_err());
        assert!(checked_native_source(&selected, &[observed(7), observed(8)]).is_err());
        let mut duplicate_serial = observed(9);
        duplicate_serial.source.identity =
            AudioSourceIdentity::new("unselected-microphone".into(), vec![]).unwrap();
        assert!(checked_native_source(&selected, &[observed(7), duplicate_serial]).is_err());
        let mut changed_identity = observed(7);
        changed_identity.source.identity = AudioSourceIdentity::new(
            "capture".into(),
            vec![("device.serial".into(), "replacement".into())],
        )
        .unwrap();
        assert!(matches!(
            checked_native_source(&selected, &[changed_identity]),
            Err(AudioError::SourceChanged { .. })
        ));
    }

    #[test]
    fn sink_monitors_never_enter_capture_inventory() {
        assert!(!source_is_capture(0));
        assert!(!source_is_capture(17));
        assert!(source_is_capture(INVALID_INDEX));
    }

    #[test]
    fn source_removal_is_latched_but_state_and_foreign_changes_are_not_loss() {
        let notifications = Notifications::default();
        notifications.selected_index.set(7);
        let userdata = ptr::from_ref(&notifications).cast_mut().cast();
        // SAFETY: Local live callback userdata, no native API or hardware access.
        unsafe {
            subscription_callback(ptr::null_mut(), SOURCE_EVENT | 0x10, 7, userdata);
            subscription_callback(ptr::null_mut(), SOURCE_EVENT | REMOVE_EVENT, 9, userdata);
        }
        assert!(notifications.source_dirty.get());
        assert!(!notifications.selected_removed.get());
        // SAFETY: Same live callback-scoped test userdata.
        unsafe { subscription_callback(ptr::null_mut(), SOURCE_EVENT | REMOVE_EVENT, 7, userdata) };
        assert!(notifications.selected_removed.get());
        // SAFETY: A new source cannot undo the old generation's selected loss.
        unsafe { subscription_callback(ptr::null_mut(), SOURCE_EVENT, 7, userdata) };
        assert!(notifications.selected_removed.get());
    }
}
