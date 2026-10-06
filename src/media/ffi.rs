//! Minimal mpv 0.41.0/client 2.5 ABI. Every handle stays on its creating thread.

use super::controller::{
    BackendCommand, BackendEvent, MediaError, OwnerBackend, PauseObservation, RequestId, StopFlag,
    SurfaceToken,
};
use super::loopback::{
    LoopbackCommand, LoopbackEvent, LoopbackOpen, LoopbackOwner, LoopbackReceipt,
};
use super::session::{Observation, ObservedFacts, Source};
use crate::capture::input::InputSpec;
use crate::domain::capture::{
    AudioAvailability, AudioEpoch, AudioError, AudioSelection, AudioSilence, AudioSourceIdentity,
    FrameSize, PlaybackGain, WatchStamp,
};
use crate::domain::{
    output::OutputPlan,
    state::{AttemptId, InitialPlayback, PauseRequestId},
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

use std::collections::VecDeque;

const MPV_FORMAT_DOUBLE: c_int = 5;

// Layouts and formats match mpv v0.41.0 include/mpv/client.h. No native event
// pointer survives a wait_event call; copied nodes own all nested allocations.
#[repr(C)]
#[derive(Clone, Copy)]
union MpvNodeValue {
    string: *mut c_char,
    flag: c_int,
    int64: i64,
    double: f64,
    list: *mut MpvNodeList,
    bytes: *mut MpvBytes,
}
#[repr(C)]
#[derive(Clone, Copy)]
struct MpvNode {
    value: MpvNodeValue,
    format: c_int,
}
#[repr(C)]
struct MpvNodeList {
    num: c_int,
    values: *mut MpvNode,
    keys: *mut *mut c_char,
}
#[repr(C)]
struct MpvBytes {
    data: *mut c_void,
    size: usize,
}
#[repr(C)]
struct MpvPropertyEvent {
    name: *const c_char,
    format: c_int,
    data: *mut c_void,
}
#[repr(C)]
struct MpvCommandEvent {
    result: MpvNode,
}

#[derive(Clone, Debug, PartialEq)]
enum NodeValue {
    None,
    String(CString),
    Flag(bool),
    Int(i64),
    Double(f64),
    Array(Vec<NodeValue>),
    Map(Vec<(CString, NodeValue)>),
    Bytes(Vec<u8>),
}
impl NodeValue {
    fn text(&self) -> Option<&str> {
        match self {
            Self::String(value) => value.to_str().ok(),
            _ => None,
        }
    }
    fn field(&self, name: &str) -> Option<&Self> {
        match self {
            Self::Map(fields) => fields
                .iter()
                .find(|(key, _)| key.to_bytes() == name.as_bytes())
                .map(|(_, value)| value),
            _ => None,
        }
    }
}

unsafe fn copy_node(node: &MpvNode, depth: usize) -> Result<NodeValue, MediaError> {
    let malformed = || MediaError::new("mpv_event", "malformed mpv node payload");
    if depth > 32 {
        return Err(malformed());
    }
    // SAFETY: Caller guarantees live pinned event payload. Format determines
    // union member; nonnull pointers and bounded counts are checked before use.
    unsafe {
        Ok(match node.format {
            0 => NodeValue::None,
            1 => {
                if node.value.string.is_null() {
                    return Err(malformed());
                }
                NodeValue::String(CStr::from_ptr(node.value.string).to_owned())
            }
            3 => NodeValue::Flag(node.value.flag != 0),
            4 => NodeValue::Int(node.value.int64),
            5 => NodeValue::Double(node.value.double),
            7 | 8 => {
                if node.value.list.is_null() {
                    return Err(malformed());
                }
                let list = &*node.value.list;
                if !(0..=65_536).contains(&list.num) || (list.num > 0 && list.values.is_null()) {
                    return Err(malformed());
                }
                let mut values = Vec::with_capacity(list.num as usize);
                for index in 0..list.num as usize {
                    values.push(copy_node(&*list.values.add(index), depth + 1)?);
                }
                if node.format == 7 {
                    NodeValue::Array(values)
                } else {
                    if list.num > 0 && list.keys.is_null() {
                        return Err(malformed());
                    }
                    let mut fields = Vec::with_capacity(values.len());
                    for (index, value) in values.into_iter().enumerate() {
                        let key = *list.keys.add(index);
                        if key.is_null() {
                            return Err(malformed());
                        }
                        fields.push((CStr::from_ptr(key).to_owned(), value));
                    }
                    // mpv map key order is unspecified. Canonicalize snapshots
                    // for exact restoration comparisons, not string re-parsing.
                    fields.sort_unstable_by(|(left, _), (right, _)| {
                        left.as_bytes().cmp(right.as_bytes())
                    });
                    NodeValue::Map(fields)
                }
            }
            9 => {
                if node.value.bytes.is_null() {
                    return Err(malformed());
                }
                let bytes = &*node.value.bytes;
                if bytes.size > 16_777_216 || (bytes.size > 0 && bytes.data.is_null()) {
                    return Err(malformed());
                }
                NodeValue::Bytes(if bytes.size == 0 {
                    Vec::new()
                } else {
                    std::slice::from_raw_parts(bytes.data.cast::<u8>(), bytes.size).to_vec()
                })
            }
            _ => {
                return Err(MediaError::new(
                    "mpv_event",
                    format!("unsupported mpv node format {}", node.format),
                ));
            }
        })
    }
}

/// Temporary pointer graph borrows owned nodes. mpv_set_property_async copies
/// before returning, so neither strings nor list backing needs to escape call.
struct NativeNode<'a> {
    node: MpvNode,
    _children: Vec<NativeNode<'a>>,
    _values: Vec<MpvNode>,
    _keys: Vec<*mut c_char>,
    _list: Option<Box<MpvNodeList>>,
    _bytes: Option<Box<MpvBytes>>,
    _borrow: PhantomData<&'a NodeValue>,
}
impl<'a> NativeNode<'a> {
    fn new(value: &'a NodeValue) -> Self {
        let mut native = Self {
            node: MpvNode {
                value: MpvNodeValue { int64: 0 },
                format: 0,
            },
            _children: Vec::new(),
            _values: Vec::new(),
            _keys: Vec::new(),
            _list: None,
            _bytes: None,
            _borrow: PhantomData,
        };
        native.node = match value {
            NodeValue::None => native.node,
            NodeValue::String(text) => MpvNode {
                value: MpvNodeValue {
                    string: text.as_ptr().cast_mut(),
                },
                format: 1,
            },
            NodeValue::Flag(flag) => MpvNode {
                value: MpvNodeValue {
                    flag: i32::from(*flag),
                },
                format: 3,
            },
            NodeValue::Int(number) => MpvNode {
                value: MpvNodeValue { int64: *number },
                format: 4,
            },
            NodeValue::Double(number) => MpvNode {
                value: MpvNodeValue { double: *number },
                format: 5,
            },
            NodeValue::Array(items) => {
                native._children = items.iter().map(Self::new).collect();
                native.list_node(7)
            }
            NodeValue::Map(fields) => {
                native._children = fields.iter().map(|(_, value)| Self::new(value)).collect();
                native._keys = fields
                    .iter()
                    .map(|(key, _)| key.as_ptr().cast_mut())
                    .collect();
                native.list_node(8)
            }
            NodeValue::Bytes(bytes) => {
                let mut data = Box::new(MpvBytes {
                    data: bytes.as_ptr().cast_mut().cast(),
                    size: bytes.len(),
                });
                let node = MpvNode {
                    value: MpvNodeValue {
                        bytes: ptr::from_mut(data.as_mut()),
                    },
                    format: 9,
                };
                native._bytes = Some(data);
                node
            }
        };
        native
    }
    fn list_node(&mut self, format: i32) -> MpvNode {
        self._values = self._children.iter().map(|child| child.node).collect();
        let mut list = Box::new(MpvNodeList {
            num: self._values.len() as i32,
            values: self._values.as_mut_ptr(),
            keys: if format == 8 {
                self._keys.as_mut_ptr()
            } else {
                ptr::null_mut()
            },
        });
        let node = MpvNode {
            value: MpvNodeValue {
                list: ptr::from_mut(list.as_mut()),
            },
            format,
        };
        self._list = Some(list);
        node
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ReplyKind {
    Command,
    Property,
    SetProperty,
}
const CLIENT_API: c_ulong = 0x0002_0005;

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
type SetOptionValue = unsafe extern "C" fn(*mut c_void, *const c_char, c_int, *mut c_void) -> c_int;
type RequestLogs = unsafe extern "C" fn(*mut c_void, *const c_char) -> c_int;
type CommandAsync = unsafe extern "C" fn(*mut c_void, u64, *const *const c_char) -> c_int;
type GetProperty = unsafe extern "C" fn(*mut c_void, *const c_char) -> *mut c_char;
type GetPropertyAsync = unsafe extern "C" fn(*mut c_void, u64, *const c_char, c_int) -> c_int;
type SetPropertyAsync =
    unsafe extern "C" fn(*mut c_void, u64, *const c_char, c_int, *mut c_void) -> c_int;
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
    set_option_value: SetOptionValue,
    request_logs: RequestLogs,
    command_async: CommandAsync,
    get_property: GetProperty,
    get_property_async: GetPropertyAsync,
    set_property_async: SetPropertyAsync,
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
                set_option_value: *library
                    .get::<SetOptionValue>(b"mpv_set_option\0")
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
                get_property_async: *library
                    .get::<GetPropertyAsync>(b"mpv_get_property_async\0")
                    .map_err(missing)?,
                set_property_async: *library
                    .get::<SetPropertyAsync>(b"mpv_set_property_async\0")
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
    fn option_double(&self, name: &CStr, mut value: f64) -> Result<(), MediaError> {
        // SAFETY: The live owner-thread handle and aligned stack f64 remain
        // valid during the synchronous call. mpv copies this typed scalar.
        let result = unsafe {
            (self.functions.set_option_value)(
                self.raw.as_ptr(),
                name.as_ptr(),
                MPV_FORMAT_DOUBLE,
                ptr::from_mut(&mut value).cast::<c_void>(),
            )
        };
        self.checked(result, name.to_str().unwrap_or("mpv option"))?;
        tracing::info!(option = %name.to_string_lossy(), value, "mpv_option");
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
    fn get_async(&self, id: u64, name: &CStr) -> Result<(), MediaError> {
        // SAFETY: Live owner handle, NUL-terminated property name, NODE format.
        let result =
            unsafe { (self.functions.get_property_async)(self.raw.as_ptr(), id, name.as_ptr(), 6) };
        self.checked(result, "get_property_async")
    }
    fn set_async(&self, id: u64, name: &CStr, value: &NodeValue) -> Result<(), MediaError> {
        let mut native = NativeNode::new(value);
        // SAFETY: Complete borrowed pointer graph stays live during call. API
        // copies value before return; no pointer is retained by Rust afterward.
        let result = unsafe {
            (self.functions.set_property_async)(
                self.raw.as_ptr(),
                id,
                name.as_ptr(),
                6,
                ptr::from_mut(&mut native.node).cast(),
            )
        };
        self.checked(result, "set_property_async")
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

#[derive(Clone, Copy)]
enum Pending {
    Pause,
    PauseSet {
        owner: u64,
        request: PauseRequestId,
        expected: bool,
    },
    PauseRead {
        owner: u64,
        request: PauseRequestId,
        expected: bool,
    },
    Metadata(usize),
    #[cfg(test)]
    FixtureProperty(&'static CStr),
}
impl Pending {
    fn property(self) -> Option<&'static CStr> {
        Some(match self {
            Self::PauseSet { .. } => return None,
            Self::Pause | Self::PauseRead { .. } => c"pause",
            Self::Metadata(0) => c"video-params",
            Self::Metadata(_) => c"container-fps",
            #[cfg(test)]
            Self::FixtureProperty(name) => name,
        })
    }
}
struct MetadataBatch {
    values: [Option<NodeValue>; 2],
    remaining: usize,
    started: bool,
    rerun: Option<bool>,
}

struct AudioRetirement {
    epoch: AudioEpoch,
    reason: AudioSilence,
}
pub(crate) struct MpvBackend {
    prefix: String,
    handle: Option<Handle>,
    input: InputSpec,
    nominal_rate_source: Source,
    diagnostics: std::collections::VecDeque<String>,
    audio: AudioSelection,
    gain: PlaybackGain,
    output: OutputPlan,
    audio_status: AudioAvailability,
    loopback: Option<LoopbackOwner>,
    owner_cancel: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    playback: InitialPlayback,
    paused: bool,
    watch: WatchStamp,
    generation: u64,
    epoch: Option<AudioEpoch>,
    last_audio_epoch: u64,
    retirement: Option<AudioRetirement>,
    terminal_audio: bool,
    initial_audio_checked: bool,
    joint_pause: Option<(u64, PauseObservation)>,
    load_id: Option<u64>,
    load_complete: bool,
    video_started: bool,
    video_ended: bool,
    last_internal: u64,
    pending: Vec<(u64, Pending)>,
    metadata: Option<MetadataBatch>,
    pause_pending: bool,
    pause_dirty: bool,
    emitted: VecDeque<BackendEvent>,
    #[cfg(test)]
    property_requests: Option<Vec<(u64, &'static CStr, Option<NodeValue>)>>,
    #[cfg(test)]
    fixture_properties: Option<VecDeque<(u64, NodeValue)>>,
}
impl MpvBackend {
    pub(crate) fn new(
        prefix: String,
        input: InputSpec,
        audio: AudioSelection,
        gain: PlaybackGain,
        playback: InitialPlayback,
        watch: WatchStamp,
        output: OutputPlan,
    ) -> Self {
        let audio_enabled = audio.enabled();
        Self {
            prefix,
            handle: None,
            input,
            nominal_rate_source: Source::MpvContainerFps,
            diagnostics: std::collections::VecDeque::new(),
            audio,
            gain,
            audio_status: if audio_enabled {
                AudioAvailability::Silent {
                    reason: if playback == InitialPlayback::Paused {
                        AudioSilence::Paused
                    } else {
                        output
                            .silence()
                            .cloned()
                            .map_or(AudioSilence::PendingRoute, AudioSilence::Output)
                    },
                }
            } else {
                AudioAvailability::Disabled
            },
            output,
            loopback: None,
            owner_cancel: None,
            playback,
            paused: playback == InitialPlayback::Paused,
            watch,
            generation: 0,
            epoch: None,
            last_audio_epoch: 0,
            retirement: None,
            terminal_audio: false,
            initial_audio_checked: false,
            joint_pause: None,
            load_id: None,
            load_complete: false,
            video_started: false,
            video_ended: false,
            last_internal: 1 << 63,
            pending: Vec::with_capacity(8),
            metadata: None,
            pause_pending: false,
            pause_dirty: false,
            emitted: VecDeque::new(),
            #[cfg(test)]
            property_requests: None,
            #[cfg(test)]
            fixture_properties: None,
        }
    }
    fn handle(&self) -> Result<&Handle, MediaError> {
        self.handle
            .as_ref()
            .ok_or_else(|| MediaError::new("mpv_handle", "owner has no libmpv handle"))
    }
    fn next_internal(&mut self) -> Result<u64, MediaError> {
        self.last_internal = self.last_internal.checked_add(1).ok_or_else(|| {
            MediaError::new("request_exhausted", "internal mpv request IDs exhausted")
        })?;
        Ok(self.last_internal)
    }
    fn get(&mut self, name: &'static CStr, pending: Pending) -> Result<(), MediaError> {
        let id = self.next_internal()?;
        #[cfg(test)]
        if let Some(requests) = &mut self.property_requests {
            requests.push((id, name, None));
            self.pending.push((id, pending));
            return Ok(());
        }
        self.handle()?.get_async(id, name)?;
        self.pending.push((id, pending));
        Ok(())
    }
    fn set(
        &mut self,
        name: &'static CStr,
        value: &NodeValue,
        pending: Pending,
    ) -> Result<(), MediaError> {
        let id = self.next_internal()?;
        #[cfg(test)]
        if let Some(requests) = &mut self.property_requests {
            requests.push((id, name, Some(value.clone())));
            self.pending.push((id, pending));
            return Ok(());
        }
        self.handle()?.set_async(id, name, value)?;
        self.pending.push((id, pending));
        Ok(())
    }
    fn publish_audio(&mut self, status: AudioAvailability) {
        if self.audio_status != status {
            self.audio_status = status;
            self.emitted
                .push_back(BackendEvent::AudioAvailability(self.audio_status.clone()));
        }
    }

    fn cancelled(&self) -> bool {
        self.owner_cancel
            .as_ref()
            .is_some_and(|cancel| cancel.load(std::sync::atomic::Ordering::Acquire))
    }

    fn start_audio(
        &mut self,
        epoch: AudioEpoch,
        source: AudioSourceIdentity,
        stamp: WatchStamp,
    ) -> Result<(), MediaError> {
        if self.cancelled() {
            return Ok(());
        }
        if self.loopback.is_some()
            || self.retirement.is_some()
            || self.epoch.is_some()
            || epoch.get() <= self.last_audio_epoch
            || self.paused
            || self.terminal_audio
            || !matches!(&self.audio, AudioSelection::Enabled { source: selected } if selected == &source)
            || stamp.watch != self.watch.watch
            || stamp.epoch.get() < self.watch.epoch.get()
        {
            return Err(MediaError::new(
                "audio_admission",
                "audio epoch/source/watch is not current",
            ));
        }
        let OutputPlan::Target { revision, target } = &self.output else {
            // This epoch was already admitted by the app/gate. No resource was
            // created, but its superseded transaction must still be consumed.
            self.last_audio_epoch = epoch.get();
            self.watch = stamp;
            if let Some(reason) = self.output.silence() {
                self.publish_audio(AudioAvailability::Silent {
                    reason: AudioSilence::Output(reason.clone()),
                });
            }
            self.emitted.push_back(BackendEvent::AudioDetached {
                epoch,
                outcome: Ok(()),
            });
            return Ok(());
        };
        let owner_cancel = std::sync::Arc::clone(
            self.owner_cancel
                .as_ref()
                .ok_or_else(|| MediaError::new("audio_admission", "owner cancellation absent"))?,
        );
        let generation = super::controller::Generation::new(self.generation)
            .ok_or_else(|| MediaError::new("audio_admission", "session generation absent"))?;
        let open = LoopbackOpen {
            generation,
            attempt: AttemptId::new(self.generation).expect("valid generation"),
            epoch,
            source,
            watch: stamp,
            output: target.clone(),
            output_revision: *revision,
            gain: self.gain,
        };
        self.last_audio_epoch = epoch.get();
        self.watch = stamp;
        self.epoch = Some(epoch);
        self.publish_audio(AudioAvailability::Opening { epoch });
        match LoopbackOwner::begin(open, owner_cancel) {
            Ok(owner) => self.loopback = Some(owner),
            Err(error) => {
                self.terminal_audio = true;
                self.epoch = None;
                self.publish_audio(AudioAvailability::Silent {
                    reason: AudioSilence::Failed(error),
                });
                self.emitted.push_back(BackendEvent::AudioDetached {
                    epoch,
                    outcome: Ok(()),
                });
            }
        }
        Ok(())
    }

    fn retire_audio(&mut self, reason: AudioSilence) {
        if let Some(epoch) = self.epoch {
            if let Some(owner) = &self.loopback {
                owner.stop();
            }
            if matches!(&reason, AudioSilence::Failed(_)) {
                self.terminal_audio = true;
                if let Some(retirement) = &mut self.retirement {
                    retirement.reason = reason.clone();
                }
            }
            if self.retirement.is_none() {
                self.retirement = Some(AudioRetirement { epoch, reason });
                self.publish_audio(AudioAvailability::Detaching { epoch });
            }
        } else if self.audio.enabled() {
            self.publish_audio(AudioAvailability::Silent { reason });
        }
    }

    fn receipt_current(&self, receipt: &LoopbackReceipt) -> bool {
        receipt.generation().get() == self.generation
            && receipt.attempt().get() == self.generation
            && self.epoch == Some(receipt.epoch())
            && self.watch == receipt.watch()
            && matches!(&self.audio, AudioSelection::Enabled { source } if source == receipt.source())
            && receipt.output_revision() == self.output.revision()
    }

    fn consume_audio_event(&mut self, event: LoopbackEvent) {
        match event {
            LoopbackEvent::Opening | LoopbackEvent::UnlinkedReady(_) => {}
            LoopbackEvent::Active(receipt)
                if self.retirement.is_none()
                    && self.receipt_current(&receipt)
                    && receipt.destination() == self.output.target() =>
            {
                self.publish_audio(AudioAvailability::Active { route: receipt });
            }
            LoopbackEvent::Switching { revision }
                if self.retirement.is_none() && revision == self.output.revision() =>
            {
                if let Some(epoch) = self.epoch {
                    self.publish_audio(AudioAvailability::Switching { epoch, revision });
                }
            }
            LoopbackEvent::Silent { reason, receipt }
                if self.retirement.is_none() && self.receipt_current(&receipt) =>
            {
                self.publish_audio(AudioAvailability::Silent {
                    reason: AudioSilence::Output(reason),
                });
            }
            LoopbackEvent::SourceLost(error) => {
                self.retire_audio(AudioSilence::WaitingForSource(error))
            }
            LoopbackEvent::Failed(error) => {
                self.terminal_audio = true;
                self.retire_audio(AudioSilence::Failed(error));
            }
            LoopbackEvent::Blocked { stage, diagnostic } => {
                if let Some(epoch) = self.epoch {
                    self.publish_audio(AudioAvailability::Blocked {
                        epoch,
                        error: AudioError::Control(format!("{stage}: {diagnostic}")),
                    });
                }
            }
            LoopbackEvent::Stopped { outcome } => {
                // Only LoopbackOwner's post-join event can clear these fields.
                let retired = self.retirement.take();
                let epoch = self.epoch.take();
                self.loopback = None;
                if let Some(epoch) = epoch {
                    let prior = retired.map(|retired| {
                        debug_assert_eq!(retired.epoch, epoch);
                        retired.reason
                    });
                    let reason = match (prior, outcome) {
                        (Some(reason @ AudioSilence::Failed(_)), _) => reason,
                        (
                            _,
                            Err(
                                error @ (AudioError::SourceMissing { .. }
                                | AudioError::SourceChanged { .. }),
                            ),
                        ) if !self.terminal_audio => AudioSilence::WaitingForSource(error),
                        (_, Err(error)) => {
                            self.terminal_audio = true;
                            AudioSilence::Failed(error)
                        }
                        (Some(reason), Ok(())) => reason,
                        (None, Ok(())) => {
                            self.terminal_audio = true;
                            AudioSilence::Failed(AudioError::Control(
                                "audio owner exited unexpectedly".into(),
                            ))
                        }
                    };
                    if matches!(&reason, AudioSilence::Failed(_)) {
                        self.terminal_audio = true;
                    }
                    self.publish_audio(AudioAvailability::Silent { reason });
                    self.emitted.push_back(BackendEvent::AudioDetached {
                        epoch,
                        outcome: Ok(()),
                    });
                }
            }
            _ => {}
        }
        self.finish_joint_pause();
    }

    fn drive_audio(&mut self) {
        for _ in 0..4 {
            let Some(event) = self.loopback.as_mut().and_then(LoopbackOwner::poll) else {
                break;
            };
            self.consume_audio_event(event);
        }
    }

    fn finish_joint_pause(&mut self) {
        if self.loopback.is_none()
            && self.epoch.is_none()
            && self.retirement.is_none()
            && let Some((owner, observation)) = self.joint_pause.take()
        {
            self.emitted
                .push_back(BackendEvent::PauseObserved(observation));
            self.emitted.push_back(BackendEvent::CommandReply {
                id: owner,
                error: 0,
            });
        }
    }

    fn begin_audio_if_ready(&mut self) -> Result<(), MediaError> {
        if !self.load_complete
            || !self.video_started
            || self.video_ended
            || self.paused
            || self.initial_audio_checked
            || self.cancelled()
        {
            return Ok(());
        }
        self.initial_audio_checked = true;
        if let AudioSelection::Enabled { source } = &self.audio
            && self.output.target().is_some()
        {
            return self.start_audio(
                AudioEpoch::new(1).expect("initial epoch"),
                source.clone(),
                self.watch,
            );
        }
        Ok(())
    }
    fn reply(
        &mut self,
        id: u64,
        kind: ReplyKind,
        error: i32,
        name: Option<CString>,
        value: Option<NodeValue>,
    ) -> Result<BackendEvent, MediaError> {
        let Some((index, pending)) = self
            .pending
            .iter()
            .enumerate()
            .find(|(_, (request, _))| *request == id)
            .map(|(index, (_, pending))| (index, *pending))
        else {
            if kind == ReplyKind::Command && self.load_id == Some(id) {
                if error >= 0 {
                    self.load_complete = true;
                    self.begin_audio_if_ready()?;
                }
                return Ok(BackendEvent::CommandReply { id, error });
            }
            return Ok(BackendEvent::Other);
        };
        let expected = match pending {
            Pending::PauseSet { .. } => {
                if kind != ReplyKind::SetProperty {
                    return Ok(BackendEvent::Other);
                }
                None
            }
            _ => {
                if kind != ReplyKind::Property {
                    return Ok(BackendEvent::Other);
                }
                pending.property()
            }
        };
        if expected.is_some_and(|expected| name.as_deref() != Some(expected)) {
            return Err(MediaError::new(
                if matches!(pending, Pending::Pause | Pending::PauseRead { .. }) {
                    "pause_property"
                } else {
                    "mpv_event"
                },
                format!("property reply {id} has unexpected name"),
            ));
        }
        self.pending.swap_remove(index);
        tracing::info!(request = id, kind = ?kind, error, property = ?name, value = ?value, "mpv_owned_reply");
        match pending {
            Pending::Pause => {
                self.pause_pending = false;
                let paused = match value {
                    Some(NodeValue::Flag(paused)) if error >= 0 => paused,
                    _ => {
                        return Err(MediaError::new(
                            "pause_property",
                            format!("invalid fresh pause property, mpv error={error}"),
                        ));
                    }
                };
                self.paused = paused;
                self.emitted
                    .push_back(BackendEvent::PauseObserved(PauseObservation {
                        request: None,
                        paused,
                    }));
                if self.pause_dirty {
                    self.pause_dirty = false;
                    self.refresh_pause()?;
                }
            }
            Pending::PauseSet {
                owner,
                request,
                expected,
            } => {
                if error < 0 {
                    return Ok(BackendEvent::CommandReply { id: owner, error });
                }
                // This fresh read belongs exclusively to the original command.
                // Never reuse a generic read admitted before the set reply.
                self.get(
                    c"pause",
                    Pending::PauseRead {
                        owner,
                        request,
                        expected,
                    },
                )?;
            }
            Pending::PauseRead {
                owner,
                request,
                expected,
            } => {
                if error < 0 || value != Some(NodeValue::Flag(expected)) {
                    return Err(MediaError::new(
                        "pause_property",
                        format!("invalid or mismatched fresh pause flag, mpv error={error}"),
                    ));
                }
                self.paused = expected;
                self.joint_pause = Some((
                    owner,
                    PauseObservation {
                        request: Some(request),
                        paused: expected,
                    },
                ));
                self.finish_joint_pause();
            }
            Pending::Metadata(field) => {
                let batch = self.metadata.as_mut().ok_or_else(|| {
                    MediaError::new("capture_metadata", "unexpected metadata reply")
                })?;
                batch.values[field] = (error >= 0).then_some(value).flatten();
                batch.remaining -= 1;
                if batch.remaining == 0 {
                    let batch = self.metadata.take().expect("completed metadata");
                    self.emitted.push_back(BackendEvent::Observed {
                        started: batch.started,
                        facts: observed_from_nodes(&batch.values, self.nominal_rate_source),
                    });
                    if let Some(started) = batch.rerun {
                        self.refresh_observed(started)?;
                    }
                }
            }
            #[cfg(test)]
            Pending::FixtureProperty(_) => {
                let value = value.filter(|_| error >= 0).ok_or_else(|| {
                    MediaError::new(
                        "fixture_property",
                        format!("invalid fresh property reply {id}, mpv error={error}"),
                    )
                })?;
                self.fixture_properties
                    .as_mut()
                    .ok_or_else(|| {
                        MediaError::new("fixture_property", "fixture property recorder absent")
                    })?
                    .push_back((id, value));
            }
        }
        self.drive_audio();
        Ok(self.emitted.pop_front().unwrap_or(BackendEvent::Other))
    }
    fn property_error(&self, id: u64, diagnostic: impl Into<String>) -> MediaError {
        let pause = self.pending.iter().any(|(request, pending)| {
            *request == id && matches!(pending, Pending::Pause | Pending::PauseRead { .. })
        });
        MediaError::new(
            if pause { "pause_property" } else { "mpv_event" },
            diagnostic,
        )
    }
}

fn observed_from_nodes(
    values: &[Option<NodeValue>; 2],
    nominal_rate_source: Source,
) -> ObservedFacts {
    let video = values[0].as_ref();
    let dimension = |name| match video.and_then(|video| video.field(name)) {
        Some(NodeValue::Int(value)) => u32::try_from(*value).ok(),
        _ => None,
    };
    let decoded_size = dimension("w")
        .zip(dimension("h"))
        .and_then(|(width, height)| FrameSize::new(width, height).ok())
        .map(|value| Observation {
            value,
            source: Source::MpvDecodedParams,
        });
    let decoded_pixel_format = video
        .and_then(|video| video.field("pixelformat"))
        .and_then(NodeValue::text)
        .filter(|text| !text.is_empty())
        .map(|text| Observation {
            value: text.to_owned(),
            source: Source::MpvDecodedParams,
        });
    let nominal_rate = match values[1].as_ref() {
        Some(NodeValue::Double(value)) if value.is_finite() && *value > 0.0 => Some(Observation {
            value: *value,
            source: nominal_rate_source,
        }),
        _ => None,
    };
    ObservedFacts {
        decoded_size,
        decoded_pixel_format,
        nominal_rate,
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
        self.generation = token.generation.get();
        self.owner_cancel = Some(stop.audio_cancel());
        if self.audio.enabled() {
            self.audio_status = AudioAvailability::Silent {
                reason: if self.paused {
                    AudioSilence::Paused
                } else {
                    self.output
                        .silence()
                        .cloned()
                        .map_or(AudioSilence::PendingRoute, AudioSilence::Output)
                },
            };
        }
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
            (c"untimed", c"yes"),
            (c"correct-pts", c"no"),
            (c"video-latency-hacks", c"yes"),
            (c"cache", c"no"),
            (c"demuxer-readahead-secs", c"0"),
        ];
        for &(name, value) in OPTIONS {
            check_stop(stop)?;
            handle.option(name, value)?;
        }
        check_stop(stop)?;
        let rate = self.input.requested().mode.rate;
        let fps = f64::from(rate.numerator()) / f64::from(rate.denominator());
        handle.option_double(c"container-fps-override", fps)?;
        check_stop(stop)?;
        handle.option(
            c"pause",
            if self.playback == InitialPlayback::Paused {
                c"yes"
            } else {
                c"no"
            },
        )?;
        handle.option(c"demuxer-lavf-format", c"v4l2")?;
        handle.option(c"demuxer-lavf-o", self.input.lavf_options())?;
        handle.option(c"audio", c"no")?;
        check_stop(stop)?;
        let xid = CString::new(token.xid.get().to_string())
            .map_err(|error| MediaError::new("mpv_xid", error.to_string()))?;
        handle.option(c"wid", &xid)?;
        check_stop(stop)?;
        // SAFETY: Live unique owner handle and static C log-level string. All
        // later log payloads are copied within their wait_event lifetime.
        let result =
            unsafe { (handle.functions.request_logs)(handle.raw.as_ptr(), c"info".as_ptr()) };
        handle.checked(result, "request info logs")?;
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
        self.nominal_rate_source = Source::MpvConfiguredContainerFps;
        Ok(())
    }
    fn submit(&mut self, id: RequestId, command: BackendCommand) -> Result<(), MediaError> {
        match command {
            BackendCommand::SetPaused { request, paused } => {
                if !paused {
                    return Err(MediaError::new(
                        "pause_admission",
                        "resume requires a fresh applied session",
                    ));
                }
                self.paused = true;
                self.retire_audio(AudioSilence::Paused);
                self.set(
                    c"pause",
                    &NodeValue::Flag(paused),
                    Pending::PauseSet {
                        owner: id.get(),
                        request,
                        expected: paused,
                    },
                )
            }
            BackendCommand::SetGain(gain) => {
                self.gain = gain;
                if !self.paused
                    && self.retirement.is_none()
                    && !self.terminal_audio
                    && let Some(owner) = &self.loopback
                    && let Err(error) = owner.submit(LoopbackCommand::SetGain(gain))
                {
                    self.terminal_audio = true;
                    self.retire_audio(AudioSilence::Failed(error));
                }
                self.emitted.push_back(BackendEvent::CommandReply {
                    id: id.get(),
                    error: 0,
                });
                Ok(())
            }
            BackendCommand::SetOutput(output) => {
                if output.revision().get() < self.output.revision().get() {
                    return Ok(());
                }
                if output.revision() == self.output.revision() {
                    if output == self.output {
                        return Ok(());
                    }
                    return Err(MediaError::new(
                        "output_intent",
                        "different plans share an output revision",
                    ));
                }
                self.output = output;
                if !self.paused
                    && self.retirement.is_none()
                    && let Some(owner) = &self.loopback
                {
                    if let Err(error) =
                        owner.submit(LoopbackCommand::SetOutput(self.output.clone()))
                    {
                        self.terminal_audio = true;
                        self.retire_audio(AudioSilence::Failed(error));
                    } else if let Some(epoch) = self.epoch {
                        self.publish_audio(AudioAvailability::Switching {
                            epoch,
                            revision: self.output.revision(),
                        });
                    }
                } else if self.audio.enabled()
                    && !self.terminal_audio
                    && !self.paused
                    && self.loopback.is_none()
                    && self.epoch.is_none()
                    && let Some(reason) = self.output.silence()
                {
                    self.publish_audio(AudioAvailability::Silent {
                        reason: AudioSilence::Output(reason.clone()),
                    });
                }
                Ok(())
            }
            BackendCommand::DetachAudio { epoch } => {
                if self.epoch == Some(epoch) {
                    self.retire_audio(AudioSilence::WaitingForSource(AudioError::RecordingLost {
                        detail: "selected source unavailable in authoritative observation".into(),
                    }));
                } else if self.last_audio_epoch != epoch.get() || self.retirement.is_some() {
                    return Err(MediaError::new("audio_admission", "stale detach epoch"));
                }
                self.emitted.push_back(BackendEvent::CommandReply {
                    id: id.get(),
                    error: 0,
                });
                Ok(())
            }
            BackendCommand::AttachAudio {
                epoch,
                source,
                stamp,
            } => {
                if self.epoch == Some(epoch)
                    && self.watch == stamp
                    && matches!(&self.audio, AudioSelection::Enabled { source: desired } if desired == &source)
                    && self.retirement.is_none()
                {
                    self.emitted.push_back(BackendEvent::CommandReply {
                        id: id.get(),
                        error: 0,
                    });
                    return Ok(());
                }
                self.initial_audio_checked = true;
                self.start_audio(epoch, source, stamp)?;
                self.emitted.push_back(BackendEvent::CommandReply {
                    id: id.get(),
                    error: 0,
                });
                Ok(())
            }
            BackendCommand::LoadInput => {
                self.load_id = Some(id.get());
                let handle = self.handle()?;
                let argv = [c"loadfile".as_ptr(), self.input.url().as_ptr(), ptr::null()];
                // SAFETY: Owner-only handle, exact cookie and copied argv.
                let result = unsafe {
                    (handle.functions.command_async)(handle.raw.as_ptr(), id.get(), argv.as_ptr())
                };
                handle.checked(result, "loadfile")
            }
        }
    }
    fn next_event(&mut self) -> Result<BackendEvent, MediaError> {
        self.drive_audio();
        if let Some(event) = self.emitted.pop_front() {
            return Ok(event);
        }
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
                if matches!(level.as_str(), "fatal" | "error" | "warn") {
                    if self.diagnostics.len() == 16 {
                        self.diagnostics.pop_front();
                    }
                    self.diagnostics.push_back(format!(
                        "{prefix}/{level}: {}",
                        text.chars().take(2048).collect::<String>()
                    ));
                }
                tracing::info!(
                    mpv_prefix = prefix,
                    mpv_level = level,
                    "{}",
                    text.trim_end()
                );
                Ok(BackendEvent::Other)
            }
            3 => {
                if event.data.is_null() {
                    return Err(
                        self.property_error(event.reply_userdata, "null property reply payload")
                    );
                }
                // SAFETY: GET_PROPERTY_REPLY payload matches pinned C layout.
                // NODE contents and name copied before any further mpv wait.
                let (name, value) = unsafe {
                    let property = &*event.data.cast::<MpvPropertyEvent>();
                    if property.name.is_null() {
                        return Err(self.property_error(event.reply_userdata, "null property name"));
                    }
                    let value = if property.format == 0 {
                        None
                    } else {
                        if property.format != 6 || property.data.is_null() {
                            return Err(self.property_error(
                                event.reply_userdata,
                                "unexpected property node payload",
                            ));
                        }
                        let node = &*property.data.cast::<MpvNode>();
                        if self.pending.iter().any(|(id, pending)| {
                            *id == event.reply_userdata
                                && matches!(pending, Pending::Pause | Pending::PauseRead { .. })
                        }) && node.format == 3
                            && !matches!(node.value.flag, 0 | 1)
                        {
                            return Err(
                                self.property_error(event.reply_userdata, "malformed pause flag")
                            );
                        }
                        Some(copy_node(node, 0).map_err(|error| {
                            self.property_error(event.reply_userdata, error.diagnostic)
                        })?)
                    };
                    (CStr::from_ptr(property.name).to_owned(), value)
                };
                self.reply(
                    event.reply_userdata,
                    ReplyKind::Property,
                    event.error,
                    Some(name),
                    value,
                )
            }
            4 => self.reply(
                event.reply_userdata,
                ReplyKind::SetProperty,
                event.error,
                None,
                None,
            ),
            5 => {
                // Command result is owned before leaving native event lifetime,
                // even commands whose scalar outcome alone is sufficient.
                let value = if event.data.is_null() {
                    None
                } else {
                    // SAFETY: COMMAND_REPLY data is pinned mpv_event_command.
                    Some(unsafe { copy_node(&(*event.data.cast::<MpvCommandEvent>()).result, 0)? })
                };
                self.reply(
                    event.reply_userdata,
                    ReplyKind::Command,
                    event.error,
                    None,
                    value,
                )
            }
            7 => {
                if event.data.is_null() {
                    return Err(MediaError::new("mpv_event", "null END_FILE payload"));
                }
                // SAFETY: END_FILE data matches the complete pinned repr(C)
                // payload. Copy only scalar reason/error; never retain its pointer.
                let end = unsafe { &*event.data.cast::<MpvEndFile>() };
                self.video_ended = true;
                Ok(BackendEvent::EndFile {
                    reason: end.reason,
                    error: end.error,
                })
            }
            8 => Ok(BackendEvent::FileLoaded),
            17 => Ok(BackendEvent::VideoReconfig),
            18 => Ok(BackendEvent::Other),
            21 => {
                self.video_started = true;
                self.begin_audio_if_ready()?;
                Ok(BackendEvent::PlaybackRestart)
            }
            24 => Ok(BackendEvent::QueueOverflow),
            _ => Ok(BackendEvent::Other),
        }
    }
    fn refresh_pause(&mut self) -> Result<Option<bool>, MediaError> {
        if self.pause_pending {
            self.pause_dirty = true;
        } else {
            self.get(c"pause", Pending::Pause)?;
            self.pause_pending = true;
        }
        Ok(None)
    }
    fn refresh_observed(&mut self, started: bool) -> Result<Option<ObservedFacts>, MediaError> {
        if let Some(batch) = &mut self.metadata {
            batch.rerun = Some(batch.rerun.unwrap_or(false) || started);
        } else {
            self.metadata = Some(MetadataBatch {
                values: [None, None],
                remaining: 2,
                started,
                rerun: None,
            });
            self.get(c"video-params", Pending::Metadata(0))?;
            self.get(c"container-fps", Pending::Metadata(1))?;
            self.refresh_pause()?;
        }
        Ok(None)
    }
    fn diagnostic(&self) -> Option<String> {
        if self.diagnostics.is_empty() {
            None
        } else {
            Some(
                self.diagnostics
                    .iter()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join("\n"),
            )
        }
    }
    fn audio_status(&self) -> AudioAvailability {
        self.audio_status.clone()
    }
    fn initial_playback(&self) -> InitialPlayback {
        self.playback
    }
    fn audio_epoch(&self) -> Option<AudioEpoch> {
        self.epoch
    }
    fn shutdown(&mut self) -> Result<(), MediaError> {
        // Retain the video handle/native lease until actual child reap AND
        // audio worker join. A blocked owner keeps this worker alive.
        if let Some(owner) = &self.loopback {
            owner.stop();
        }
        let mut outcome = Ok(());
        while let Some(owner) = &mut self.loopback {
            if let Some(joined) = owner.try_join() {
                outcome =
                    joined.map_err(|error| MediaError::new("audio_cancel", error.to_string()));
                self.loopback = None;
                break;
            }
            std::thread::park_timeout(std::time::Duration::from_millis(10));
        }
        self.epoch = None;
        self.retirement = None;
        drop(self.handle.take()); // real mpv_terminate_destroy on its owner thread
        self.pending.clear();
        outcome
    }
}

#[cfg(test)]
fn fixture_watch_stamp() -> WatchStamp {
    WatchStamp {
        watch: crate::domain::capture::WatchId::new(1).unwrap(),
        epoch: crate::domain::capture::ObservationEpoch::new(1).unwrap(),
    }
}

/// Isolated observations of actual fixture handle lifetimes, never scripted acks.
#[cfg(test)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum FixtureMilestone {
    Created {
        generation: super::controller::Generation,
    },
    /// Gain records the checked initial configuration; probe reads prove values.
    Initialized {
        generation: super::controller::Generation,
        gain: PlaybackGain,
    },
    Destroyed {
        generation: super::controller::Generation,
    },
}

#[cfg(test)]
#[derive(Clone, Default)]
pub(crate) struct FixtureRecorder(std::sync::Arc<std::sync::Mutex<Vec<FixtureMilestone>>>);

#[cfg(test)]
impl FixtureRecorder {
    fn record(&self, milestone: FixtureMilestone) -> Result<(), MediaError> {
        self.0
            .lock()
            .map_err(|_| {
                MediaError::new("fixture_recorder", "fixture milestone recorder poisoned")
            })?
            .push(milestone);
        Ok(())
    }

