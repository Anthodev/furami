//! Minimal mpv 0.41.0/client 2.5 ABI. Every handle stays on its creating thread.

use super::controller::{
    BackendCommand, BackendEvent, MediaError, OwnerBackend, RequestId, StopFlag, SurfaceToken,
};
use libloading::Library;
use std::{
    ffi::{CStr, CString, c_char, c_int, c_ulong, c_void},
    marker::PhantomData,
    path::{Path, PathBuf},
    ptr::{self, NonNull},
    rc::Rc,
    thread::{self, Thread},
};

const CLIENT_API: c_ulong = 0x0002_0005;
const PROOF_SOURCE: &CStr = c"av://lavfi:testsrc2=size=1280x720:rate=30:d=60";

#[repr(C)]
struct MpvEvent {
    event_id: c_int,
    error: c_int,
    reply_userdata: u64,
    data: *mut c_void,
}
#[repr(C)]
struct MpvLogMessage {
    prefix: *const c_char,
    level: *const c_char,
    text: *const c_char,
    log_level: c_int,
}
#[repr(C)]
struct MpvEndFile {
    reason: c_int,
    error: c_int,
    playlist_entry_id: i64,
    playlist_insert_id: i64,
    playlist_insert_num_entries: c_int,
}

type Create = unsafe extern "C" fn() -> *mut c_void;
type Initialize = unsafe extern "C" fn(*mut c_void) -> c_int;
type SetOption = unsafe extern "C" fn(*mut c_void, *const c_char, *const c_char) -> c_int;
type RequestLogs = unsafe extern "C" fn(*mut c_void, *const c_char) -> c_int;
type CommandAsync = unsafe extern "C" fn(*mut c_void, u64, *const *const c_char) -> c_int;
type GetProperty = unsafe extern "C" fn(*mut c_void, *const c_char) -> *mut c_char;
type Free = unsafe extern "C" fn(*mut c_void);
type WaitEvent = unsafe extern "C" fn(*mut c_void, f64) -> *mut MpvEvent;
type SetWakeup =
    unsafe extern "C" fn(*mut c_void, Option<unsafe extern "C" fn(*mut c_void)>, *mut c_void);
type ClientApi = unsafe extern "C" fn() -> c_ulong;
type ErrorString = unsafe extern "C" fn(c_int) -> *const c_char;
type TerminateDestroy = unsafe extern "C" fn(*mut c_void);

struct Functions {
    create: Create,
    initialize: Initialize,
    set_option: SetOption,
    request_logs: RequestLogs,
    command_async: CommandAsync,
    get_property: GetProperty,
    free: Free,
    wait_event: WaitEvent,
    set_wakeup: SetWakeup,
    client_api: ClientApi,
    error_string: ErrorString,
    terminate_destroy: TerminateDestroy,
}
impl Functions {
    fn resolve(library: &Library) -> Result<Self, MediaError> {
        let missing = |error: libloading::Error| MediaError::new("mpv_symbol", error.to_string());
        // SAFETY: Each signature mirrors the pinned client.h C ABI above. Function
        // pointers are private, and Handle retains this Library until destruction.
        unsafe {
            Ok(Self {
                create: *library.get::<Create>(b"mpv_create\0").map_err(missing)?,
                initialize: *library
                    .get::<Initialize>(b"mpv_initialize\0")
                    .map_err(missing)?,
                set_option: *library
                    .get::<SetOption>(b"mpv_set_option_string\0")
                    .map_err(missing)?,
                request_logs: *library
                    .get::<RequestLogs>(b"mpv_request_log_messages\0")
                    .map_err(missing)?,
                command_async: *library
                    .get::<CommandAsync>(b"mpv_command_async\0")
                    .map_err(missing)?,
                get_property: *library
                    .get::<GetProperty>(b"mpv_get_property_string\0")
                    .map_err(missing)?,
                free: *library.get::<Free>(b"mpv_free\0").map_err(missing)?,
                wait_event: *library
                    .get::<WaitEvent>(b"mpv_wait_event\0")
                    .map_err(missing)?,
                set_wakeup: *library
                    .get::<SetWakeup>(b"mpv_set_wakeup_callback\0")
                    .map_err(missing)?,
                client_api: *library
                    .get::<ClientApi>(b"mpv_client_api_version\0")
                    .map_err(missing)?,
                error_string: *library
                    .get::<ErrorString>(b"mpv_error_string\0")
                    .map_err(missing)?,
                terminate_destroy: *library
                    .get::<TerminateDestroy>(b"mpv_terminate_destroy\0")
                    .map_err(missing)?,
            })
        }
    }
}

