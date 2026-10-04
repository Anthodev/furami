//! Minimal mpv 0.41.0/client 2.5 ABI. Every handle stays on its creating thread.

use super::controller::{
    AudioStatus, BackendCommand, BackendEvent, MediaError, OwnerBackend, PauseObservation,
    RequestId, StopFlag, SurfaceToken,
};
use super::session::{Observation, ObservedFacts, Source};
use crate::capture::audio::{AudioEvent, RecordingGuard};
use crate::capture::input::InputSpec;
use crate::domain::capture::{AudioError, AudioSelection, FrameSize, PlaybackGain};
use crate::domain::state::PauseRequestId;
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
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AudioStep {
    SaveFormat,
    SaveOptions,
    SetFormat,
    SetOptions,
    Add,
    RestoreFormat,
    RestoreOptions,
    CheckFormat,
    CheckOptions,
    Done,
}
struct AudioOpen {
    step: AudioStep,
    pending: Option<(u64, ReplyKind)>,
    saved_format: Option<NodeValue>,
    saved_options: Option<NodeValue>,
    add_error: Option<i32>,
}
impl AudioOpen {
    fn new() -> Self {
        Self {
            step: AudioStep::SaveFormat,
            pending: None,
            saved_format: None,
            saved_options: None,
            add_error: None,
        }
    }
    fn arm(&mut self, id: u64, kind: ReplyKind, step: AudioStep) {
        self.step = step;
        self.pending = Some((id, kind));
    }
    fn matches(&self, id: u64, kind: ReplyKind) -> bool {
        self.pending == Some((id, kind))
    }
    fn advance(&mut self, error: i32, value: Option<NodeValue>) -> Result<AudioStep, MediaError> {
        if error < 0 && self.step != AudioStep::Add {
            return Err(MediaError::new(
                "audio_options",
                format!("audio option window {:?} failed: {error}", self.step),
            ));
        }
        self.pending = None;
        self.step = match self.step {
            AudioStep::SaveFormat => {
                if value.as_ref().and_then(NodeValue::text) != Some("v4l2") {
                    return Err(MediaError::new(
                        "audio_options",
                        "original next-open lavf format is not V4L2",
                    ));
                }
                self.saved_format = value;
                AudioStep::SaveOptions
            }
            AudioStep::SaveOptions => {
                if !matches!(value, Some(NodeValue::Map(_))) {
                    return Err(MediaError::new(
                        "audio_options",
                        "original lavf options are not a key/value map",
                    ));
                }
                self.saved_options = value;
                AudioStep::SetFormat
            }
            AudioStep::SetFormat => AudioStep::SetOptions,
            AudioStep::SetOptions => AudioStep::Add,
            AudioStep::Add => {
                self.add_error = (error < 0).then_some(error);
                AudioStep::RestoreFormat
            }
            AudioStep::RestoreFormat => AudioStep::RestoreOptions,
            AudioStep::RestoreOptions => AudioStep::CheckFormat,
            AudioStep::CheckFormat => {
                if value != self.saved_format {
                    return Err(MediaError::new(
                        "audio_options",
                        "lavf format restoration readback differs",
                    ));
                }
                AudioStep::CheckOptions
            }
            AudioStep::CheckOptions => {
                if value != self.saved_options {
                    return Err(MediaError::new(
                        "audio_options",
                        "lavf options restoration readback differs",
                    ));
                }
                AudioStep::Done
            }
            AudioStep::Done => {
                return Err(MediaError::new(
                    "audio_options",
                    "duplicate audio open completion",
                ));
            }
        };
        Ok(self.step)
    }
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
    Audio,
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
    TrackList,
    Aid,
    CurrentAudio,
    GainVolume {
        owner: u64,
        gain: PlaybackGain,
    },
    GainMute {
        owner: u64,
    },
    #[cfg(test)]
    FixtureProperty(&'static CStr),
}
impl Pending {
    fn property(self) -> Option<&'static CStr> {
        Some(match self {
            Self::Audio => return None,
            Self::Pause | Self::PauseSet { .. } | Self::PauseRead { .. } => c"pause",
            Self::Metadata(0) => c"video-params",
            Self::Metadata(_) => c"container-fps",
            Self::TrackList => c"track-list",
            Self::Aid => c"aid",
            Self::CurrentAudio => c"current-tracks/audio",
            Self::GainVolume { .. } => c"volume",
            Self::GainMute { .. } => c"mute",
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

pub(crate) struct MpvBackend {
    prefix: String,
    handle: Option<Handle>,
    input: InputSpec,
    diagnostics: std::collections::VecDeque<String>,
    audio: AudioSelection,
    gain: PlaybackGain,
    audio_status: AudioStatus,
    guard: Option<RecordingGuard>,
    cancel: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    audio_url: Option<CString>,
    audio_open: Option<AudioOpen>,
    audio_attempted: bool,
    load_id: Option<u64>,
    load_complete: bool,
    video_started: bool,
    last_internal: u64,
    pending: Vec<(u64, Pending)>,
    metadata: Option<MetadataBatch>,
    pause_pending: bool,
    pause_dirty: bool,
    track_id: Option<i64>,
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
    ) -> Self {
        Self {
            prefix,
            handle: None,
            input,
            diagnostics: std::collections::VecDeque::new(),
            audio,
            gain,
            audio_status: AudioStatus::Disabled,
            guard: None,
            cancel: None,
            audio_url: None,
            audio_open: None,
            audio_attempted: false,
            load_id: None,
            load_complete: false,
            video_started: false,
            last_internal: 1 << 63,
            pending: Vec::with_capacity(8),
            metadata: None,
            pause_pending: false,
            pause_dirty: false,
            track_id: None,
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
    fn audio_failed(&mut self, error: AudioError) {
        if !matches!(self.audio_status, AudioStatus::RestartRequired(_)) {
            tracing::warn!(error = %error, requested = ?self.audio, "audio_restart_required");
            self.audio_status = AudioStatus::RestartRequired(error);
            self.emitted
                .push_back(BackendEvent::AudioStatus(self.audio_status.clone()));
        }
    }
    fn backend_audio_error(
        operation: &str,
        code: Option<i32>,
        detail: impl Into<String>,
    ) -> AudioError {
        AudioError::Backend {
            operation: operation.into(),
            code,
            detail: detail.into(),
        }
    }
    fn begin_audio_if_ready(&mut self) -> Result<(), MediaError> {
        if self.audio_attempted
            || !self.load_complete
            || !self.video_started
            || self.guard.is_none()
            || self
                .cancel
                .as_ref()
                .is_some_and(|cancel| cancel.load(std::sync::atomic::Ordering::Acquire))
        {
            return Ok(());
        }
        self.audio_attempted = true;
        self.audio_open = Some(AudioOpen::new());
        self.submit_audio_step()
    }
    fn submit_audio_step(&mut self) -> Result<(), MediaError> {
        // Stop wins every continuation, including SetOptions -> audio-add.
        // Leave unresolved options on old handle for safe whole-handle teardown.
        if self
            .cancel
            .as_ref()
            .is_some_and(|cancel| cancel.load(std::sync::atomic::Ordering::Acquire))
        {
            return Ok(());
        }
        let step = self
            .audio_open
            .as_ref()
            .ok_or_else(|| MediaError::new("audio_options", "audio open state absent"))?
            .step;
        let id = self.next_internal()?;
        let (kind, name, value) = match step {
            AudioStep::SaveFormat | AudioStep::CheckFormat => {
                (ReplyKind::Property, c"options/demuxer-lavf-format", None)
            }
            AudioStep::SaveOptions | AudioStep::CheckOptions => {
                (ReplyKind::Property, c"options/demuxer-lavf-o", None)
            }
            AudioStep::SetFormat => (
                ReplyKind::SetProperty,
                c"options/demuxer-lavf-format",
                Some(NodeValue::String(c"pulse".to_owned())),
            ),
            AudioStep::SetOptions => {
                let tag = self
                    .guard
                    .as_ref()
                    .ok_or_else(|| MediaError::new("audio_options", "recording guard absent"))?
                    .tag();
                let tag = CString::new(tag)
                    .map_err(|error| MediaError::new("audio_options", error.to_string()))?;
                (
                    ReplyKind::SetProperty,
                    c"options/demuxer-lavf-o",
                    Some(NodeValue::Map(vec![
                        (c"name".to_owned(), NodeValue::String(tag.clone())),
                        (c"stream_name".to_owned(), NodeValue::String(tag)),
                    ])),
                )
            }
            AudioStep::RestoreFormat => {
                (ReplyKind::SetProperty, c"options/demuxer-lavf-format", None)
            }
            AudioStep::RestoreOptions => (ReplyKind::SetProperty, c"options/demuxer-lavf-o", None),
            AudioStep::Add => (ReplyKind::Command, c"", None),
            AudioStep::Done => return Ok(()),
        };
        let handle = self.handle()?;
        match kind {
            ReplyKind::Property => handle.get_async(id, name)?,
            ReplyKind::SetProperty => {
                let saved = self.audio_open.as_ref().and_then(|open| match step {
                    AudioStep::RestoreFormat => open.saved_format.as_ref(),
                    AudioStep::RestoreOptions => open.saved_options.as_ref(),
                    _ => None,
                });
                handle.set_async(
                    id,
                    name,
                    value.as_ref().or(saved).ok_or_else(|| {
                        MediaError::new("audio_options", "saved lavf option absent")
                    })?,
                )?;
            }
            ReplyKind::Command => {
                let url = self
                    .audio_url
                    .as_ref()
                    .ok_or_else(|| MediaError::new("audio_options", "explicit audio URL absent"))?;
                let argv = [
                    c"audio-add".as_ptr(),
                    url.as_ptr(),
                    c"select".as_ptr(),
                    ptr::null(),
                ];
                // SAFETY: Owner-only handle and live NUL-terminated argv. Async
                // API copies arguments; exact source URL is separate from options.
                let result = unsafe {
                    (handle.functions.command_async)(handle.raw.as_ptr(), id, argv.as_ptr())
                };
                handle.checked(result, "initial audio-add")?;
            }
        }
        tracing::info!(request = id, step = ?step, option = %name.to_string_lossy(), value = ?value, "audio_open_request");
        self.audio_open
            .as_mut()
            .expect("audio state retained")
            .arm(id, kind, step);
        self.pending.push((id, Pending::Audio));
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
            Pending::Audio => {
                let open = self.audio_open.as_ref().ok_or_else(|| {
                    MediaError::new("audio_options", "audio state absent on reply")
                })?;
                if !open.matches(id, kind) {
                    return Ok(BackendEvent::Other);
                }
                match open.step {
                    AudioStep::SaveFormat | AudioStep::CheckFormat => {
                        Some(c"options/demuxer-lavf-format")
                    }
                    AudioStep::SaveOptions | AudioStep::CheckOptions => {
                        Some(c"options/demuxer-lavf-o")
                    }
                    _ => None,
                }
            }
            Pending::GainVolume { .. } | Pending::GainMute { .. } | Pending::PauseSet { .. } => {
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
            Pending::Audio => {
                let open = self.audio_open.as_mut().expect("matched audio state");
                let add_completed = open.step == AudioStep::Add && error >= 0;
                let step = open.advance(error, value)?;
                if add_completed && let Some(guard) = &self.guard {
                    guard.mark_open_complete();
                }
                if step == AudioStep::Done {
                    let open = self.audio_open.take().expect("completed audio state");
                    if let Some(code) = open.add_error {
                        self.audio_failed(Self::backend_audio_error(
                            "audio-add",
                            Some(code),
                            self.diagnostic().unwrap_or_default(),
                        ));
                    } else {
                        self.get(c"track-list", Pending::TrackList)?;
                    }
                } else {
                    self.submit_audio_step()?;
                }
            }
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
                self.emitted
                    .push_back(BackendEvent::PauseObserved(PauseObservation {
                        request: Some(request),
                        paused: expected,
                    }));
                self.emitted.push_back(BackendEvent::CommandReply {
                    id: owner,
                    error: 0,
                });
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
                        facts: observed_from_nodes(&batch.values),
                    });
                    if let Some(started) = batch.rerun {
                        self.refresh_observed(started)?;
                    }
                }
            }
            Pending::TrackList => {
                self.track_id = exact_audio_track(value.as_ref(), self.audio_url.as_deref());
                if error < 0 || self.track_id.is_none() {
                    self.audio_failed(Self::backend_audio_error("track-list", Some(error), "audio-add did not expose exactly one selected external track for requested source"));
                } else {
                    self.get(c"aid", Pending::Aid)?;
                }
            }
            Pending::Aid => {
                if error < 0 || value != self.track_id.map(NodeValue::Int) {
                    self.audio_failed(Self::backend_audio_error(
                        "aid",
                        Some(error),
                        "selected audio ID does not match exact requested external track",
                    ));
                } else {
                    self.get(c"current-tracks/audio", Pending::CurrentAudio)?;
                }
            }
            Pending::CurrentAudio => {
                if error < 0
                    || value.as_ref().and_then(|value| value.field("id"))
                        != self.track_id.map(NodeValue::Int).as_ref()
                {
                    self.audio_failed(Self::backend_audio_error(
                        "current-tracks/audio",
                        Some(error),
                        "current audio track does not match requested source",
                    ));
                } else if !matches!(self.audio_status, AudioStatus::RestartRequired(_)) {
                    self.audio_status = AudioStatus::Active;
                    self.emitted
                        .push_back(BackendEvent::AudioStatus(AudioStatus::Active));
                }
            }
            Pending::GainVolume { owner, gain } => {
                if error < 0 {
                    return Ok(BackendEvent::CommandReply { id: owner, error });
                }
                self.set(
                    c"mute",
                    &NodeValue::Flag(gain.muted),
                    Pending::GainMute { owner },
                )?;
            }
            Pending::GainMute { owner } => {
                return Ok(BackendEvent::CommandReply { id: owner, error });
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

fn exact_audio_track(tracks: Option<&NodeValue>, url: Option<&CStr>) -> Option<i64> {
    let (Some(NodeValue::Array(tracks)), Some(url)) = (tracks, url) else {
        return None;
    };
    let mut matches = tracks.iter().filter(|track| {
        track.field("type").and_then(NodeValue::text) == Some("audio")
            && track.field("external") == Some(&NodeValue::Flag(true))
            && track.field("external-filename").and_then(NodeValue::text) == url.to_str().ok()
    });
    let track = matches.next()?;
    if matches.next().is_some() || track.field("selected") != Some(&NodeValue::Flag(true)) {
        return None;
    }
    match track.field("id") {
        Some(NodeValue::Int(id)) if *id > 0 => Some(*id),
        _ => None,
    }
}

fn observed_from_nodes(values: &[Option<NodeValue>; 2]) -> ObservedFacts {
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
            source: Source::MpvContainerFps,
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
        if let AudioSelection::Enabled { source } = &self.audio {
            self.audio_status = AudioStatus::Opening;
            self.cancel = Some(stop.audio_cancel());
            let url = CString::new(format!("av://pulse:{}", source.name()))
                .map_err(|error| MediaError::new("audio_input", error.to_string()))?;
            self.audio_url = Some(url);
            match RecordingGuard::start(source, token.generation.get(), stop.audio_cancel()) {
                Ok(guard) => self.guard = Some(guard),
                Err(AudioError::Cancelled) if stop.is_set() => return check_stop(stop),
                Err(error) => self.audio_failed(error),
            }
            check_stop(stop)?;
        }
        let path = qualified_library(&self.prefix)?;
        self.handle = Some(Handle::create(&path, stop)?);
        check_stop(stop)?;
        let handle = self.handle()?;
        // mpv 0.41.0 registers `osc` only with HAVE_LUA. The frozen no-Lua
        // build has neither that option nor an OSC, so no `osc=no` is submitted.
        const OPTIONS: &[(&CStr, &CStr)] = &[
            (c"config", c"no"),
            (c"pause", c"no"),
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
        handle.option(c"demuxer-lavf-format", c"v4l2")?;
        let common_clock = if matches!(self.audio, AudioSelection::Enabled { .. }) {
            Some(
                self.input
                    .common_clock_options()
                    .map_err(|error| MediaError::new("audio_input", error.to_string()))?,
            )
        } else {
            None
        };
        handle.option(
            c"demuxer-lavf-o",
            common_clock.as_deref().unwrap_or(self.input.lavf_options()),
        )?;
        if common_clock.is_some() {
            handle.option(c"rebase-start-time", c"no")?;
        }
        handle.option(c"audio", c"no")?;
        let volume = CString::new(self.gain.volume_percent.to_string())
            .map_err(|error| MediaError::new("audio_gain", error.to_string()))?;
        handle.option(c"volume", &volume)?;
        handle.option(c"mute", if self.gain.muted { c"yes" } else { c"no" })?;
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
        Ok(())
    }
    fn submit(&mut self, id: RequestId, command: BackendCommand) -> Result<(), MediaError> {
        if let BackendCommand::SetPaused { request, paused } = command {
            return self.set(
                c"pause",
                &NodeValue::Flag(paused),
                Pending::PauseSet {
                    owner: id.get(),
                    request,
                    expected: paused,
                },
            );
        }
        if let BackendCommand::SetGain(gain) = command {
            return self.set(
                c"volume",
                &NodeValue::Double(f64::from(gain.volume_percent)),
                Pending::GainVolume {
                    owner: id.get(),
                    gain,
                },
            );
        }
        if command == BackendCommand::LoadInput {
            self.load_id = Some(id.get());
        }
        let handle = self.handle()?;
        let arguments = match command {
            BackendCommand::LoadInput => {
                [c"loadfile".as_ptr(), self.input.url().as_ptr(), ptr::null()]
            }
            BackendCommand::SetGain(_) | BackendCommand::SetPaused { .. } => {
                unreachable!("playback controls use property requests")
            }
        };
        // SAFETY: Live owner-thread handle, checked nonzero ID, terminated argv
        // of separately owned NUL-free strings. mpv copies before returning;
        // the node never enters the lavf option string.
        let result = unsafe {
            (handle.functions.command_async)(handle.raw.as_ptr(), id.get(), arguments.as_ptr())
        };
        handle.checked(result, "command_async")
    }
    fn next_event(&mut self) -> Result<BackendEvent, MediaError> {
        if let Some(event) = self.guard.as_ref().and_then(RecordingGuard::poll_event) {
            match event {
                AudioEvent::SourceLost(error) => self.audio_failed(error),
                AudioEvent::CancelFailed(error) => {
                    return Err(MediaError::new("audio_cancel", error.to_string()));
                }
                AudioEvent::Cancelled => {}
            }
        }
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
                Ok(BackendEvent::EndFile {
                    reason: end.reason,
                    error: end.error,
                })
            }
            8 => Ok(BackendEvent::FileLoaded),
            17 => Ok(BackendEvent::VideoReconfig),
            18 => {
                if self.audio_status == AudioStatus::Active
                    && !self.pending.iter().any(|(_, pending)| {
                        matches!(
                            pending,
                            Pending::TrackList | Pending::Aid | Pending::CurrentAudio
                        )
                    })
                {
                    self.get(c"track-list", Pending::TrackList)?;
                }
                Ok(BackendEvent::Other)
            }
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
    fn audio_status(&self) -> AudioStatus {
        self.audio_status.clone()
    }
    fn shutdown(&mut self) -> Result<(), MediaError> {
        if let Some(cancel) = &self.cancel {
            cancel.store(true, std::sync::atomic::Ordering::Release);
        }
        if self.handle.is_none() {
            tracing::info!("owner_handle_absent");
        }
        // Unresolved next-open option window is never restored speculatively.
        // Entire old handle must finish destruction before another generation.
        drop(self.handle.take());
        if let Some(guard) = self.guard.take() {
            guard
                .quiesce()
                .map_err(|error| MediaError::new("audio_cancel", error.to_string()))?;
        }
        self.pending.clear();
        self.audio_open = None;
        Ok(())
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
            inner: MpvBackend::new(prefix, input, AudioSelection::default(), gain),
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
            (c"ao", c"null"),
            (c"ao-null-untimed", c"no"),
            (c"aid", c"auto"),
            (c"pause", c"no"),
        ];
        for &(name, value) in OPTIONS {
            check_stop(stop)?;
            handle.option(name, value)?;
        }
        let volume = CString::new(self.inner.gain.volume_percent.to_string())
            .map_err(|error| MediaError::new("audio_gain", error.to_string()))?;
        handle.option(c"volume", &volume)?;
        handle.option(c"mute", if self.inner.gain.muted { c"yes" } else { c"no" })?;
        check_stop(stop)?;
        // SAFETY: The genuine owner-thread handle is live; options above were
        // checked before initialization and no native window/Pulse guard exists.
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

    fn audio_status(&self) -> AudioStatus {
        self.inner.audio_status()
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
        fixture_wait(backend, "volume then mute transaction", |_, event| {
            Ok(matches!(event, BackendEvent::CommandReply { id, error: 0 } if id == owner.get()))
        })?;
        assert_eq!(
            fixture_property(backend, c"volume")?,
            NodeValue::Double(f64::from(gain.volume_percent)),
        );
        assert_eq!(
            fixture_property(backend, c"mute")?,
            NodeValue::Flag(gain.muted)
        );
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
                Some(recorder.clone()),
            )
            .unwrap();
            let result = (|| -> Result<(), MediaError> {
                backend.initialize(fixture_token(2), &stop)?;
                fixture_load(&mut backend)?;
                assert_eq!(
                    fixture_property(&mut backend, c"volume")?,
                    NodeValue::Double(80.0)
                );
                assert_eq!(
                    fixture_property(&mut backend, c"mute")?,
                    NodeValue::Flag(false)
                );
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
    fn audio_window_ignores_unmatched_and_wrong_reply_kinds() {
        let mut open = AudioOpen::new();
        open.arm(101, ReplyKind::Property, AudioStep::SaveFormat);
        assert!(!open.matches(102, ReplyKind::Property));
        assert!(!open.matches(101, ReplyKind::Command));
        assert!(open.matches(101, ReplyKind::Property));
        assert_eq!(open.step, AudioStep::SaveFormat);
    }

    #[test]
    fn failed_add_restores_both_saved_options_before_reporting_failure() {
        let mut open = AudioOpen::new();
        open.saved_format = Some(NodeValue::String(CString::new("v4l2").unwrap()));
        open.saved_options = Some(NodeValue::Map(vec![(
            CString::new("input_format").unwrap(),
            NodeValue::String(CString::new("nv12").unwrap()),
        )]));
        open.arm(105, ReplyKind::Command, AudioStep::Add);
        assert_eq!(open.advance(-13, None).unwrap(), AudioStep::RestoreFormat);
        assert_eq!(open.add_error, Some(-13));
        assert_eq!(open.advance(0, None).unwrap(), AudioStep::RestoreOptions);
        assert_eq!(open.advance(0, None).unwrap(), AudioStep::CheckFormat);
        let saved = open.saved_format.clone();
        assert_eq!(open.advance(0, saved).unwrap(), AudioStep::CheckOptions);
        let saved = open.saved_options.clone();
        assert_eq!(open.advance(0, saved).unwrap(), AudioStep::Done);
        assert_eq!(open.add_error, Some(-13));
    }

    #[test]
    fn failed_option_or_restore_cannot_progress_to_audio_open_or_active() {
        for step in [
            AudioStep::SetFormat,
            AudioStep::SetOptions,
            AudioStep::RestoreFormat,
        ] {
            let mut open = AudioOpen::new();
            open.arm(10, ReplyKind::SetProperty, step);
            assert!(open.advance(-5, None).is_err());
            assert_eq!(open.step, step);
        }
    }

    #[test]
    fn restore_readback_contradiction_is_error_not_active_audio() {
        let mut open = AudioOpen::new();
        open.saved_format = Some(NodeValue::String(CString::new("v4l2").unwrap()));
        open.arm(12, ReplyKind::Property, AudioStep::CheckFormat);
        assert!(
            open.advance(0, Some(NodeValue::String(CString::new("pulse").unwrap())))
                .is_err()
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
    fn invalid_option_snapshots_prevent_any_pulse_option_mutation() {
        let mut open = AudioOpen::new();
        assert!(
            open.advance(0, Some(NodeValue::String(c"pulse".to_owned())))
                .is_err()
        );
        let mut open = AudioOpen::new();
        assert_eq!(
            open.advance(0, Some(NodeValue::String(c"v4l2".to_owned())))
                .unwrap(),
            AudioStep::SaveOptions
        );
        assert!(
            open.advance(0, Some(NodeValue::String(c"input_format=nv12".to_owned())))
                .is_err()
        );
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
    fn exact_track_selection_rejects_other_sources_duplicates_and_unselected_track() {
        let track = |source: &CStr, selected: bool| {
            NodeValue::Map(vec![
                (c"type".to_owned(), NodeValue::String(c"audio".to_owned())),
                (c"external".to_owned(), NodeValue::Flag(true)),
                (
                    c"external-filename".to_owned(),
                    NodeValue::String(source.to_owned()),
                ),
                (c"selected".to_owned(), NodeValue::Flag(selected)),
                (c"id".to_owned(), NodeValue::Int(4)),
            ])
        };
        let exact = c"av://pulse:selected-source";
        assert_eq!(
            exact_audio_track(
                Some(&NodeValue::Array(vec![track(exact, true)])),
                Some(exact)
            ),
            Some(4)
        );
        assert_eq!(
            exact_audio_track(
                Some(&NodeValue::Array(vec![track(c"av://pulse:other", true)])),
                Some(exact)
            ),
            None
        );
        assert_eq!(
            exact_audio_track(
                Some(&NodeValue::Array(vec![track(exact, false)])),
                Some(exact)
            ),
            None
        );
        assert_eq!(
            exact_audio_track(
                Some(&NodeValue::Array(vec![
                    track(exact, true),
                    track(exact, true)
                ])),
                Some(exact)
            ),
            None
        );
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
        let observed = observed_from_nodes(&values);
        assert_eq!(
            observed.decoded_pixel_format.unwrap().source,
            Source::MpvDecodedParams
        );
        assert_eq!(
            observed.nominal_rate.unwrap().source,
            Source::MpvContainerFps
        );
        assert_eq!(
            observed_from_nodes(&[None, Some(NodeValue::Double(f64::NAN))]).nominal_rate,
            None
        );
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
        )
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

    #[test]
    fn first_audio_failure_preserves_requested_enabled_identity_and_never_becomes_disabled() {
        let mut backend = backend();
        backend.audio = AudioSelection::Enabled {
            source: crate::domain::capture::AudioSourceIdentity::new(
                "selected-source".into(),
                vec![],
            )
            .unwrap(),
        };
        let first = AudioError::SourceMissing {
            name: "selected-source".into(),
        };
        backend.audio_failed(first.clone());
        backend.audio_failed(AudioError::Control("later failure".into()));
        assert_eq!(backend.audio_status(), AudioStatus::RestartRequired(first));
        assert!(backend.audio.enabled());
        assert_eq!(backend.audio.source().unwrap().name(), "selected-source");
        assert_eq!(backend.emitted.len(), 1);
    }
}

#[cfg(test)]
mod opening_cancel_tests {
    use super::*;
    use crate::{
        capture::{input::CaptureSelection, linux},
        domain::capture::{CaptureMode, CapturedFourCc, FrameRate},
    };

    #[test]
    fn latched_stop_prevents_audio_add_and_late_option_continuations_without_core_calls() {
        let mode = CaptureMode {
            captured_fourcc: CapturedFourCc::from_bytes(*b"NV12"),
            size: FrameSize::new(2560, 1440).unwrap(),
            rate: FrameRate::new(60, 1).unwrap(),
        };
        let snapshot = linux::session_fixture(&["/dev/video0"], mode);
        let selection =
            CaptureSelection::from_snapshot(&snapshot, Path::new("/dev/video0"), mode).unwrap();
        let mut backend = MpvBackend::new(
            String::new(),
            selection.validate_snapshot(&snapshot).unwrap(),
            AudioSelection::default(),
            PlaybackGain::default(),
        );
        backend.cancel = Some(std::sync::Arc::new(std::sync::atomic::AtomicBool::new(
            true,
        )));
        for step in [
            AudioStep::SetFormat,
            AudioStep::SetOptions,
            AudioStep::Add,
            AudioStep::RestoreFormat,
        ] {
            let mut open = AudioOpen::new();
            open.step = step;
            backend.audio_open = Some(open);
            backend.submit_audio_step().unwrap();
            assert!(backend.pending.is_empty());
            assert_eq!(backend.audio_open.as_ref().unwrap().step, step);
        }
        assert!(backend.handle.is_none());
    }
}