    pub(crate) fn snapshot(&self) -> Result<Vec<FixtureMilestone>, MediaError> {
        self.0
            .lock()
            .map(|milestones| milestones.clone())
            .map_err(|_| MediaError::new("fixture_recorder", "fixture milestone recorder poisoned"))
    }
}

/// Finite-file qualification adapter. Only initialization and input selection
/// differ from production; control transactions and event/lifetime logic do not.
#[cfg(test)]
pub(crate) struct FixtureBackend {
    inner: MpvBackend,
    fixture: CString,
    recorder: Option<FixtureRecorder>,
    generation: Option<super::controller::Generation>,
}

#[cfg(test)]
impl FixtureBackend {
    /// Construct on the owner thread, using a synthetically validated InputSpec
    /// only for requested metadata. No capture device or Pulse source is opened.
    pub(crate) fn new(
        prefix: String,
        fixture: PathBuf,
        input: InputSpec,
        gain: PlaybackGain,
        playback: InitialPlayback,
        watch: WatchStamp,
        recorder: Option<FixtureRecorder>,
    ) -> Result<Self, MediaError> {
        use std::os::unix::ffi::OsStrExt;
        if !fixture.is_absolute() || !fixture.is_file() {
            return Err(MediaError::new(
                "fixture_path",
                "FURAMI_PLAYBACK_FIXTURE must be an absolute existing file",
            ));
        }
        let fixture = fixture
            .canonicalize()
            .map_err(|error| MediaError::new("fixture_path", error.to_string()))?;
        let fixture = CString::new(fixture.as_os_str().as_bytes())
            .map_err(|error| MediaError::new("fixture_path", error.to_string()))?;
        Ok(Self {
            inner: MpvBackend::new(
                prefix,
                input,
                AudioSelection::default(),
                gain,
                playback,
                watch,
                OutputPlan::Silent {
                    revision: crate::domain::output::OutputRevision::first(),
                    reason: crate::domain::output::OutputSilence::NoAvailableOutput,
                },
            ),
            fixture,
            recorder,
            generation: None,
        })
    }

