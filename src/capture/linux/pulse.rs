//! Private libpulse ABI and independent, fail-closed recording cancellation.

use std::{
    cell::{Cell, RefCell},
    ffi::{CStr, c_char, c_int, c_void},
    fs::File,
    io::Read,
    ptr::{self, NonNull},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, Sender},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use crate::{
    capture::audio::{AudioEvent, AudioSource},
    domain::capture::{AudioError, AudioSourceIdentity},
};

const INVALID_INDEX: u32 = u32::MAX;
const READY: c_int = 4;
const FAILED: c_int = 5;
const TERMINATED: c_int = 6;
const NO_ENTITY: c_int = 5;
const SOURCE_EVENT: c_int = 1;
const SINK_INPUT_EVENT: c_int = 2;
const SOURCE_OUTPUT_EVENT: c_int = 3;
const CLIENT_EVENT: c_int = 5;
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
struct ClientInfo {
    index: u32,
    name: *const c_char,
    owner_module: u32,
    driver: *const c_char,
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
}

type SourceCallback = unsafe extern "C" fn(*mut c_void, *const SourceInfo, c_int, *mut c_void);
type ClientCallback = unsafe extern "C" fn(*mut c_void, *const ClientInfo, c_int, *mut c_void);
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
    fn pa_context_get_client_info_list(
        context: *mut c_void,
        callback: ClientCallback,
        userdata: *mut c_void,
    ) -> *mut c_void;
    fn pa_context_get_client_info(
        context: *mut c_void,
        index: u32,
        callback: ClientCallback,
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
    fn pa_context_kill_source_output(
        context: *mut c_void,
        index: u32,
        callback: SuccessCallback,
        userdata: *mut c_void,
    ) -> *mut c_void;
    fn pa_context_kill_client(
        context: *mut c_void,
        index: u32,
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
        // freed. It does not promise to undo a server-side kill.
        unsafe {
            pa_operation_cancel(self.0.as_ptr());
            pa_operation_unref(self.0.as_ptr());
        }
    }
}

#[derive(Default)]
struct Notifications {
    source_dirty: Cell<bool>,
    recording_dirty: Cell<bool>,
    selected_index: Cell<u32>,
    selected_removed: Cell<bool>,
    owned_output: Cell<Option<u32>>,
    owned_client: Cell<Option<u32>>,
    owned_removed: Cell<Option<&'static str>>,
}

struct Connection {
    mainloop: NonNull<c_void>,
    context: NonNull<c_void>,
    notifications: Box<Notifications>,
}