fn qualified_library(prefix: &str) -> Result<PathBuf, MediaError> {
    let prefix = Path::new(prefix);
    if !prefix.is_absolute() || !prefix.is_dir() {
        return Err(MediaError::new(
            "media_prefix",
            "FURAMI_MEDIA_PREFIX must be an absolute existing directory",
        ));
    }
    let canonical_prefix = prefix
        .canonicalize()
        .map_err(|e| MediaError::new("media_prefix", e.to_string()))?;
    let library = canonical_prefix
        .join("lib/libmpv.so")
        .canonicalize()
        .map_err(|e| MediaError::new("mpv_library", e.to_string()))?;
    if !library.starts_with(&canonical_prefix) || !library.is_file() {
        return Err(MediaError::new(
            "mpv_library",
            "lib/libmpv.so resolves outside frozen media prefix or is not a file",
        ));
    }
    Ok(library)
}

struct WakeContext {
    thread: Thread,
}
unsafe extern "C" fn wake_owner(context: *mut c_void) {
    // SAFETY: Registered userdata points to a stable Box<WakeContext>, kept alive
    // through unregister and terminate_destroy. Foreign callbacks only unpark;
    // no libmpv call, mutable access or GUI target is permitted here.
    unsafe {
        (&*context.cast::<WakeContext>()).thread.unpark();
    }
}

struct Handle {
    raw: NonNull<c_void>,
    functions: Functions,
    wake: Box<WakeContext>,
    _library: Library,
    _owner_only: PhantomData<Rc<()>>,
}
impl Handle {
    fn create(path: &Path, stop: &StopFlag) -> Result<Self, MediaError> {
        check_stop(stop)?;
        // SAFETY: Only the explicitly qualified canonical ELF is loaded. Its
        // initialization code is part of the approved frozen stack, not user media.
        let library = unsafe { Library::new(path) }
            .map_err(|e| MediaError::new("mpv_load", e.to_string()))?;
        check_stop(stop)?;
        let functions = Functions::resolve(&library)?;
        // SAFETY: Resolved pinned zero-argument C function; library remains live.
        let api = unsafe { (functions.client_api)() };
        if api != CLIENT_API {
            return Err(MediaError::new(
                "mpv_api",
                format!("libmpv client API {api:#x}, expected {CLIENT_API:#x}"),
            ));
        }
        check_stop(stop)?;
        // SAFETY: mpv_create runs on the eventual owner thread, after C++ has
        // established XInitThreads and LC_NUMERIC=C. Returned handle is private.
        let raw = NonNull::new(unsafe { (functions.create)() })
            .ok_or_else(|| MediaError::new("mpv_create", "mpv_create returned null"))?;
        let mut handle = Self {
            raw,
            functions,
            wake: Box::new(WakeContext {
                thread: thread::current(),
            }),
            _library: library,
            _owner_only: PhantomData,
        };
        let context = ptr::from_mut(handle.wake.as_mut()).cast::<c_void>();
        // SAFETY: Handle and callback ABI match client.h; Box allocation stays
        // stable even if Handle moves. Drop keeps callback data through shutdown.
        unsafe {
            (handle.functions.set_wakeup)(handle.raw.as_ptr(), Some(wake_owner), context);
        }
        tracing::info!(path = %path.display(), api, "libmpv_loaded");
        Ok(handle)
    }
    fn checked(&self, code: c_int, operation: &str) -> Result<(), MediaError> {
        if code >= 0 {
            return Ok(());
        }
        // SAFETY: Pinned error_string returns a static NUL-terminated C string;
        // copy occurs while owning Library, before any unload.
        let detail =
            unsafe { CStr::from_ptr((self.functions.error_string)(code)) }.to_string_lossy();
        Err(MediaError::new(
            "mpv_call",
            format!("{operation}: {detail} ({code})"),
        ))
    }
    fn option(&self, name: &CStr, value: &CStr) -> Result<(), MediaError> {
        // SAFETY: Owner-thread live handle and NUL-terminated arguments remain
        // valid throughout the synchronous call; mpv copies option values.
        let result = unsafe {
            (self.functions.set_option)(self.raw.as_ptr(), name.as_ptr(), value.as_ptr())
        };
        self.checked(result, name.to_str().unwrap_or("mpv option"))?;
        tracing::info!(option = %name.to_string_lossy(), value = %value.to_string_lossy(), "mpv_option");
        Ok(())
    }
    fn property(&self, name: &CStr) -> Result<String, MediaError> {
        // SAFETY: Live owner handle, valid C property name. Returned allocation
        // belongs to mpv and is copied before the matching mpv_free below.
        let value = unsafe { (self.functions.get_property)(self.raw.as_ptr(), name.as_ptr()) };
        if value.is_null() {
            return Err(MediaError::new(
                "mpv_property",
                format!("missing property {}", name.to_string_lossy()),
            ));
        }
        // SAFETY: Nonnull mpv-owned property string is NUL terminated per API.
        let owned = unsafe { CStr::from_ptr(value) }
            .to_string_lossy()
            .into_owned();
        // SAFETY: This is precisely the allocation returned above, freed once,
        // through its owning Library while Handle is still live.
        unsafe {
            (self.functions.free)(value.cast());
        }
        Ok(owned)
    }
}
impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: !Send/!Sync ensures owner-thread destruction of the unique raw
        // handle. Unregister first, retain wake Box/Functions/Library throughout
        // terminate_destroy, which is the actual destruction completion boundary.
        unsafe {
            (self.functions.set_wakeup)(self.raw.as_ptr(), None, ptr::null_mut());
            (self.functions.terminate_destroy)(self.raw.as_ptr());
        }
    }
}