    fn record(&self, milestone: FixtureMilestone) -> Result<(), MediaError> {
        self.recorder
            .as_ref()
            .map_or(Ok(()), |recorder| recorder.record(milestone))
    }
}

#[cfg(test)]
impl OwnerBackend for FixtureBackend {
    fn initialize(&mut self, token: SurfaceToken, stop: &StopFlag) -> Result<(), MediaError> {
        check_stop(stop)?;
        if self.inner.handle.is_some() {
            return Err(MediaError::new(
                "fixture_handle",
                "fixture already initialized",
            ));
        }
        let path = qualified_library(&self.inner.prefix)?;
        self.inner.handle = Some(Handle::create(&path, stop)?);
        self.generation = Some(token.generation);
        self.record(FixtureMilestone::Created {
            generation: token.generation,
        })?;
        let handle = self.inner.handle()?;
        const OPTIONS: &[(&CStr, &CStr)] = &[
            (c"config", c"no"),
            (c"load-scripts", c"no"),
            (c"input-default-bindings", c"no"),
            (c"input-vo-keyboard", c"no"),
            (c"input-terminal", c"no"),
            (c"input-cursor", c"no"),
            (c"terminal", c"no"),
            (c"vo", c"null"),
            (c"audio", c"no"),
        ];
        for &(name, value) in OPTIONS {
            check_stop(stop)?;
            handle.option(name, value)?;
        }
        handle.option(
            c"pause",
            if self.inner.playback == InitialPlayback::Paused {
                c"yes"
            } else {
                c"no"
            },
        )?;
        check_stop(stop)?;
        // SAFETY: The genuine owner-thread handle is live; options above were
        // checked before initialization; no native window or audio owner exists.
        let result = unsafe { (handle.functions.initialize)(handle.raw.as_ptr()) };
        handle.checked(result, "fixture initialize")?;
        check_stop(stop)?;
        let version = handle.property(c"mpv-version")?;
        if version.trim() != "mpv v0.41.0" {
            return Err(MediaError::new(
                "mpv_version",
                format!("runtime mpv-version {version:?}, expected mpv v0.41.0"),
            ));
        }
        self.record(FixtureMilestone::Initialized {
            generation: token.generation,
            gain: self.inner.gain,
        })
    }