impl Connection {
    fn connect(shutdown: Option<&AtomicBool>) -> Result<Self, AudioError> {
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
        if shutdown.is_some_and(|flag| flag.load(Ordering::Acquire)) {
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

    fn wait(
        &mut self,
        raw: *mut c_void,
        done: &Cell<bool>,
        shutdown: Option<&AtomicBool>,
    ) -> Result<(), AudioError> {
        let operation = Operation(NonNull::new(raw).ok_or_else(|| self.error("queue operation"))?);
        let deadline = Instant::now() + CONTROL_TIMEOUT;
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

    fn clients(
        &mut self,
        index: Option<u32>,
        shutdown: &AtomicBool,
    ) -> Result<Vec<Client>, AudioError> {
        let reply = Reply::<Client>::default();
        reply.allow_missing.set(index.is_some());
        // SAFETY: Same bounded callback lifetime as sources().
        let raw = unsafe {
            match index {
                Some(index) => pa_context_get_client_info(
                    self.context.as_ptr(),
                    index,
                    client_callback,
                    reply.userdata(),
                ),
                None => pa_context_get_client_info_list(
                    self.context.as_ptr(),
                    client_callback,
                    reply.userdata(),
                ),
            }
        };
        self.wait(raw, &reply.done, Some(shutdown))?;
        reply.finish()
    }

    fn outputs(
        &mut self,
        index: Option<u32>,
        shutdown: &AtomicBool,
    ) -> Result<Vec<SourceOutput>, AudioError> {
        let reply = Reply::<SourceOutput>::default();
        reply.allow_missing.set(index.is_some());
        // SAFETY: Same bounded callback lifetime as sources().
        let raw = unsafe {
            match index {
                Some(index) => pa_context_get_source_output_info(
                    self.context.as_ptr(),
                    index,
                    output_callback,
                    reply.userdata(),
                ),
                None => pa_context_get_source_output_info_list(
                    self.context.as_ptr(),
                    output_callback,
                    reply.userdata(),
                ),
            }
        };
        self.wait(raw, &reply.done, Some(shutdown))?;
        reply.finish()
    }

    fn playback_clients(&mut self, shutdown: &AtomicBool) -> Result<Vec<u32>, AudioError> {
        let reply = Reply::<u32>::default();
        // SAFETY: Same bounded callback lifetime as sources().
        let raw = unsafe {
            pa_context_get_sink_input_info_list(
                self.context.as_ptr(),
                sink_input_callback,
                reply.userdata(),
            )
        };
        self.wait(raw, &reply.done, Some(shutdown))?;
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
        // SOURCE + SINK_INPUT + SOURCE_OUTPUT + CLIENT before any audio-add.
        // SAFETY: Callback lifetime bounded by wait, as above.
        let raw = unsafe {
            pa_context_subscribe(
                self.context.as_ptr(),
                0x02 | 0x04 | 0x08 | 0x20,
                success_callback,
                reply.userdata(),
            )
        };
        self.wait(raw, &reply.done, Some(shutdown))?;
        reply.finish().map_err(|_| self.error("subscribe"))
    }

    fn kill(
        &mut self,
        target: &Target,
        shutdown: &AtomicBool,
    ) -> Result<CancelOutcome, AudioError> {
        if !self.ready() || shutdown.load(Ordering::Acquire) {
            return Err(AudioError::Control(
                "control connection or guard expired".into(),
            ));
        }
        let reply = Success::default();
        // SAFETY: Only fresh, exact attributed index from this unchanged live
        // connection reaches this operation. No source/mixer/default API exists.
        let raw = unsafe {
            match target {
                Target::Output(output, _) => pa_context_kill_source_output(
                    self.context.as_ptr(),
                    output.index,
                    success_callback,
                    reply.userdata(),
                ),
                Target::Client(client) => pa_context_kill_client(
                    self.context.as_ptr(),
                    client.index,
                    success_callback,
                    reply.userdata(),
                ),
            }
        };
        self.wait(raw, &reply.done, Some(shutdown))?;
        cancel_reply(reply.succeeded.get(), reply.error_code.get(), self.ready())
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
    error_code: Cell<Option<c_int>>,
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

unsafe extern "C" fn success_callback(context: *mut c_void, success: c_int, userdata: *mut c_void) {
    // SAFETY: Success remains live until operation cancelled/unreferenced.
    let reply = unsafe { &*userdata.cast::<Success>() };
    reply.succeeded.set(success != 0);
    if success == 0 {
        // SAFETY: Capture this operation's callback error while context is live.
        reply
            .error_code
            .set(Some(unsafe { pa_context_errno(context) }));
    }
    reply.done.set(true);
}

fn missing_lookup(targeted: bool, empty: bool, ready: bool, code: c_int) -> bool {
    targeted && empty && ready && code == NO_ENTITY
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CancelOutcome {
    Acknowledged,
    StillArmed,
}

fn cancel_reply(
    success: bool,
    code: Option<c_int>,
    ready: bool,
) -> Result<CancelOutcome, AudioError> {
    if !ready {
        return Err(AudioError::Control(
            "control connection lost during cancellation".into(),
        ));
    }
    if success {
        Ok(CancelOutcome::Acknowledged)
    } else if code == Some(NO_ENTITY) {
        // Native destroy may have removed the exact target before kill arrived.
        // This is not a kill ACK or native-destroy proof. Remain armed.
        Ok(CancelOutcome::StillArmed)
    } else {
        Err(AudioError::Control(format!(
            "kill rejected with Pulse error {code:?}"
        )))
    }
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
        SOURCE_EVENT => {
            if index == notifications.selected_index.get() {
                notifications.source_dirty.set(true);
            }
            if index == notifications.selected_index.get() && event & TYPE_MASK == REMOVE_EVENT {
                notifications.selected_removed.set(true);
            }
        }
        SOURCE_OUTPUT_EVENT | CLIENT_EVENT => {
            notifications.recording_dirty.set(true);
            if event & TYPE_MASK == REMOVE_EVENT && notifications.owned_removed.get().is_none() {
                let owned = match event & FACILITY_MASK {
                    SOURCE_OUTPUT_EVENT => notifications.owned_output.get() == Some(index),
                    CLIENT_EVENT => notifications.owned_client.get() == Some(index),
                    _ => false,
                };
                if owned {
                    notifications.owned_removed.set(Some(
                        if event & FACILITY_MASK == CLIENT_EVENT {
                            "Pulse client"
                        } else {
                            "source-output"
                        },
                    ));
                }
            }
        }
        SINK_INPUT_EVENT => notifications.recording_dirty.set(true),
        _ => {}
    }
}

fn owned_recording_loss(notifications: &Notifications, stopped: bool) -> Option<AudioError> {
    if stopped {
        None
    } else {
        notifications
            .owned_removed
            .get()
            .map(|kind| AudioError::RecordingLost {
                detail: format!("positively attributed {kind} removed"),
            })
    }
}

fn latch_missing_owned(
    notifications: &Notifications,
    clients: &[Client],
    outputs: &[SourceOutput],
) {
    if notifications.owned_removed.get().is_some() {
        return;
    }
    if notifications
        .owned_client
        .get()
        .is_some_and(|index| !clients.iter().any(|client| client.index == index))
    {
        notifications.owned_removed.set(Some("Pulse client"));
    } else if notifications
        .owned_output
        .get()
        .is_some_and(|index| !outputs.iter().any(|output| output.index == index))
    {
        notifications.owned_removed.set(Some("source-output"));
    }
}

fn cache_owned_recording(notifications: &Notifications, target: &Target) {
    match target {
        Target::Output(output, client) => {
            notifications.owned_output.set(Some(output.index));
            notifications.owned_client.set(Some(client.index));
        }
        Target::Client(client) => notifications.owned_client.set(Some(client.index)),
    }
}

fn refresh_owned_recording(
    connection: &mut Connection,
    tag: &str,
    pid: &str,
    opening: &AtomicBool,
    shutdown: &AtomicBool,
) -> Result<(), AudioError> {
    let clients = connection.clients(None, shutdown)?;
    let outputs = connection.outputs(None, shutdown)?;
    // A removal can arrive while initial introspection is completing, before
    // cached indices exist. A subsequent complete inventory proves absence of
    // previously attributed objects without inventing a physical-source error.
    latch_missing_owned(&connection.notifications, &clients, &outputs);
    // Partial/foreign/ambiguous attribution does not establish an owned record
    // and cannot be promoted into source loss. Cancellation separately rejects
    // it; an actual inventory/connection failure remains an owned error result.
    match choose_target(
        tag,
        pid,
        &clients,
        &outputs,
        opening.load(Ordering::Acquire),
    ) {
        Ok(Some(target @ Target::Output(_, _))) => {
            cache_owned_recording(&connection.notifications, &target);
        }
        Ok(Some(Target::Client(client))) => {
            if recording_only_client(&client, &connection.playback_clients(shutdown)?).is_ok() {
                cache_owned_recording(&connection.notifications, &Target::Client(client));
            }
        }
        Ok(None) | Err(_) => {}
    }
    Ok(())
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

#[derive(Clone)]
struct ObservedSource {
    source: AudioSource,
    index: u32,
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
        }))
    })();
    reply.row(row);
}

fn source_is_capture(monitor_of_sink: u32) -> bool {
    monitor_of_sink == INVALID_INDEX
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Client {
    index: u32,
    name: String,
    app: Option<String>,
    pid: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SourceOutput {
    index: u32,
    client: u32,
    name: String,
    app: Option<String>,
    media: Option<String>,
    pid: Option<String>,
}

unsafe extern "C" fn client_callback(
    context: *mut c_void,
    info: *const ClientInfo,
    eol: c_int,
    userdata: *mut c_void,
) {
    // SAFETY: Matching Reply type, bounded operation lifetime.
    let reply = unsafe { &*userdata.cast::<Reply<Client>>() };
    if eol != 0 {
        if eol < 0 {
            unsafe { reply.end_error(context, "client inventory") };
        }
        reply.done.set(true);
        return;
    }
    if info.is_null() {
        reply.row(Err(AudioError::Unavailable(
            "null client inventory row".into(),
        )));
        return;
    }
    // SAFETY: Copy every borrowed identity before callback ends.
    let info = unsafe { &*info };
    reply.row((|| {
        Ok(Some(Client {
            index: info.index,
            name: unsafe { copied_string(info.name) }?.unwrap_or_default(),
            app: unsafe { property(info.proplist, c"application.name") }?,
            pid: unsafe { property(info.proplist, c"application.process.id") }?,
        }))
    })());
}

unsafe extern "C" fn output_callback(
    context: *mut c_void,
    info: *const SourceOutputInfo,
    eol: c_int,
    userdata: *mut c_void,
) {
    // SAFETY: Matching Reply type, bounded operation lifetime.
    let reply = unsafe { &*userdata.cast::<Reply<SourceOutput>>() };
    if eol != 0 {
        if eol < 0 {
            unsafe { reply.end_error(context, "source-output inventory") };
        }
        reply.done.set(true);
        return;
    }
    if info.is_null() {
        reply.row(Err(AudioError::Unavailable(
            "null source-output inventory row".into(),
        )));
        return;
    }
    // SAFETY: Copy every borrowed identity before callback ends.
    let info = unsafe { &*info };
    reply.row((|| {
        Ok(Some(SourceOutput {
            index: info.index,
            client: info.client,
            name: unsafe { copied_string(info.name) }?.unwrap_or_default(),
            app: unsafe { property(info.proplist, c"application.name") }?,
            media: unsafe { property(info.proplist, c"media.name") }?,
            pid: unsafe { property(info.proplist, c"application.process.id") }?,
        }))
    })());
}

unsafe extern "C" fn sink_input_callback(
    context: *mut c_void,
    info: *const SinkInputInfo,
    eol: c_int,
    userdata: *mut c_void,
) {
    // SAFETY: Matching Reply type and bounded operation lifetime.
    let reply = unsafe { &*userdata.cast::<Reply<u32>>() };
    if eol != 0 {
        if eol < 0 {
            reply.row(Err(unsafe { context_error(context, "playback inventory") }));
        }
        reply.done.set(true);
    } else if info.is_null() {
        reply.row(Err(AudioError::Unavailable(
            "null playback inventory row".into(),
        )));
    } else {
        // SAFETY: Documented sink-input prefix valid during callback.
        reply.row(Ok(Some(unsafe { (*info).client })));
    }
}

fn recording_only_client(client: &Client, playback_clients: &[u32]) -> Result<(), AudioError> {
    if playback_clients.contains(&client.index) {
        Err(AudioError::Control(
            "dedicated capture client owns a playback stream".into(),
        ))
    } else {
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Target {
    Output(SourceOutput, Client),
    Client(Client),
}

fn choose_target(
    tag: &str,
    pid: &str,
    clients: &[Client],
    outputs: &[SourceOutput],
    opening: bool,
) -> Result<Option<Target>, AudioError> {
    // Any partial claim of this unique tag must be explained before a kill.
    let mut related_clients = clients
        .iter()
        .filter(|client| client.name == tag || client.app.as_deref() == Some(tag));
    let client = related_clients.next();
    if related_clients.next().is_some() {
        return Err(AudioError::Ambiguous(
            "multiple clients claim per-opening tag".into(),
        ));
    }
    let mut related_outputs = outputs.iter().filter(|output| {
        output.name == tag
            || output.app.as_deref() == Some(tag)
            || output.media.as_deref() == Some(tag)
            || client.is_some_and(|client| client.index == output.client)
    });
    let output = related_outputs.next();
    if related_outputs.next().is_some() {
        return Err(AudioError::Ambiguous(
            "multiple streams claim per-opening tag".into(),
        ));
    }
    let Some(client) = client else {
        return if output.is_none() {
            Ok(None)
        } else {
            Err(AudioError::Control(
                "tagged stream has no attributable client".into(),
            ))
        };
    };
    if client.index == INVALID_INDEX
        || client.name != tag
        || client.app.as_deref() != Some(tag)
        || client.pid.as_deref() != Some(pid)
    {
        return Err(AudioError::Control(
            "client ownership does not match tag and PID".into(),
        ));
    }
    if let Some(output) = output {
        if output.index == INVALID_INDEX
            || output.client != client.index
            || output.name != tag
            || output.app.as_deref() != Some(tag)
            || output.media.as_deref() != Some(tag)
            || output.pid.as_deref() != Some(pid)
        {
            return Err(AudioError::Control(
                "stream ownership does not match tag, PID and client".into(),
            ));
        }
        // Routed source can be EasyEffects; requested-source index is not an
        // ownership condition. Kill only this uniquely attributed source-output.
        return Ok(Some(Target::Output(output.clone(), client.clone())));
    }
    Ok(opening.then(|| Target::Client(client.clone())))
}

fn same_output(previous: &SourceOutput, fresh: &SourceOutput) -> bool {
    previous == fresh
}

pub(crate) fn discover() -> Result<Vec<AudioSource>, AudioError> {
    let mut connection = Connection::connect(None)?;
    Ok(connection
        .sources(None)?
        .into_iter()
        .map(|row| row.source)
        .collect())
}

pub(crate) struct GuardWorker {
    tag: String,
    opening: Arc<AtomicBool>,
    shutdown: Arc<AtomicBool>,
    events: Receiver<AudioEvent>,
    thread: Option<JoinHandle<Result<(), AudioError>>>,
}

impl GuardWorker {
    pub(crate) fn start(
        source: AudioSourceIdentity,
        generation: u64,
        cancel: Arc<AtomicBool>,
    ) -> Result<Self, AudioError> {
        if cancel.load(Ordering::Acquire) {
            return Err(AudioError::Cancelled);
        }
        let mut nonce = [0u8; 16];
        File::open("/dev/urandom")
            .and_then(|mut file| file.read_exact(&mut nonce))
            .map_err(|error| AudioError::Unavailable(format!("opening tag entropy: {error}")))?;
        let tag = format!(
            "Furami-{}-{generation}-{:032x}",
            std::process::id(),
            u128::from_ne_bytes(nonce)
        );
        let worker_tag = tag.clone();
        let opening = Arc::new(AtomicBool::new(true));
        let shutdown = Arc::new(AtomicBool::new(false));
        let (events_tx, events) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let worker_opening = Arc::clone(&opening);
        let worker_shutdown = Arc::clone(&shutdown);
        let thread = thread::Builder::new()
            .name(format!("audio-guard-{generation}"))
            .spawn(move || {
                let initialized = (|| {
                    let mut connection = Connection::connect(Some(&cancel))?;
                    let sources = connection.sources(Some(&cancel))?;
                    let selected = validated_source_index(&source, &sources)?;
                    connection.subscribe(selected, &cancel)?;
                    // Recheck after subscription ACK closes enumeration/subscribe
                    // loss gap. Subsequent exact removals remain latched.
                    validated_source_index(&source, &connection.sources(Some(&cancel))?).and_then(
                        |index| {
                            if index != selected || connection.notifications.selected_removed.get()
                            {
                                Err(AudioError::SourceMissing {
                                    name: source.name().into(),
                                })
                            } else {
                                Ok(())
                            }
                        },
                    )?;
                    Ok::<_, AudioError>(connection)
                })();
                let mut connection = match initialized {
                    Ok(connection) => {
                        if ready_tx.send(Ok(())).is_err() {
                            return Ok(());
                        }
                        connection
                    }
                    Err(error) => {
                        let _ = ready_tx.send(Err(error.clone()));
                        return Err(error);
                    }
                };
                run_guard(
                    &mut connection,
                    &source,
                    &worker_tag,
                    &cancel,
                    &worker_opening,
                    &worker_shutdown,
                    &events_tx,
                )
            })
            .map_err(|error| AudioError::Unavailable(format!("audio guard thread: {error}")))?;
        let mut worker = Self {
            tag,
            opening,
            shutdown,
            events,
            thread: Some(thread),
        };
        match ready_rx.recv() {
            Ok(Ok(())) => Ok(worker),
            Ok(Err(error)) => {
                let _ = worker.join();
                Err(error)
            }
            Err(_) => {
                let _ = worker.join();
                Err(AudioError::Unavailable(
                    "audio guard initialization ended without result".into(),
                ))
            }
        }
    }

    pub(crate) fn tag(&self) -> &str {
        &self.tag
    }

    pub(crate) fn mark_open_complete(&self) {
        self.opening.store(false, Ordering::Release);
    }

    pub(crate) fn poll_event(&self) -> Option<AudioEvent> {
        self.events.try_recv().ok()
    }

    fn join(&mut self) -> Result<(), AudioError> {
        self.shutdown.store(true, Ordering::Release);
        match self.thread.take() {
            Some(thread) => thread
                .join()
                .map_err(|_| AudioError::Control("audio guard thread panicked".into()))?,
            None => Ok(()),
        }
    }

    pub(crate) fn quiesce(mut self) -> Result<(), AudioError> {
        self.join()
    }
}

impl Drop for GuardWorker {
    fn drop(&mut self) {
        // Normal caller explicitly quiesces after mpv destruction. Error/drop
        // paths still join rather than leaking control context or worker.
        let _ = self.join();
    }
}

fn validated_source_index(
    source: &AudioSourceIdentity,
    rows: &[ObservedSource],
) -> Result<u32, AudioError> {
    let mut matches = rows
        .iter()
        .filter(|row| row.source.identity.name() == source.name());
    let selected = matches.next().ok_or_else(|| AudioError::SourceMissing {
        name: source.name().into(),
    })?;
    if matches.next().is_some() {
        return Err(AudioError::Ambiguous(format!(
            "multiple sources named `{}`",
            source.name()
        )));
    }
    if !source.compatible_with(&selected.source.identity) {
        return Err(AudioError::SourceChanged {
            name: source.name().into(),
        });
    }
    Ok(selected.index)
}

fn fresh_target(
    connection: &mut Connection,
    target: &Target,
    tag: &str,
    pid: &str,
    opening: &AtomicBool,
    shutdown: &AtomicBool,
) -> Result<Option<Target>, AudioError> {
    let client = match target {
        Target::Output(_, client) | Target::Client(client) => client,
    };
    let clients = connection.clients(Some(client.index), shutdown)?;
    if clients.is_empty() {
        return Ok(None);
    }
    match target {
        Target::Output(previous, _) => {
            let outputs = connection.outputs(Some(previous.index), shutdown)?;
            checked_cancel_target(target, &clients, &outputs, tag, pid, false)
        }
        Target::Client(_) => {
            recording_only_client(client, &connection.playback_clients(shutdown)?)?;
            // New output can appear after first inventory: prefer narrower kill
            // if now attributable. Otherwise pending opening alone authorizes
            // dedicated client kill; Active never does.
            let outputs = connection.outputs(None, shutdown)?;
            checked_cancel_target(
                target,
                &clients,
                &outputs,
                tag,
                pid,
                opening.load(Ordering::Acquire),
            )
        }
    }
}

fn checked_cancel_target(
    previous: &Target,
    clients: &[Client],
    outputs: &[SourceOutput],
    tag: &str,
    pid: &str,
    opening: bool,
) -> Result<Option<Target>, AudioError> {
    let client = match previous {
        Target::Output(_, client) | Target::Client(client) => client,
    };
    if clients.is_empty() {
        return Ok(None);
    }
    if clients.len() != 1 || &clients[0] != client {
        return Err(AudioError::Control(
            "client changed before cancellation".into(),
        ));
    }
    if let Target::Output(output, _) = previous {
        if outputs.is_empty() {
            return Ok(None);
        }
        if outputs.len() != 1 || !same_output(output, &outputs[0]) {
            return Err(AudioError::Control(
                "stream changed before cancellation".into(),
            ));
        }
    }
    choose_target(tag, pid, clients, outputs, opening)
}

fn run_guard(
    connection: &mut Connection,
    source: &AudioSourceIdentity,
    tag: &str,
    cancel: &AtomicBool,
    opening: &AtomicBool,
    shutdown: &AtomicBool,
    events: &Sender<AudioEvent>,
) -> Result<(), AudioError> {
    let pid = std::process::id().to_string();
    let mut source_failed = false;
    let mut control_error = None;
    let mut cancelled = false;
    let mut cancel_seen = false;
    while !shutdown.load(Ordering::Acquire) {
        if !connection.ready() {
            let error = connection.error("guard connection lost");
            if cancel.load(Ordering::Acquire) {
                let _ = events.send(AudioEvent::CancelFailed(error.clone()));
            } else if !source_failed {
                let _ = events.send(AudioEvent::SourceLost(error.clone()));
            }
            return Err(error);
        }
        if !cancel.load(Ordering::Acquire)
            && !source_failed
            && (connection.notifications.selected_removed.get()
                || connection.notifications.source_dirty.replace(false))
        {
            let checked = if connection.notifications.selected_removed.get() {
                Err(AudioError::SourceMissing {
                    name: source.name().into(),
                })
            } else {
                connection.sources(Some(shutdown)).and_then(|rows| {
                    let index = validated_source_index(source, &rows)?;
                    if index != connection.notifications.selected_index.get() {
                        Err(AudioError::SourceChanged {
                            name: source.name().into(),
                        })
                    } else {
                        Ok(())
                    }
                })
            };
            if let Err(error) = checked {
                if shutdown.load(Ordering::Acquire) {
                    break;
                }
                if !cancel.load(Ordering::Acquire) {
                    source_failed = true;
                    let _ = events.send(AudioEvent::SourceLost(error));
                }
            }
        }
        let stop = cancel.load(Ordering::Acquire);
        let dirty = connection.notifications.recording_dirty.replace(false);
        if !stop
            && !source_failed
            && dirty
            && let Err(error) = refresh_owned_recording(connection, tag, &pid, opening, shutdown)
        {
            if shutdown.load(Ordering::Acquire) {
                break;
            }
            if !cancel.load(Ordering::Acquire) {
                source_failed = true;
                let _ = events.send(AudioEvent::SourceLost(error));
            }
        }
        if !source_failed
            && let Some(error) =
                owned_recording_loss(&connection.notifications, cancel.load(Ordering::Acquire))
        {
            source_failed = true;
            let _ = events.send(AudioEvent::SourceLost(error));
        }
        if stop && !cancelled && control_error.is_none() && (!cancel_seen || dirty) {
            cancel_seen = true;
            let attempt = (|| {
                let clients = connection.clients(None, shutdown)?;
                let outputs = connection.outputs(None, shutdown)?;
                let Some(target) = choose_target(
                    tag,
                    &pid,
                    &clients,
                    &outputs,
                    opening.load(Ordering::Acquire),
                )?
                else {
                    // Stop-before-open is level-triggered. Subscription continues
                    // waiting for its dedicated client/output until native destroy.
                    return Ok(CancelOutcome::StillArmed);
                };
                let Some(target) = fresh_target(connection, &target, tag, &pid, opening, shutdown)?
                else {
                    return Ok(CancelOutcome::StillArmed);
                };
                if matches!(target, Target::Client(_)) && !opening.load(Ordering::Acquire) {
                    return Ok(CancelOutcome::StillArmed);
                }
                connection.kill(&target, shutdown)
            })();
            match attempt {
                Ok(CancelOutcome::Acknowledged) => {
                    cancelled = true;
                    let _ = events.send(AudioEvent::Cancelled);
                }
                Ok(CancelOutcome::StillArmed) => {}
                Err(error) => {
                    if shutdown.load(Ordering::Acquire) {
                        break;
                    }
                    let _ = events.send(AudioEvent::CancelFailed(error.clone()));
                    control_error = Some(error);
                }
            }
        }
        if let Err(error) = connection.step(Instant::now() + CONTROL_TIMEOUT, Some(shutdown)) {
            if shutdown.load(Ordering::Acquire) {
                break;
            }
            if cancel.load(Ordering::Acquire) {
                let _ = events.send(AudioEvent::CancelFailed(error.clone()));
            } else if !source_failed {
                let _ = events.send(AudioEvent::SourceLost(error.clone()));
            }
            return Err(error);
        }
    }
    match control_error {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_destroy_winning_fresh_recheck_leaves_cancellation_armed() {
        let client = client(7, "tag", "42");
        let output = output(8, 7, "tag", "42");
        let target = Target::Output(output.clone(), client.clone());
        assert_eq!(
            checked_cancel_target(&target, &[], &[], "tag", "42", false).unwrap(),
            None
        );
        assert_eq!(
            checked_cancel_target(
                &target,
                std::slice::from_ref(&client),
                &[],
                "tag",
                "42",
                false,
            )
            .unwrap(),
            None
        );
        let mut foreign = output.clone();
        foreign.app = Some("foreign".into());
        assert!(
            checked_cancel_target(
                &target,
                std::slice::from_ref(&client),
                &[foreign],
                "tag",
                "42",
                false,
            )
            .is_err()
        );
        assert_eq!(
            checked_cancel_target(&target, &[client], &[output], "tag", "42", false).unwrap(),
            Some(target)
        );
    }

    #[test]
    fn authoritative_missing_kill_reply_is_not_ack_or_false_failure() {
        assert_eq!(
            cancel_reply(false, Some(NO_ENTITY), true).unwrap(),
            CancelOutcome::StillArmed
        );
        assert_eq!(
            cancel_reply(true, None, true).unwrap(),
            CancelOutcome::Acknowledged
        );
        assert!(cancel_reply(false, Some(1), true).is_err());
        assert!(cancel_reply(false, Some(NO_ENTITY), false).is_err());
        assert!(cancel_reply(true, None, false).is_err());
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
    fn only_positively_attributed_recording_removal_becomes_loss() {
        let notifications = Notifications::default();
        let userdata = ptr::from_ref(&notifications).cast_mut().cast();
        // SAFETY: Live local userdata; callback uses no native context API.
        unsafe {
            subscription_callback(
                ptr::null_mut(),
                SOURCE_OUTPUT_EVENT | REMOVE_EVENT,
                8,
                userdata,
            )
        };
        assert_eq!(owned_recording_loss(&notifications, false), None);
        notifications.owned_output.set(Some(8));
        notifications.owned_client.set(Some(7));
        // SAFETY: Foreign recording/client removal cannot match cached owner.
        unsafe {
            subscription_callback(
                ptr::null_mut(),
                SOURCE_OUTPUT_EVENT | REMOVE_EVENT,
                9,
                userdata,
            );
            subscription_callback(ptr::null_mut(), CLIENT_EVENT | REMOVE_EVENT, 6, userdata);
        }
        assert_eq!(owned_recording_loss(&notifications, false), None);
        // SAFETY: Exact previously attributed recording is removed.
        unsafe {
            subscription_callback(
                ptr::null_mut(),
                SOURCE_OUTPUT_EVENT | REMOVE_EVENT,
                8,
                userdata,
            )
        };
        assert!(matches!(
            owned_recording_loss(&notifications, false),
            Some(AudioError::RecordingLost { .. })
        ));
        assert_eq!(owned_recording_loss(&notifications, true), None);
    }

    #[test]
    fn attributed_client_removal_is_transport_loss_without_source_claim() {
        let notifications = Notifications::default();
        notifications.owned_client.set(Some(7));
        let userdata = ptr::from_ref(&notifications).cast_mut().cast();
        // SAFETY: Local callback userdata, no runtime or hardware access.
        unsafe { subscription_callback(ptr::null_mut(), CLIENT_EVENT | REMOVE_EVENT, 7, userdata) };
        let Some(AudioError::RecordingLost { detail }) =
            owned_recording_loss(&notifications, false)
        else {
            panic!("expected attributed recording client loss");
        };
        assert!(detail.contains("client"));
        assert_eq!(owned_recording_loss(&notifications, true), None);
    }

    #[test]
    fn fresh_inventory_closes_removal_before_attribution_callback_gap() {
        let notifications = Notifications::default();
        let clients = [client(7, "tag", "42")];
        let outputs = [output(8, 7, "tag", "42")];
        let target = choose_target("tag", "42", &clients, &outputs, false)
            .unwrap()
            .unwrap();
        // Simulate REMOVE dispatched between an attributed inventory row and
        // caching that row after the operation's end-of-list callback.
        let userdata = ptr::from_ref(&notifications).cast_mut().cast();
        // SAFETY: Local live callback userdata; no native context operations.
        unsafe {
            subscription_callback(
                ptr::null_mut(),
                SOURCE_OUTPUT_EVENT | REMOVE_EVENT,
                8,
                userdata,
            )
        };
        assert_eq!(owned_recording_loss(&notifications, false), None);
        cache_owned_recording(&notifications, &target);
        assert!(notifications.recording_dirty.get());
        latch_missing_owned(&notifications, &clients, &[]);
        assert!(matches!(
            owned_recording_loss(&notifications, false),
            Some(AudioError::RecordingLost { .. })
        ));
        assert_eq!(owned_recording_loss(&notifications, true), None);
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
        };
        assert_eq!(
            validated_source_index(&selected, &[observed(7)]).unwrap(),
            7
        );
        assert_eq!(
            validated_source_index(&selected, &[observed(55)]).unwrap(),
            55
        );
        assert!(matches!(
            validated_source_index(&selected, &[observed(7), observed(55)]),
            Err(AudioError::Ambiguous(_))
        ));
    }

    #[test]
    fn sink_monitors_never_enter_capture_inventory() {
        assert!(!source_is_capture(0));
        assert!(!source_is_capture(17));
        assert!(source_is_capture(INVALID_INDEX));
    }

    #[test]
    fn playback_client_never_authorizes_pending_open_client_kill() {
        let own = client(7, "tag", "42");
        assert!(recording_only_client(&own, &[2, 7]).is_err());
        assert!(recording_only_client(&own, &[2, 9]).is_ok());
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

    fn client(index: u32, tag: &str, pid: &str) -> Client {
        Client {
            index,
            name: tag.into(),
            app: Some(tag.into()),
            pid: Some(pid.into()),
        }
    }

    fn output(index: u32, client: u32, tag: &str, pid: &str) -> SourceOutput {
        SourceOutput {
            index,
            client,
            name: tag.into(),
            app: Some(tag.into()),
            media: Some(tag.into()),
            pid: Some(pid.into()),
        }
    }

    #[test]
    fn exact_owned_output_is_independent_of_routed_source_index() {
        let clients = [client(7, "tag", "42")];
        let outputs = [output(8, 7, "tag", "42")];
        assert_eq!(
            choose_target("tag", "42", &clients, &outputs, false).unwrap(),
            Some(Target::Output(outputs[0].clone(), clients[0].clone()))
        );
    }

    #[test]
    fn foreign_partial_and_duplicate_ownership_fail_closed() {
        let own = client(7, "tag", "42");
        assert!(choose_target("tag", "42", &[client(7, "tag", "99")], &[], true).is_err());
        assert!(choose_target("tag", "42", &[own.clone(), own.clone()], &[], true).is_err());
        let mut partial = output(8, 7, "tag", "42");
        partial.media = None;
        assert!(choose_target("tag", "42", &[own], &[partial], true).is_err());
    }

    #[test]
    fn invalid_client_never_authorizes_kill() {
        let clients = [client(INVALID_INDEX, "tag", "42")];
        assert!(choose_target("tag", "42", &clients, &[], true).is_err());
    }

    #[test]
    fn stopped_before_open_stays_armed_and_only_opening_can_kill_client() {
        assert_eq!(choose_target("tag", "42", &[], &[], true).unwrap(), None);
        let clients = [client(7, "tag", "42")];
        assert_eq!(
            choose_target("tag", "42", &clients, &[], true).unwrap(),
            Some(Target::Client(clients[0].clone()))
        );
        assert_eq!(
            choose_target("tag", "42", &clients, &[], false).unwrap(),
            None
        );
    }

    #[test]
    fn stale_index_or_changed_client_fails_fresh_recheck() {
        let old = output(8, 7, "tag", "42");
        let mut changed = old.clone();
        changed.client = 9;
        assert!(!same_output(&old, &changed));
        changed = old.clone();
        changed.index = 12;
        assert!(!same_output(&old, &changed));
        changed = old.clone();
        changed.app = Some("foreign".into());
        assert!(!same_output(&old, &changed));
        assert_ne!(client(7, "tag", "42"), client(7, "foreign", "42"));
    }
}