pub(crate) struct MpvBackend {
    prefix: String,
    handle: Option<Handle>,
}
impl MpvBackend {
    pub(crate) fn new(prefix: String) -> Self {
        Self {
            prefix,
            handle: None,
        }
    }
    fn handle(&self) -> Result<&Handle, MediaError> {
        self.handle
            .as_ref()
            .ok_or_else(|| MediaError::new("mpv_handle", "owner has no libmpv handle"))
    }
}
fn check_stop(stop: &StopFlag) -> Result<(), MediaError> {
    if stop.is_set() {
        Err(MediaError::new(
            "cancelled",
            "initialization cancelled before playback",
        ))
    } else {
        Ok(())
    }
}
impl OwnerBackend for MpvBackend {
    fn initialize(&mut self, token: SurfaceToken, stop: &StopFlag) -> Result<(), MediaError> {
        check_stop(stop)?;
        let path = qualified_library(&self.prefix)?;
        self.handle = Some(Handle::create(&path, stop)?);
        check_stop(stop)?;
        let handle = self.handle()?;
        // mpv 0.41.0 registers `osc` only with HAVE_LUA. The frozen no-Lua
        // build has neither that option nor an OSC, so no `osc=no` is submitted.
        const OPTIONS: &[(&CStr, &CStr)] = &[
            (c"config", c"no"),
            (c"load-scripts", c"no"),
            (c"input-default-bindings", c"no"),
            (c"input-vo-keyboard", c"no"),
            (c"input-cursor", c"no"),
            (c"input-cursor-passthrough", c"yes"),
            (c"cursor-autohide", c"no"),
            (c"vo", c"gpu-next"),
            (c"gpu-api", c"vulkan"),
            (c"gpu-context", c"x11vk"),
            (c"gpu-sw", c"no"),
        ];
        for &(name, value) in OPTIONS {
            check_stop(stop)?;
            handle.option(name, value)?;
        }
        check_stop(stop)?;
        let xid = CString::new(token.xid.get().to_string()).expect("decimal XID contains no NUL");
        handle.option(c"wid", &xid)?;
        check_stop(stop)?;
        // SAFETY: Live unique owner handle and static C log-level string. All
        // later log payloads are copied within their wait_event lifetime.
        let result = unsafe { (handle.functions.request_logs)(handle.raw.as_ptr(), c"v".as_ptr()) };
        handle.checked(result, "request verbose logs")?;
        check_stop(stop)?;
        // SAFETY: All pinned options were set on this owner-thread handle before
        // initialization. Synchronous initialize may block only this owner thread.
        let result = unsafe { (handle.functions.initialize)(handle.raw.as_ptr()) };
        handle.checked(result, "initialize")?;
        check_stop(stop)?;
        let version = handle.property(c"mpv-version")?;
        if version.trim_matches(|c: char| c.is_ascii_whitespace()) != "mpv v0.41.0" {
            return Err(MediaError::new(
                "mpv_version",
                format!("runtime mpv-version {version:?}, expected mpv v0.41.0"),
            ));
        }
        check_stop(stop)?;
        tracing::info!(
            generation = token.generation.get(),
            xid = token.xid.get(),
            version,
            "mpv_initialized"
        );
        Ok(())
    }
    fn submit(&mut self, id: RequestId, command: BackendCommand) -> Result<(), MediaError> {
        let handle = self.handle()?;
        let arguments = match command {
            BackendCommand::LoadProofSource => {
                [c"loadfile".as_ptr(), PROOF_SOURCE.as_ptr(), ptr::null()]
            }
            BackendCommand::TogglePause => [c"cycle".as_ptr(), c"pause".as_ptr(), ptr::null()],
        };
        // SAFETY: Owner-thread live handle, checked nonzero ID and a terminated
        // argv of static C strings. mpv copies arguments before returning.
        let result = unsafe {
            (handle.functions.command_async)(handle.raw.as_ptr(), id.get(), arguments.as_ptr())
        };
        handle.checked(result, "command_async")
    }
    fn next_event(&mut self) -> Result<BackendEvent, MediaError> {
        let handle = self.handle()?;
        // SAFETY: This owner is the sole wait_event caller. All event and nested
        // values below are consumed/copied before the next wait or handle drop.
        let event = unsafe { &*(handle.functions.wait_event)(handle.raw.as_ptr(), 0.0) };
        match event.event_id {
            0 => Ok(BackendEvent::None),
            1 => Ok(BackendEvent::Shutdown),
            2 => {
                if event.data.is_null() {
                    return Err(MediaError::new("mpv_event", "null log payload"));
                }
                // SAFETY: LOG_MESSAGE data matches pinned repr(C) payload and its
                // three C strings stay valid within this wait_event interval.
                let (prefix, level, text) = unsafe {
                    let log = &*event.data.cast::<MpvLogMessage>();
                    (
                        CStr::from_ptr(log.prefix).to_string_lossy().into_owned(),
                        CStr::from_ptr(log.level).to_string_lossy().into_owned(),
                        CStr::from_ptr(log.text).to_string_lossy().into_owned(),
                    )
                };
                tracing::info!(
                    mpv_prefix = prefix,
                    mpv_level = level,
                    "{}",
                    text.trim_end()
                );
                Ok(BackendEvent::Other)
            }
            5 => Ok(BackendEvent::CommandReply {
                id: event.reply_userdata,
                error: event.error,
            }),
            7 => {
                if event.data.is_null() {
                    return Err(MediaError::new("mpv_event", "null END_FILE payload"));
                }
                // SAFETY: END_FILE data matches the complete pinned repr(C)
                // payload. Copy only scalar reason/error; never retain its pointer.
                let end = unsafe { &*event.data.cast::<MpvEndFile>() };
                Ok(BackendEvent::EndFile {
                    reason: end.reason,
                    error: end.error,
                })
            }
            8 => {
                let vo = handle.property(c"current-vo")?;
                tracing::info!(current_vo = vo, "mpv_file_loaded");
                Ok(BackendEvent::FileLoaded)
            }
            24 => Ok(BackendEvent::QueueOverflow),
            _ => Ok(BackendEvent::Other),
        }
    }
    fn read_pause(&mut self) -> Result<bool, MediaError> {
        match self.handle()?.property(c"pause")?.as_str() {
            "yes" => Ok(true),
            "no" => Ok(false),
            value => Err(MediaError::new(
                "pause_property",
                format!("invalid pause property {value:?}"),
            )),
        }
    }
    fn shutdown(&mut self) {
        if self.handle.is_none() {
            tracing::info!("owner_handle_absent");
        }
        drop(self.handle.take());
    }
}