    fn submit(&mut self, id: RequestId, command: BackendCommand) -> Result<(), MediaError> {
        if let BackendCommand::SetPaused { request, paused } = &command {
            // Only this finite-video fixture permits unpause for time-pos checks.
            return self.inner.set(
                c"pause",
                &NodeValue::Flag(*paused),
                Pending::PauseSet {
                    owner: id.get(),
                    request: *request,
                    expected: *paused,
                },
            );
        }
        if command != BackendCommand::LoadInput {
            return self.inner.submit(id, command);
        }
        self.inner.load_id = Some(id.get());
        let handle = self.inner.handle()?;
        let arguments = [c"loadfile".as_ptr(), self.fixture.as_ptr(), ptr::null()];
        // SAFETY: Owner-only live handle, checked request ID and owned,
        // NUL-terminated argv. libmpv copies the finite-file path on submission.
        let result = unsafe {
            (handle.functions.command_async)(handle.raw.as_ptr(), id.get(), arguments.as_ptr())
        };
        handle.checked(result, "fixture loadfile")
    }

    fn next_event(&mut self) -> Result<BackendEvent, MediaError> {
        self.inner.next_event()
    }

    fn refresh_pause(&mut self) -> Result<Option<bool>, MediaError> {
        self.inner.refresh_pause()
    }

    fn refresh_observed(&mut self, started: bool) -> Result<Option<ObservedFacts>, MediaError> {
        self.inner.refresh_observed(started)
    }

    fn diagnostic(&self) -> Option<String> {
        self.inner.diagnostic()
    }

    fn audio_status(&self) -> AudioAvailability {
        self.inner.audio_status()
    }
    fn initial_playback(&self) -> InitialPlayback {
        self.inner.playback
    }

    fn shutdown(&mut self) -> Result<(), MediaError> {
        // This returns only after the real Handle::drop/terminate_destroy.
        self.inner.shutdown()?;
        if let Some(generation) = self.generation.take() {
            self.record(FixtureMilestone::Destroyed { generation })?;
        }
        Ok(())
    }
}

#[cfg(test)]
impl Drop for FixtureBackend {
    fn drop(&mut self) {
        // Assertion/error/timeout paths must retire the same genuine handle.
        if let Err(error) = self.shutdown() {
            tracing::error!(%error, "fixture_shutdown_failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

    fn fixture_input() -> InputSpec {
        use crate::{
            capture::{input::CaptureSelection, linux},
            domain::capture::{CaptureMode, CapturedFourCc, FrameRate},
        };
        let mode = CaptureMode {
            captured_fourcc: CapturedFourCc::from_bytes(*b"NV12"),
            size: FrameSize::new(320, 240).unwrap(),
            rate: FrameRate::new(30, 1).unwrap(),
        };
        let catalog = linux::session_fixture(&["/dev/video0"], mode);
        CaptureSelection::from_snapshot(&catalog, Path::new("/dev/video0"), mode)
            .unwrap()
            .validate_snapshot(&catalog)
            .unwrap()
    }

    fn fixture_token(generation: u64) -> SurfaceToken {
        use super::super::controller::{Generation, X11WindowId};
        SurfaceToken {
            generation: Generation::new(generation).unwrap(),
            // Null output never reads this test-only surface ID.
            xid: X11WindowId::new(generation).unwrap(),
        }
    }

    fn fixture_wait(
        backend: &mut FixtureBackend,
        transition: &str,
        completed: impl FnMut(&mut FixtureBackend, BackendEvent) -> Result<bool, MediaError>,
    ) -> Result<(), MediaError> {
        fixture_wait_until(
            backend,
            transition,
            std::time::Instant::now() + FIXTURE_WAIT,
            completed,
        )
    }

    fn fixture_wait_until(
        backend: &mut FixtureBackend,
        transition: &str,
        deadline: std::time::Instant,
        mut completed: impl FnMut(&mut FixtureBackend, BackendEvent) -> Result<bool, MediaError>,
    ) -> Result<(), MediaError> {
        loop {
            if std::time::Instant::now() >= deadline {
                // Genuine terminate_destroy runs even on a transition timeout.
                backend.shutdown()?;
                return Err(MediaError::new(
                    "fixture_timeout",
                    format!("{transition} did not complete within 10 seconds"),
                ));
            }
            let event = backend.next_event()?;
            match &event {
                BackendEvent::CommandReply { id, error } if *error < 0 => {
                    return Err(MediaError::new(
                        "fixture_command",
                        format!("request {id} failed with mpv error {error}"),
                    ));
                }
                BackendEvent::EndFile { .. }
                | BackendEvent::Shutdown
                | BackendEvent::QueueOverflow => {
                    return Err(MediaError::new(
                        "fixture_event",
                        format!("unexpected {event:?} during {transition}"),
                    ));
                }
                _ => {}
            }
            let idle = event == BackendEvent::None;
            if completed(backend, event)? {
                return Ok(());
            }
            if idle {
                thread::park_timeout(std::time::Duration::from_millis(5));
            }
        }
    }

    fn fixture_load(backend: &mut FixtureBackend) -> Result<(), MediaError> {
        let id = RequestId::for_test(1);
        backend.submit(id, BackendCommand::LoadInput)?;
        let mut loaded = false;
        let mut started = false;
        let mut observed = false;
        fixture_wait(
            backend,
            "file playback and fresh decoded metadata",
            |backend, event| {
                match event {
                    BackendEvent::CommandReply {
                        id: reply,
                        error: 0,
                    } if reply == id.get() => {
                        loaded = true;
                    }
                    BackendEvent::PlaybackRestart if !started => {
                        started = true;
                        backend.refresh_observed(true)?;
                    }
                    BackendEvent::Observed {
                        started: true,
                        facts,
                    } => {
                        let size = facts.decoded_size.as_ref().map(|value| value.value);
                        let rate = facts.nominal_rate.as_ref().map(|value| value.value);
                        if size != Some(FrameSize::new(320, 240).unwrap()) || rate != Some(30.0) {
                            return Err(MediaError::new(
                                "fixture_metadata",
                                format!("expected decoded 320x240 at nominal 30fps, got {facts:?}"),
                            ));
                        }
                        observed = true;
                    }
                    _ => {}
                }
                Ok(loaded && started && observed)
            },
        )
    }

    fn fixture_property(
        backend: &mut FixtureBackend,
        name: &'static CStr,
    ) -> Result<NodeValue, MediaError> {
        fixture_property_until(backend, name, std::time::Instant::now() + FIXTURE_WAIT)
    }

    fn fixture_property_until(
        backend: &mut FixtureBackend,
        name: &'static CStr,
        deadline: std::time::Instant,
    ) -> Result<NodeValue, MediaError> {
        backend
            .inner
            .fixture_properties
            .get_or_insert_with(VecDeque::new);
        backend.inner.get(name, Pending::FixtureProperty(name))?;
        let id = backend.inner.last_internal;
        let mut value = None;
        fixture_wait_until(
            backend,
            &format!("fresh {}", name.to_string_lossy()),
            deadline,
            |backend, _| {
                let replies = backend
                    .inner
                    .fixture_properties
                    .as_mut()
                    .expect("probe enabled");
                if let Some(index) = replies.iter().position(|(reply, _)| *reply == id) {
                    value = Some(replies.remove(index).expect("matched property reply").1);
                }
                Ok(value.is_some())
            },
        )?;
        Ok(value.expect("wait completed only with exact property reply"))
    }

    fn fixture_time(backend: &mut FixtureBackend) -> Result<f64, MediaError> {
        fixture_time_until(backend, std::time::Instant::now() + FIXTURE_WAIT)
    }

    fn fixture_time_until(
        backend: &mut FixtureBackend,
        deadline: std::time::Instant,
    ) -> Result<f64, MediaError> {
        match fixture_property_until(backend, c"time-pos", deadline)? {
            NodeValue::Double(value) if value.is_finite() && value >= 0.0 => Ok(value),
            value => Err(MediaError::new(
                "fixture_time",
                format!("expected finite nonnegative time-pos, got {value:?}"),
            )),
        }
    }

    fn fixture_advancing(backend: &mut FixtureBackend) -> Result<(), MediaError> {
        let deadline = std::time::Instant::now() + FIXTURE_WAIT;
        let first = fixture_time_until(backend, deadline)?;
        loop {
            if std::time::Instant::now() >= deadline {
                backend.shutdown()?;
                return Err(MediaError::new(
                    "fixture_timeout",
                    "time-pos did not advance within 10 seconds",
                ));
            }
            thread::park_timeout(std::time::Duration::from_millis(100));
            let latest = fixture_time_until(backend, deadline)?;
            if latest > first + 0.1 {
                eprintln!("fixture time-pos advanced: {first:.6} -> {latest:.6}");
                return Ok(());
            }
        }
    }

    fn fixture_pause(
        backend: &mut FixtureBackend,
        id: u64,
        paused: bool,
    ) -> Result<(), MediaError> {
        let request = PauseRequestId::new(id).unwrap();
        let owner = RequestId::for_test(id);
        backend.submit(owner, BackendCommand::SetPaused { request, paused })?;
        let mut observed = false;
        let mut replied = false;
        fixture_wait(backend, "correlated pause set/readback", |_, event| {
            match event {
                BackendEvent::PauseObserved(PauseObservation {
                    request: Some(reply),
                    paused: value,
                }) if reply == request && value == paused => observed = true,
                BackendEvent::CommandReply {
                    id: reply,
                    error: 0,
                } if reply == owner.get() => {
                    replied = true;
                }
                _ => {}
            }
            Ok(observed && replied)
        })?;
        assert_eq!(
            fixture_property(backend, c"pause")?,
            NodeValue::Flag(paused)
        );
        Ok(())
    }

    fn fixture_gain(
        backend: &mut FixtureBackend,
        id: u64,
        gain: PlaybackGain,
    ) -> Result<(), MediaError> {
        let owner = RequestId::for_test(id);
        backend.submit(owner, BackendCommand::SetGain(gain))?;
        fixture_wait(backend, "aggregate gain mailbox", |_, event| {
            Ok(matches!(event, BackendEvent::CommandReply { id, error: 0 } if id == owner.get()))
        })?;
        assert_eq!(backend.inner.gain, gain);
        Ok(())
    }

    #[test]
    #[ignore = "requires frozen libmpv and FURAMI_PLAYBACK_FIXTURE"]
    fn real_libmpv_playback_controls() {
        let prefix = std::env::var("FURAMI_MEDIA_PREFIX").expect("set frozen FURAMI_MEDIA_PREFIX");
        let fixture = PathBuf::from(
            std::env::var_os("FURAMI_PLAYBACK_FIXTURE").expect("set FURAMI_PLAYBACK_FIXTURE"),
        );
        let recorder = FixtureRecorder::default();
        let stop = StopFlag::for_test();
        let latest_gain = PlaybackGain::new(80, false).unwrap();
        {
            let mut backend = FixtureBackend::new(
                prefix.clone(),
                fixture.clone(),
                fixture_input(),
                PlaybackGain::default(),
                InitialPlayback::Live,
                fixture_watch_stamp(),
                Some(recorder.clone()),
            )
            .unwrap();
            let result = (|| -> Result<(), MediaError> {
                backend.initialize(fixture_token(1), &stop)?;
                fixture_load(&mut backend)?;
                let handle = backend.inner.handle()?.raw;
                fixture_advancing(&mut backend)?;
                fixture_pause(&mut backend, 2, true)?;
                let paused = fixture_time(&mut backend)?;
                for _ in 0..3 {
                    thread::park_timeout(std::time::Duration::from_millis(200));
                    assert_eq!(
                        fixture_time(&mut backend)?,
                        paused,
                        "paused time-pos changed"
                    );
                }
                eprintln!("fixture paused time-pos stable across four fresh reads: {paused:.6}");
                fixture_gain(&mut backend, 3, PlaybackGain::new(80, true).unwrap())?;
                fixture_gain(&mut backend, 4, latest_gain)?;
                assert_eq!(
                    backend.inner.handle()?.raw,
                    handle,
                    "gain replaced the handle"
                );
                // Unpause is permitted only for this finite file, never live input.
                fixture_pause(&mut backend, 5, false)?;
                fixture_advancing(&mut backend)?;
                assert_eq!(
                    backend.inner.handle()?.raw,
                    handle,
                    "controls replaced the handle"
                );
                Ok(())
            })();
            backend
                .shutdown()
                .expect("genuine first handle destruction");
            result.expect("real finite-file controls");
        }
        assert!(matches!(recorder.snapshot().unwrap().last(),
            Some(FixtureMilestone::Destroyed { generation }) if generation.get() == 1));
        {
            let mut backend = FixtureBackend::new(
                prefix,
                fixture,
                fixture_input(),
                latest_gain,
                InitialPlayback::Live,
                fixture_watch_stamp(),
                Some(recorder.clone()),
            )
            .unwrap();
            let result = (|| -> Result<(), MediaError> {
                backend.initialize(fixture_token(2), &stop)?;
                fixture_load(&mut backend)?;
                assert_eq!(backend.inner.gain, latest_gain);
                assert_eq!(
                    fixture_property(&mut backend, c"pause")?,
                    NodeValue::Flag(false)
                );
                fixture_advancing(&mut backend)?;
                Ok(())
            })();
            backend
                .shutdown()
                .expect("genuine second handle destruction");
            result.expect("fresh finite-file handle retained latest gain and started unpaused");
        }
        let milestones = recorder.snapshot().unwrap();
        assert_eq!(
            milestones,
            vec![
                FixtureMilestone::Created {
                    generation: fixture_token(1).generation
                },
                FixtureMilestone::Initialized {
                    generation: fixture_token(1).generation,
                    gain: PlaybackGain::default()
                },
                FixtureMilestone::Destroyed {
                    generation: fixture_token(1).generation
                },
                FixtureMilestone::Created {
                    generation: fixture_token(2).generation
                },
                FixtureMilestone::Initialized {
                    generation: fixture_token(2).generation,
                    gain: latest_gain
                },
                FixtureMilestone::Destroyed {
                    generation: fixture_token(2).generation
                },
            ]
        );
        eprintln!("real libmpv finite-file milestones: {milestones:?}");
        eprintln!(
            "finite time/property qualification only; no audible silence, live freshness, or Qt/XID claim"
        );
    }

    #[test]
    fn copied_command_and_property_nodes_outlive_native_payloads() {
        let mut text = CString::new("source").unwrap();
        let native = MpvNode {
            value: MpvNodeValue {
                string: text.as_ptr().cast_mut(),
            },
            format: 1,
        };
        // SAFETY: Native node points to live CString, and format matches union member.
        let copied = unsafe { copy_node(&native, 0) }.unwrap();
        text = CString::new("reused").unwrap();
        assert_eq!(text.to_bytes(), b"reused");
        assert_eq!(copied, NodeValue::String(CString::new("source").unwrap()));
    }

    #[test]
    fn nested_owned_nodes_roundtrip_without_native_borrow_escape() {
        let expected = NodeValue::Map(vec![
            (
                c"flags".to_owned(),
                NodeValue::Array(vec![NodeValue::Flag(true), NodeValue::Int(8)]),
            ),
            (
                c"name".to_owned(),
                NodeValue::String(c"exact,capture=tag".to_owned()),
            ),
        ]);
        let copied = {
            let native = NativeNode::new(&expected);
            // SAFETY: Native borrowed graph remains live through deep copy.
            unsafe { copy_node(&native.node, 0) }.unwrap()
        };
        assert_eq!(copied, expected);
        assert_eq!(std::mem::size_of::<MpvNode>(), 16);
        assert_eq!(std::mem::size_of::<MpvNodeList>(), 24);
        assert_eq!(std::mem::size_of::<MpvPropertyEvent>(), 24);
        assert_eq!(std::mem::size_of::<MpvCommandEvent>(), 16);
    }

    #[test]
    fn async_metadata_retains_decoder_provenance_and_never_certifies_capture_fourcc_or_cadence() {
        let values = [
            Some(NodeValue::Map(vec![
                (c"w".to_owned(), NodeValue::Int(2560)),
                (c"h".to_owned(), NodeValue::Int(1440)),
                (
                    c"pixelformat".to_owned(),
                    NodeValue::String(c"nv12".to_owned()),
                ),
            ])),
            Some(NodeValue::Double(59.94)),
        ];
        for source in [Source::MpvContainerFps, Source::MpvConfiguredContainerFps] {
            let observed = observed_from_nodes(&values, source);
            assert_eq!(
                observed.decoded_pixel_format.as_ref().unwrap().source,
                Source::MpvDecodedParams
            );
            assert_eq!(observed.nominal_rate.as_ref().unwrap().source, source);
            for unavailable in [
                None,
                Some(NodeValue::None),
                Some(NodeValue::Int(60)),
                Some(NodeValue::String(c"60".to_owned())),
                Some(NodeValue::Double(f64::NAN)),
                Some(NodeValue::Double(f64::INFINITY)),
                Some(NodeValue::Double(f64::NEG_INFINITY)),
                Some(NodeValue::Double(0.0)),
                Some(NodeValue::Double(-60.0)),
            ] {
                assert!(
                    observed_from_nodes(&[None, unavailable], source)
                        .nominal_rate
                        .is_none()
                );
            }
        }
    }
}

#[cfg(test)]
mod reply_tests {
    use super::*;
    use crate::{
        capture::{input::CaptureSelection, linux},
        domain::capture::{CaptureMode, CapturedFourCc, FrameRate},
    };

    fn backend() -> MpvBackend {
        let mode = CaptureMode {
            captured_fourcc: CapturedFourCc::from_bytes(*b"NV12"),
            size: FrameSize::new(2560, 1440).unwrap(),
            rate: FrameRate::new(60, 1).unwrap(),
        };
        let snapshot = linux::session_fixture(&["/dev/video0"], mode);
        let selection =
            CaptureSelection::from_snapshot(&snapshot, Path::new("/dev/video0"), mode).unwrap();
        MpvBackend::new(
            String::new(),
            selection.validate_snapshot(&snapshot).unwrap(),
            AudioSelection::default(),
            PlaybackGain::default(),
            InitialPlayback::Live,
            fixture_watch_stamp(),
            OutputPlan::Silent {
                revision: crate::domain::output::OutputRevision::first(),
                reason: crate::domain::output::OutputSilence::NoAvailableOutput,
            },
        )
    }

    #[test]
    fn metadata_property_errors_remain_unverified_with_either_rate_provenance() {
        for source in [Source::MpvContainerFps, Source::MpvConfiguredContainerFps] {
            for error in [0, -10] {
                let mut backend = backend();
                backend.nominal_rate_source = source;
                backend.property_requests = Some(Vec::new());
                backend.refresh_observed(true).unwrap();
                let requests = backend.property_requests.as_ref().unwrap();
                let video_id = requests[0].0;
                let rate_id = requests[1].0;
                backend
                    .reply(
                        video_id,
                        ReplyKind::Property,
                        -10,
                        Some(c"video-params".to_owned()),
                        None,
                    )
                    .unwrap();
                let event = backend
                    .reply(
                        rate_id,
                        ReplyKind::Property,
                        error,
                        Some(c"container-fps".to_owned()),
                        Some(NodeValue::Double(60.0)),
                    )
                    .unwrap();
                let BackendEvent::Observed {
                    facts,
                    started: true,
                } = event
                else {
                    panic!("completed metadata must reach the session consumer");
                };
                let facts = super::super::session::SessionFacts::verify(
                    backend.input.requested().clone(),
                    facts,
                )
                .unwrap();
                assert_eq!(
                    facts.verification.nominal_rate,
                    if error < 0 {
                        super::super::session::VerificationStatus::Unverified
                    } else if source == Source::MpvConfiguredContainerFps {
                        super::super::session::VerificationStatus::Configured
                    } else {
                        super::super::session::VerificationStatus::Approximate
                    }
                );
                assert!(facts.observed.decoded_size.is_none());
                if error < 0 {
                    assert!(facts.observed.nominal_rate.is_none());
                } else {
                    assert_eq!(facts.observed.nominal_rate.as_ref().unwrap().source, source);
                }
            }
        }
    }

    #[test]
    fn pause_set_success_submits_new_read_separate_from_existing_generic_refresh() {
        let mut backend = backend();
        backend.property_requests = Some(Vec::new());
        let request = PauseRequestId::new(6).unwrap();
        backend.refresh_pause().unwrap();
        let generic = backend.property_requests.as_ref().unwrap()[0].0;
        backend
            .submit(
                RequestId::for_test(20),
                BackendCommand::SetPaused {
                    request,
                    paused: true,
                },
            )
            .unwrap();
        let set = backend.property_requests.as_ref().unwrap()[1].0;
        assert_eq!(
            backend.property_requests.as_ref().unwrap()[1].2,
            Some(NodeValue::Flag(true))
        );
        assert_eq!(
            backend
                .reply(set, ReplyKind::SetProperty, 0, None, None)
                .unwrap(),
            BackendEvent::Other
        );
        let requests = backend.property_requests.as_ref().unwrap();
        assert_eq!(requests.len(), 3);
        let read = requests[2].0;
        assert_ne!(read, generic);
        assert_ne!(read, set);
        assert_eq!(requests[2].1, c"pause");
        assert_eq!(requests[2].2, None);
        assert_eq!(
            backend
                .reply(
                    generic,
                    ReplyKind::Property,
                    0,
                    Some(c"pause".to_owned()),
                    Some(NodeValue::Flag(false))
                )
                .unwrap(),
            BackendEvent::PauseObserved(PauseObservation {
                request: None,
                paused: false
            })
        );
        assert_eq!(
            backend
                .reply(
                    read,
                    ReplyKind::Property,
                    0,
                    Some(c"pause".to_owned()),
                    Some(NodeValue::Flag(true))
                )
                .unwrap(),
            BackendEvent::PauseObserved(PauseObservation {
                request: Some(request),
                paused: true
            })
        );
        assert_eq!(
            backend.emitted.pop_front(),
            Some(BackendEvent::CommandReply { id: 20, error: 0 })
        );
    }

    #[test]
    fn correlated_pause_read_requires_exact_kind_id_name_and_expected_flag() {
        let request = PauseRequestId::new(3).unwrap();
        let id = (1 << 63) + 9;
        let mut backend = backend();
        backend.pending.push((
            id,
            Pending::PauseRead {
                owner: 17,
                request,
                expected: true,
            },
        ));
        for (reply, kind) in [
            (id + 1, ReplyKind::Property),
            (id, ReplyKind::Command),
            (id, ReplyKind::SetProperty),
        ] {
            assert_eq!(
                backend
                    .reply(
                        reply,
                        kind,
                        0,
                        Some(c"pause".to_owned()),
                        Some(NodeValue::Flag(true))
                    )
                    .unwrap(),
                BackendEvent::Other
            );
            assert_eq!(backend.pending.len(), 1);
        }
        assert_eq!(
            backend
                .reply(
                    id,
                    ReplyKind::Property,
                    0,
                    Some(c"pause".to_owned()),
                    Some(NodeValue::Flag(true))
                )
                .unwrap(),
            BackendEvent::PauseObserved(PauseObservation {
                request: Some(request),
                paused: true
            })
        );
        assert_eq!(
            backend.emitted.pop_front(),
            Some(BackendEvent::CommandReply { id: 17, error: 0 })
        );
        assert!(backend.pending.is_empty());
        assert_eq!(
            backend
                .reply(
                    id,
                    ReplyKind::Property,
                    0,
                    Some(c"pause".to_owned()),
                    Some(NodeValue::Flag(false))
                )
                .unwrap(),
            BackendEvent::Other
        );
    }

    #[test]
    fn correlated_pause_invalid_or_failed_read_is_pause_property_failure() {
        for (error, name, value) in [
            (-5, Some(c"pause".to_owned()), Some(NodeValue::Flag(true))),
            (0, Some(c"mute".to_owned()), Some(NodeValue::Flag(true))),
            (0, None, Some(NodeValue::Flag(true))),
            (0, Some(c"pause".to_owned()), Some(NodeValue::Int(1))),
            (0, Some(c"pause".to_owned()), Some(NodeValue::Flag(false))),
            (0, Some(c"pause".to_owned()), None),
        ] {
            let mut backend = backend();
            let id = (1 << 63) + 10;
            backend.pending.push((
                id,
                Pending::PauseRead {
                    owner: 18,
                    request: PauseRequestId::new(4).unwrap(),
                    expected: true,
                },
            ));
            assert_eq!(
                backend
                    .reply(id, ReplyKind::Property, error, name, value)
                    .unwrap_err()
                    .code,
                "pause_property"
            );
            assert!(backend.emitted.is_empty());
        }
    }

    #[test]
    fn pause_set_wrong_kind_or_id_cannot_complete_and_negative_reply_keeps_outer_identity() {
        let mut backend = backend();
        let id = (1 << 63) + 11;
        backend.pending.push((
            id,
            Pending::PauseSet {
                owner: 19,
                request: PauseRequestId::new(5).unwrap(),
                expected: true,
            },
        ));
        assert_eq!(
            backend
                .reply(
                    id,
                    ReplyKind::Property,
                    0,
                    Some(c"pause".to_owned()),
                    Some(NodeValue::Flag(true))
                )
                .unwrap(),
            BackendEvent::Other
        );
        assert_eq!(
            backend
                .reply(id + 1, ReplyKind::SetProperty, 0, None, None)
                .unwrap(),
            BackendEvent::Other
        );
        assert_eq!(
            backend
                .reply(19, ReplyKind::Command, 0, None, None)
                .unwrap(),
            BackendEvent::Other
        );
        assert_eq!(backend.pending.len(), 1);
        assert_eq!(
            backend
                .reply(id, ReplyKind::SetProperty, -5, None, None)
                .unwrap(),
            BackendEvent::CommandReply { id: 19, error: -5 }
        );
        assert!(backend.pending.is_empty());
        assert_eq!(
            backend
                .reply(id, ReplyKind::SetProperty, 0, None, None)
                .unwrap(),
            BackendEvent::Other
        );
    }

    #[test]
    fn live_property_pending_survives_wrong_kind_and_late_duplicate_cannot_change_pause() {
        let mut backend = backend();
        let id = (1 << 63) + 7;
        backend.pending.push((id, Pending::Pause));
        backend.pause_pending = true;
        assert_eq!(
            backend
                .reply(id, ReplyKind::Command, 0, None, None)
                .unwrap(),
            BackendEvent::Other
        );
        assert_eq!(backend.pending.len(), 1);
        assert_eq!(
            backend
                .reply(
                    id + 1,
                    ReplyKind::Property,
                    0,
                    Some(c"pause".to_owned()),
                    Some(NodeValue::Flag(true))
                )
                .unwrap(),
            BackendEvent::Other
        );
        assert_eq!(backend.pending.len(), 1);
        assert_eq!(
            backend
                .reply(
                    id,
                    ReplyKind::Property,
                    0,
                    Some(c"pause".to_owned()),
                    Some(NodeValue::Flag(false))
                )
                .unwrap(),
            BackendEvent::PauseObserved(PauseObservation {
                request: None,
                paused: false
            })
        );
        assert!(backend.pending.is_empty());
        assert_eq!(
            backend
                .reply(
                    id,
                    ReplyKind::Property,
                    0,
                    Some(c"pause".to_owned()),
                    Some(NodeValue::Flag(true))
                )
                .unwrap(),
            BackendEvent::Other
        );
    }

    #[test]
    fn matched_property_with_wrong_name_is_terminal_not_accepted_data() {
        let mut backend = backend();
        let id = (1 << 63) + 8;
        backend.pending.push((id, Pending::Pause));
        assert_eq!(
            backend
                .reply(
                    id,
                    ReplyKind::Property,
                    0,
                    Some(c"mute".to_owned()),
                    Some(NodeValue::Flag(true))
                )
                .unwrap_err()
                .code,
            "pause_property"
        );
        assert_eq!(backend.pending.len(), 1);
    }

    fn enabled_backend() -> (
        MpvBackend,
        AudioSourceIdentity,
        crate::domain::output::LiveSinkTarget,
    ) {
        let mut backend = backend();
        let source = AudioSourceIdentity::new("exact.capture".into(), vec![]).unwrap();
        let target = crate::domain::output::LiveSinkTarget::new(
            crate::domain::output::SinkIdentity::new("output".into(), vec![]).unwrap(),
            std::num::NonZeroU64::new(20).unwrap(),
            10,
        )
        .unwrap();
        backend.generation = 1;
        backend.audio = AudioSelection::Enabled {
            source: source.clone(),
        };
        backend.output = OutputPlan::Target {
            revision: crate::domain::output::OutputRevision::first(),
            target: target.clone(),
        };
        (backend, source, target)
    }

    #[test]
    fn output_and_gain_never_submit_mpv_audio_properties_or_create_video_handles() {
        let (mut backend, _, target) = enabled_backend();
        backend.property_requests = Some(Vec::new());
        let revision = backend.output.revision().next();
        backend
            .submit(
                RequestId::for_test(1),
                BackendCommand::SetOutput(OutputPlan::Target {
                    revision,
                    target: target.clone(),
                }),
            )
            .unwrap();
        backend
            .submit(
                RequestId::for_test(2),
                BackendCommand::SetGain(PlaybackGain::new(50, true).unwrap()),
            )
            .unwrap();
        assert!(backend.property_requests.as_ref().unwrap().is_empty());
        assert!(backend.handle.is_none() && backend.loopback.is_none() && backend.epoch.is_none());
        assert_eq!(backend.output.target(), Some(&target));
        assert_eq!(backend.gain, PlaybackGain::new(50, true).unwrap());
    }

    #[test]
    fn active_receipt_requires_current_attempt_epoch_watch_source_and_output_revision() {
        let (mut backend, source, target) = enabled_backend();
        let generation = super::super::controller::Generation::new(1).unwrap();
        let attempt = AttemptId::new(1).unwrap();
        let epoch = AudioEpoch::new(1).unwrap();
        let revision = backend.output.revision();
        backend.epoch = Some(epoch);
        for wrong in 0..7 {
            let mut stamp = backend.watch;
            if wrong == 2 {
                stamp.epoch = crate::domain::capture::ObservationEpoch::new(2).unwrap();
            }
            let receipt = LoopbackReceipt::for_test(
                if wrong == 6 {
                    super::super::controller::Generation::new(2).unwrap()
                } else {
                    generation
                },
                if wrong == 0 {
                    AttemptId::new(2).unwrap()
                } else {
                    attempt
                },
                if wrong == 1 {
                    AudioEpoch::new(2).unwrap()
                } else {
                    epoch
                },
                stamp,
                if wrong == 3 {
                    AudioSourceIdentity::new("microphone".into(), vec![]).unwrap()
                } else {
                    source.clone()
                },
                if wrong == 5 {
                    crate::domain::output::LiveSinkTarget::new(
                        target.identity.clone(),
                        std::num::NonZeroU64::new(999).unwrap(),
                        99,
                    )
                    .unwrap()
                } else {
                    target.clone()
                },
                if wrong == 4 {
                    revision.next()
                } else {
                    revision
                },
            );
            backend.consume_audio_event(LoopbackEvent::Active(receipt));
            assert!(!matches!(
                backend.audio_status,
                AudioAvailability::Active { .. }
            ));
        }
        let receipt = LoopbackReceipt::for_test(
            generation,
            attempt,
            epoch,
            backend.watch,
            source.clone(),
            target,
            revision,
        );
        backend.consume_audio_event(LoopbackEvent::Active(receipt.clone()));
        assert_eq!(
            backend.audio_status,
            AudioAvailability::Active { route: receipt }
        );
    }

    #[test]
    fn joint_pause_requires_observed_video_freeze_and_post_join_audio_stop() {
        let (mut backend, _, _) = enabled_backend();
        let epoch = AudioEpoch::new(1).unwrap();
        backend.epoch = Some(epoch);
        backend.retirement = Some(AudioRetirement {
            epoch,
            reason: AudioSilence::Paused,
        });
        let request = PauseRequestId::new(1).unwrap();
        let id = (1 << 63) + 1;
        backend.pending.push((
            id,
            Pending::PauseRead {
                owner: 9,
                request,
                expected: true,
            },
        ));
        assert_eq!(
            backend
                .reply(
                    id,
                    ReplyKind::Property,
                    0,
                    Some(c"pause".to_owned()),
                    Some(NodeValue::Flag(true))
                )
                .unwrap(),
            BackendEvent::Other
        );
        assert!(!backend.emitted.iter().any(|event| matches!(
            event,
            BackendEvent::PauseObserved(_) | BackendEvent::CommandReply { id: 9, .. }
        )));
        backend.consume_audio_event(LoopbackEvent::Blocked {
            stage: "reap",
            diagnostic: "not retired".into(),
        });
        assert!(backend.joint_pause.is_some());
        assert!(
            !backend
                .emitted
                .iter()
                .any(|event| matches!(event, BackendEvent::PauseObserved(_)))
        );
        let desired = OutputPlan::Silent {
            revision: backend.output.revision().next(),
            reason: crate::domain::output::OutputSilence::ManualRequiresAction,
        };
        backend
            .submit(
                RequestId::for_test(10),
                BackendCommand::SetOutput(desired.clone()),
            )
            .unwrap();
        assert_eq!(backend.output, desired);
        assert!(matches!(
            backend.audio_status,
            AudioAvailability::Blocked { .. }
        ));
        assert!(!backend.terminal_audio);
        // This event is the supervisor's actual reap/join boundary, not a command ACK.
        backend.consume_audio_event(LoopbackEvent::Stopped { outcome: Ok(()) });
        assert!(backend.emitted.iter().any(|event| matches!(event, BackendEvent::PauseObserved(PauseObservation { request: Some(observed), paused: true }) if *observed == request)));
        assert!(
            backend
                .emitted
                .iter()
                .any(|event| matches!(event, BackendEvent::CommandReply { id: 9, error: 0 }))
        );
        assert!(backend.epoch.is_none() && backend.retirement.is_none());
    }

    #[test]
    fn source_loss_retires_only_audio_and_terminal_failure_is_not_source_recovery() {
        let (mut backend, source, _) = enabled_backend();
        let epoch = AudioEpoch::new(1).unwrap();
        backend.epoch = Some(epoch);
        backend.consume_audio_event(LoopbackEvent::SourceLost(AudioError::SourceMissing {
            name: source.name().into(),
        }));
        assert!(
            matches!(backend.audio_status, AudioAvailability::Detaching { epoch: active } if active == epoch)
        );
        assert!(!backend.terminal_audio);
        backend.consume_audio_event(LoopbackEvent::Stopped { outcome: Ok(()) });
        assert!(matches!(
            backend.audio_status,
            AudioAvailability::Silent {
                reason: AudioSilence::WaitingForSource(_)
            }
        ));
        assert!(!backend.video_ended);
        backend.epoch = Some(AudioEpoch::new(2).unwrap());
        backend.consume_audio_event(LoopbackEvent::Failed(AudioError::Control(
            "helper died".into(),
        )));
        backend.consume_audio_event(LoopbackEvent::Stopped {
            outcome: Err(AudioError::Control("helper died".into())),
        });
        assert!(backend.terminal_audio);
        assert!(matches!(
            backend.audio_status,
            AudioAvailability::Silent {
                reason: AudioSilence::Failed(_)
            }
        ));
        assert!(
            backend
                .start_audio(AudioEpoch::new(3).unwrap(), source, backend.watch)
                .is_err()
        );
        assert!(!backend.video_ended);
    }

    #[test]
    fn superseded_attach_completes_across_actual_mailbox_and_gate_then_admits_fresh_epoch() {
        use crate::{
            app::ports::{ImmediateIntent, SessionEvent, SessionRunner, StopReason, SubmitStatus},
            domain::state::{ApplyId, AttemptKey, AttemptPurpose},
            media::{
                controller::{
                    Generation, OwnerEndpoint, X11WindowId,
                    test_support::{Config, Driver},
                },
                gate::GateRunner,
            },
        };
        let (mut audio, source, target) = enabled_backend();
        let first_revision = audio.output.revision();
        let initial = OutputPlan::Silent {
            revision: first_revision,
            reason: crate::domain::output::OutputSilence::NoAvailableOutput,
        };
        audio.output = initial.clone();
        let requested = audio.input.requested();
        let settings = crate::domain::state::DraftSettings {
            video: crate::domain::capture::ModeRequest {
                identity: requested.identity.clone(),
                mode: requested.mode,
            },
            audio: AudioSelection::Enabled {
                source: source.clone(),
            },
        };
        let prepared = crate::capture::apply::fixture_prepared(settings).unwrap();
        audio.watch = prepared.stamp();
        let stamp = audio.watch;
        let (drivers_tx, drivers) = std::sync::mpsc::channel();
        let mut gate = GateRunner::with_spawner(move |generation, session| {
            let requested = session.video.requested();
            let (driver, backend) = Driver::pair(Config::default());
            driver.set_observed(ObservedFacts {
                decoded_size: Some(Observation {
                    value: requested.mode.size,
                    source: Source::MpvDecodedParams,
                }),
                decoded_pixel_format: None,
                nominal_rate: Some(Observation {
                    value: f64::from(requested.mode.rate.numerator())
                        / f64::from(requested.mode.rate.denominator()),
                    source: Source::MpvContainerFps,
                }),
            });
            drivers_tx.send(driver).unwrap();
            OwnerEndpoint::spawn_with_backend_requested(generation, requested, move || backend)
        });
        let attempt = AttemptId::new(1).unwrap();
        let key = AttemptKey {
            apply: ApplyId::new(1).unwrap(),
            attempt,
            purpose: AttemptPurpose::Candidate,
        };
        gate.begin_open(
            key,
            prepared,
            PlaybackGain::default(),
            InitialPlayback::Live,
            initial,
        )
        .unwrap();
        assert!(gate.take_native_update().create_native);
        let driver = drivers.recv().unwrap();
        gate.surface_ready(SurfaceToken {
            generation: Generation::new(1).unwrap(),
            xid: X11WindowId::new(47).unwrap(),
        });
        driver
            .initialized
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        let (load, _) = driver
            .submitted
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        driver.send(BackendEvent::AudioAvailability(AudioAvailability::Silent {
            reason: AudioSilence::Output(crate::domain::output::OutputSilence::NoAvailableOutput),
        }));
        driver.send(BackendEvent::FileLoaded);
        driver.send(BackendEvent::CommandReply {
            id: load.get(),
            error: 0,
        });
        driver.send(BackendEvent::PlaybackRestart);
        driver.fence();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if matches!(gate.poll(), Some(SessionEvent::OpenVerified { .. })) {
                break;
            }
            assert!(std::time::Instant::now() < deadline, "video did not verify");
            std::thread::yield_now();
        }
        // Hold an existing discrete transaction so AttachAudio cannot consume.
        assert_eq!(
            gate.submit_immediate(attempt, ImmediateIntent::SetGain(PlaybackGain::default())),
            SubmitStatus::Accepted
        );
        let (held, _) = driver
            .submitted
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        let target_plan = OutputPlan::Target {
            revision: first_revision.next(),
            target: target.clone(),
        };
        assert_eq!(
            gate.submit_immediate(attempt, ImmediateIntent::SetOutput(target_plan)),
            SubmitStatus::Accepted
        );
        let (id, command) = driver
            .submitted
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        audio.submit(id, command).unwrap();
        let first = AudioEpoch::new(1).unwrap();
        assert_eq!(
            gate.submit_immediate(
                attempt,
                ImmediateIntent::AttachAudio {
                    epoch: first,
                    source: source.clone(),
                    stamp
                }
            ),
            SubmitStatus::Accepted
        );
        let silent = OutputPlan::Silent {
            revision: first_revision.next().next(),
            reason: crate::domain::output::OutputSilence::ManualRequiresAction,
        };
        assert_eq!(
            gate.submit_immediate(attempt, ImmediateIntent::SetOutput(silent)),
            SubmitStatus::Accepted
        );
        // Real latest-value mailbox bypasses the held transaction first.
        let (id, command) = driver
            .submitted
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        audio.submit(id, command).unwrap();
        driver.send(BackendEvent::CommandReply {
            id: held.get(),
            error: 0,
        });
        let (id, command) = driver
            .submitted
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        assert!(matches!(&command, BackendCommand::AttachAudio { epoch, .. } if *epoch == first));
        audio.submit(id, command).unwrap(); // Real no-resource admission path.
        assert_eq!(audio.last_audio_epoch, first.get());
        assert!(audio.loopback.is_none() && audio.epoch.is_none());
        while let Some(event) = audio.emitted.pop_front() {
            driver.send(event);
        }
        driver.fence();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if matches!(gate.poll(), Some(SessionEvent::AudioDetached { epoch, outcome: Ok(()), .. }) if epoch == first)
            {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "superseded admitted epoch was not completed"
            );
            std::thread::yield_now();
        }
        assert!(!gate.take_native_update().release_native);
        let returned = OutputPlan::Target {
            revision: first_revision.next().next().next(),
            target,
        };
        assert_eq!(
            gate.submit_immediate(attempt, ImmediateIntent::SetOutput(returned)),
            SubmitStatus::Accepted
        );
        let (id, command) = driver
            .submitted
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        audio.submit(id, command).unwrap();
        let second = AudioEpoch::new(2).unwrap();
        assert_eq!(
            gate.submit_immediate(
                attempt,
                ImmediateIntent::AttachAudio {
                    epoch: second,
                    source,
                    stamp
                }
            ),
            SubmitStatus::Accepted
        );
        let (_, command) = driver
            .submitted
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        assert!(matches!(command, BackendCommand::AttachAudio { epoch, .. } if epoch == second));
        assert_eq!(gate.attempt(), Some(attempt));
        // End at the native-free boundary; M2 spawning is qualified separately.
        gate.stop(attempt, StopReason::Close);
        driver
            .destroyed
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if matches!(gate.poll(), Some(SessionEvent::OwnerStopped { .. })) {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "session worker did not retire"
            );
            std::thread::yield_now();
        }
        assert!(gate.take_native_update().release_native);
    }

    #[test]
    fn pending_retirement_never_hides_terminal_failed_or_coalesced_join_error() {
        for paused in [false, true] {
            for consumed_failed in [false, true] {
                let (mut backend, _, _) = enabled_backend();
                let epoch = AudioEpoch::new(1).unwrap();
                backend.epoch = Some(epoch);
                backend.retirement = Some(AudioRetirement {
                    epoch,
                    reason: if paused {
                        AudioSilence::Paused
                    } else {
                        AudioSilence::WaitingForSource(AudioError::SourceMissing {
                            name: "exact.capture".into(),
                        })
                    },
                });
                let failure = AudioError::Control("bounded reap failed".into());
                let (owner, release) =
                    LoopbackOwner::for_test_retiring_worker(Err(failure.clone()));
                backend.loopback = Some(owner);
                if consumed_failed {
                    backend.consume_audio_event(LoopbackEvent::Failed(failure.clone()));
                    assert!(matches!(
                        backend.retirement.as_ref().unwrap().reason,
                        AudioSilence::Failed(_)
                    ));
                    assert!(
                        !backend
                            .emitted
                            .iter()
                            .any(|event| matches!(event, BackendEvent::AudioDetached { .. }))
                    );
                }
                assert!(backend.loopback.as_mut().unwrap().try_join().is_none());
                release.send(()).unwrap();
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
                while backend.loopback.is_some() {
                    backend.drive_audio(); // Existing supervisor's REAL post-join event.
                    assert!(
                        std::time::Instant::now() < deadline,
                        "test worker did not retire"
                    );
                    std::thread::yield_now();
                }
                assert!(backend.terminal_audio);
                assert_eq!(
                    backend.audio_status,
                    AudioAvailability::Silent {
                        reason: AudioSilence::Failed(failure)
                    }
                );
                assert!(backend.emitted.iter().any(|event| matches!(event,
                    BackendEvent::AudioDetached { epoch: retired, outcome: Ok(()) } if *retired == epoch)));
                assert!(!backend.video_ended);
            }
        }
    }

    #[test]
    fn cancelling_source_owner_retains_gain_until_real_join_and_fresh_same_source_epoch() {
        let (mut backend, source, _) = enabled_backend();
        let epoch = AudioEpoch::new(1).unwrap();
        let gain = PlaybackGain::new(37, true).unwrap();
        let (owner, release) = LoopbackOwner::for_test_retiring_worker(Ok(()));
        // The old unconditional forwarding really encounters this rejection.
        assert_eq!(
            owner.submit(LoopbackCommand::SetGain(gain)),
            Err(AudioError::Cancelled)
        );
        backend.loopback = Some(owner);
        backend.epoch = Some(epoch);
        backend.last_audio_epoch = epoch.get();
        backend.retire_audio(AudioSilence::WaitingForSource(AudioError::SourceMissing {
            name: source.name().into(),
        }));
        backend
            .submit(RequestId::for_test(7), BackendCommand::SetGain(gain))
            .unwrap();
        assert_eq!(backend.gain, gain);
        assert!(!backend.terminal_audio);
        backend.drive_audio();
        assert!(backend.loopback.is_some());
        assert!(
            !backend
                .emitted
                .iter()
                .any(|event| matches!(event, BackendEvent::AudioDetached { .. }))
        );
        release.send(()).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while backend.loopback.is_some() {
            backend.drive_audio();
            assert!(
                std::time::Instant::now() < deadline,
                "test worker did not retire"
            );
            std::thread::yield_now();
        }
        assert!(!backend.terminal_audio);
        assert!(matches!(
            backend.audio_status,
            AudioAvailability::Silent {
                reason: AudioSilence::WaitingForSource(_)
            }
        ));
        let mut returned = backend.watch;
        returned.epoch =
            crate::domain::capture::ObservationEpoch::new(returned.epoch.get() + 1).unwrap();
        // Keep the native-free output parked; the fresh same-source admission
        // still exercises all real epoch/source/watch/terminal guards.
        backend.output = OutputPlan::Silent {
            revision: backend.output.revision().next(),
            reason: crate::domain::output::OutputSilence::ManualRequiresAction,
        };
        backend
            .submit(
                RequestId::for_test(8),
                BackendCommand::AttachAudio {
                    epoch: AudioEpoch::new(2).unwrap(),
                    source,
                    stamp: returned,
                },
            )
            .unwrap();
        assert_eq!(backend.last_audio_epoch, 2);
        assert_eq!(backend.watch, returned);
        assert_eq!(backend.gain, gain);
        assert_eq!(backend.generation, 1);
        assert!(!backend.video_ended);
    }
}
