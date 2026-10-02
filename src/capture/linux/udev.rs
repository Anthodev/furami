//! Private libudev ownership and physical USB observations. No video ioctl or
//! mutable device operation belongs here; callers classify the returned nodes.

use std::{
    ffi::{CStr, CString, OsStr, c_char, c_int},
    fs::{self, File, OpenOptions},
    io,
    marker::PhantomData,
    num::NonZeroU8,
    os::unix::{
        ffi::OsStrExt,
        fs::{FileTypeExt, MetadataExt},
    },
    path::{Path, PathBuf},
    ptr::NonNull,
};

use crate::domain::capture::{DeviceIdentity, UsbTopology};

use super::CaptureError;

#[repr(C)]
struct Udev {
    _private: [u8; 0],
}

#[repr(C)]
struct UdevEnumerate {
    _private: [u8; 0],
}

#[repr(C)]
struct UdevDevice {
    _private: [u8; 0],
}

#[repr(C)]
struct UdevListEntry {
    _private: [u8; 0],
}

// Signatures follow libudev.h. Link the runtime SONAME directly, so compilation
// needs neither libudev's header nor its unversioned development linker symlink.
#[link(name = "libudev.so.1", kind = "dylib", modifiers = "+verbatim")]
unsafe extern "C" {
    fn udev_new() -> *mut Udev;
    fn udev_unref(udev: *mut Udev) -> *mut Udev;
    fn udev_enumerate_new(udev: *mut Udev) -> *mut UdevEnumerate;
    fn udev_enumerate_unref(enumerate: *mut UdevEnumerate) -> *mut UdevEnumerate;
    fn udev_enumerate_add_match_subsystem(
        enumerate: *mut UdevEnumerate,
        subsystem: *const c_char,
    ) -> c_int;
    fn udev_enumerate_scan_devices(enumerate: *mut UdevEnumerate) -> c_int;
    fn udev_enumerate_get_list_entry(enumerate: *mut UdevEnumerate) -> *mut UdevListEntry;
    fn udev_list_entry_get_next(entry: *mut UdevListEntry) -> *mut UdevListEntry;
    fn udev_list_entry_get_name(entry: *mut UdevListEntry) -> *const c_char;
    fn udev_device_new_from_syspath(udev: *mut Udev, syspath: *const c_char) -> *mut UdevDevice;
    fn udev_device_unref(device: *mut UdevDevice) -> *mut UdevDevice;
    fn udev_device_get_parent(device: *mut UdevDevice) -> *mut UdevDevice;
    fn udev_device_get_parent_with_subsystem_devtype(
        device: *mut UdevDevice,
        subsystem: *const c_char,
        devtype: *const c_char,
    ) -> *mut UdevDevice;
    fn udev_device_get_subsystem(device: *mut UdevDevice) -> *const c_char;
    fn udev_device_get_sysname(device: *mut UdevDevice) -> *const c_char;
    fn udev_device_get_syspath(device: *mut UdevDevice) -> *const c_char;
    fn udev_device_get_devnode(device: *mut UdevDevice) -> *const c_char;
    fn udev_device_get_driver(device: *mut UdevDevice) -> *const c_char;
}

struct Context {
    raw: NonNull<Udev>,
}

impl Context {
    fn new() -> Result<Self, CaptureError> {
        // SAFETY: Zero-argument official constructor returns an owned reference.
        let raw = NonNull::new(unsafe { udev_new() }).ok_or(CaptureError::UdevFailure {
            operation: "udev_enumerate",
        })?;
        Ok(Self { raw })
    }
}

impl Drop for Context {
    fn drop(&mut self) {
        // SAFETY: This wrapper owns exactly one reference; dependents borrow it.
        unsafe { udev_unref(self.raw.as_ptr()) };
    }
}

struct Enumeration<'context> {
    raw: NonNull<UdevEnumerate>,
    _context: PhantomData<&'context Context>,
}

impl<'context> Enumeration<'context> {
    fn new(context: &'context Context) -> Result<Self, CaptureError> {
        // SAFETY: Context is live and constructor returns an owned reference.
        let raw = NonNull::new(unsafe { udev_enumerate_new(context.raw.as_ptr()) }).ok_or(
            CaptureError::UdevFailure {
                operation: "udev_enumerate",
            },
        )?;
        Ok(Self {
            raw,
            _context: PhantomData,
        })
    }

    fn scan_video(&mut self) -> Result<(), CaptureError> {
        // SAFETY: Enumeration and static C string remain live for both calls.
        udev_result(unsafe {
            udev_enumerate_add_match_subsystem(self.raw.as_ptr(), c"video4linux".as_ptr())
        })?;
        udev_result(unsafe { udev_enumerate_scan_devices(self.raw.as_ptr()) })
    }

    fn first_entry(&self) -> Option<ListEntry<'_>> {
        // SAFETY: Returned entry is borrowed from this live enumeration.
        NonNull::new(unsafe { udev_enumerate_get_list_entry(self.raw.as_ptr()) }).map(|raw| {
            ListEntry {
                raw,
                _enumeration: PhantomData,
            }
        })
    }
}

impl Drop for Enumeration<'_> {
    fn drop(&mut self) {
        // SAFETY: Owned reference is released once, after borrowed entries expire.
        unsafe { udev_enumerate_unref(self.raw.as_ptr()) };
    }
}

#[derive(Clone, Copy)]
struct ListEntry<'enumeration> {
    raw: NonNull<UdevListEntry>,
    _enumeration: PhantomData<&'enumeration UdevEnumerate>,
}

impl<'enumeration> ListEntry<'enumeration> {
    fn name(self) -> Result<&'enumeration CStr, CaptureError> {
        // SAFETY: Entry and its NUL-terminated string belong to live enumeration.
        unsafe { borrowed_string(udev_list_entry_get_name(self.raw.as_ptr())) }.ok_or(
            CaptureError::UdevFailure {
                operation: "udev_enumerate",
            },
        )
    }

    fn next(self) -> Option<Self> {
        // SAFETY: All entries are borrowed from the same live enumeration.
        NonNull::new(unsafe { udev_list_entry_get_next(self.raw.as_ptr()) }).map(|raw| Self {
            raw,
            _enumeration: PhantomData,
        })
    }
}

struct Device<'context> {
    raw: NonNull<UdevDevice>,
    _context: PhantomData<&'context Context>,
}

impl<'context> Device<'context> {
    fn new(context: &'context Context, syspath: &CStr) -> Result<Self, CaptureError> {
        // SAFETY: Context and path are live; a non-NULL result is owned.
        let raw = NonNull::new(unsafe {
            udev_device_new_from_syspath(context.raw.as_ptr(), syspath.as_ptr())
        })
        .ok_or(CaptureError::UdevFailure {
            operation: "udev_enumerate",
        })?;
        Ok(Self {
            raw,
            _context: PhantomData,
        })
    }

    fn borrow(&self) -> DeviceRef<'_> {
        DeviceRef {
            raw: self.raw,
            _owner: PhantomData,
        }
    }
}

impl Drop for Device<'_> {
    fn drop(&mut self) {
        // SAFETY: Owned reference is released once. Ancestors are borrowed only.
        unsafe { udev_device_unref(self.raw.as_ptr()) };
    }
}

#[derive(Clone, Copy)]
struct DeviceRef<'owner> {
    raw: NonNull<UdevDevice>,
    _owner: PhantomData<&'owner UdevDevice>,
}

impl<'owner> DeviceRef<'owner> {
    fn parent(self) -> Option<Self> {
        // SAFETY: Ancestor is retained by the owning child device.
        NonNull::new(unsafe { udev_device_get_parent(self.raw.as_ptr()) }).map(|raw| Self {
            raw,
            _owner: PhantomData,
        })
    }

    fn usb_parent(self, devtype: &CStr) -> Option<Self> {
        // SAFETY: Child, its retained ancestors and input C strings remain live.
        NonNull::new(unsafe {
            udev_device_get_parent_with_subsystem_devtype(
                self.raw.as_ptr(),
                c"usb".as_ptr(),
                devtype.as_ptr(),
            )
        })
        .map(|raw| Self {
            raw,
            _owner: PhantomData,
        })
    }

    fn subsystem(self) -> Option<&'owner CStr> {
        // SAFETY: Borrowed property remains valid while owner retains the device.
        unsafe { borrowed_string(udev_device_get_subsystem(self.raw.as_ptr())) }
    }

    fn sysname(self) -> Option<&'owner CStr> {
        // SAFETY: Borrowed property remains valid while owner retains the device.
        unsafe { borrowed_string(udev_device_get_sysname(self.raw.as_ptr())) }
    }

    fn driver(self) -> Option<&'owner CStr> {
        // SAFETY: Borrowed property remains valid while owner retains the device.
        unsafe { borrowed_string(udev_device_get_driver(self.raw.as_ptr())) }
    }

    fn syspath(self) -> Result<PathBuf, CaptureError> {
        // SAFETY: Copy path before owner can release its NUL-terminated property.
        let path = unsafe { borrowed_string(udev_device_get_syspath(self.raw.as_ptr())) }.ok_or(
            CaptureError::UdevFailure {
                operation: "udev_enumerate",
            },
        )?;
        Ok(PathBuf::from(OsStr::from_bytes(path.to_bytes())))
    }

    fn devnode(self) -> Option<PathBuf> {
        // SAFETY: Copy path before owner can release its NUL-terminated property.
        unsafe { borrowed_string(udev_device_get_devnode(self.raw.as_ptr())) }
            .map(|path| PathBuf::from(OsStr::from_bytes(path.to_bytes())))
    }
}

/// Caller must keep the libudev owner of `value` alive throughout `'owner`.
unsafe fn borrowed_string<'owner>(value: *const c_char) -> Option<&'owner CStr> {
    if value.is_null() {
        None
    } else {
        // SAFETY: Caller guarantees a live NUL-terminated libudev property.
        Some(unsafe { CStr::from_ptr(value) })
    }
}

fn udev_result(result: c_int) -> Result<(), CaptureError> {
    if result < 0 {
        // libudev integer errors are -errno, not the thread's residual errno.
        let errno = result.checked_neg().ok_or(CaptureError::UdevFailure {
            operation: "udev_enumerate",
        })?;
        Err(CaptureError::Udev {
            operation: "udev_enumerate",
            source: io::Error::from_raw_os_error(errno),
        })
    } else {
        Ok(())
    }
}

#[derive(Debug)]
pub(super) struct NodeObservation {
    pub devnode: PathBuf,
    pub syspath: PathBuf,
}

#[derive(Debug)]
pub(super) struct ObservedDevice {
    pub identity: DeviceIdentity,
    pub usb_syspath: PathBuf,
    pub nodes: Vec<NodeObservation>,
}

#[derive(Debug)]
pub(super) struct Observations {
    pub raw_video_nodes: usize,
    pub devices: Vec<ObservedDevice>,
}

/// Scan every video4linux entry. Non-USB/non-UVC nodes are excluded explicitly;
/// errors on any UVC candidate abort, including missing devnodes and attributes.
pub(super) fn scan() -> Result<Observations, CaptureError> {
    let context = Context::new()?;
    let mut enumeration = Enumeration::new(&context)?;
    enumeration.scan_video()?;
    let mut observations = Observations {
        raw_video_nodes: 0,
        devices: Vec::new(),
    };
    let mut entry = enumeration.first_entry();
    while let Some(current) = entry {
        observations.raw_video_nodes += 1;
        let path = current.name()?;
        let device = Device::new(&context, path)?;
        if let Some((identity, usb_syspath, node)) = observe(device.borrow())? {
            add_observation(&mut observations.devices, identity, usb_syspath, node)?;
        }
        entry = current.next();
    }
    Ok(observations)
}

fn observe(
    node: DeviceRef<'_>,
) -> Result<Option<(DeviceIdentity, PathBuf, NodeObservation)>, CaptureError> {
    if node.subsystem() != Some(c"video4linux") {
        return Ok(None);
    }
    let Some(usb) = node.usb_parent(c"usb_device") else {
        return Ok(None);
    };
    let Some(interface) = node.usb_parent(c"usb_interface") else {
        return Ok(None);
    };
    if interface.driver() != Some(c"uvcvideo") {
        return Ok(None);
    }
    let syspath = node.syspath()?;
    let devnode = node.devnode().ok_or_else(|| CaptureError::MissingDevnode {
        syspath: syspath.clone(),
    })?;
    let usb_syspath = usb.syspath()?;
    let identity = read_identity(usb, &usb_syspath)?;
    Ok(Some((
        identity,
        usb_syspath,
        NodeObservation { devnode, syspath },
    )))
}

fn add_observation(
    devices: &mut Vec<ObservedDevice>,
    identity: DeviceIdentity,
    usb_syspath: PathBuf,
    node: NodeObservation,
) -> Result<(), CaptureError> {
    if let Some(device) = devices
        .iter_mut()
        .find(|device| device.usb_syspath == usb_syspath)
    {
        if device.identity != identity {
            return Err(CaptureError::StaleSnapshot { path: node.devnode });
        }
        device.nodes.push(node);
    } else {
        devices.push(ObservedDevice {
            identity,
            usb_syspath,
            nodes: vec![node],
        });
    }
    Ok(())
}

fn read_identity(usb: DeviceRef<'_>, syspath: &Path) -> Result<DeviceIdentity, CaptureError> {
    let vendor = parse_hex_attribute(syspath, "idVendor", &read_required(syspath, "idVendor")?)?;
    let product = parse_hex_attribute(syspath, "idProduct", &read_required(syspath, "idProduct")?)?;
    let ports = parse_ports(syspath, &read_required(syspath, "devpath")?)?;
    let serial_path = syspath.join("serial");
    let serial = match fs::read(&serial_path) {
        Ok(bytes) => parse_serial(syspath, &bytes)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(access_error(&serial_path, "usb_attribute", error)),
    };
    let controller = controller(usb, syspath)?;
    Ok(DeviceIdentity::new(
        vendor,
        product,
        UsbTopology::new(controller, ports)?,
        serial,
    )?)
}

fn read_required(syspath: &Path, attribute: &'static str) -> Result<Vec<u8>, CaptureError> {
    let path = syspath.join(attribute);
    match fs::read(&path) {
        Ok(bytes) => Ok(bytes),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            Err(CaptureError::MissingUsbAttribute {
                syspath: syspath.to_owned(),
                attribute,
            })
        }
        Err(error) => Err(access_error(&path, "usb_attribute", error)),
    }
}

fn controller(usb: DeviceRef<'_>, syspath: &Path) -> Result<String, CaptureError> {
    let mut parent = usb.parent();
    let mut platform = None;
    while let Some(device) = parent {
        match device.subsystem() {
            Some(subsystem) if subsystem == c"pci" => {
                return controller_name("pci", device, syspath);
            }
            Some(subsystem) if subsystem == c"platform" && platform.is_none() => {
                platform = Some(device);
            }
            _ => {}
        }
        parent = device.parent();
    }
    if let Some(platform) = platform {
        controller_name("platform", platform, syspath)
    } else {
        Err(CaptureError::MissingUsbAttribute {
            syspath: syspath.to_owned(),
            attribute: "topology",
        })
    }
}

fn controller_name(
    prefix: &str,
    device: DeviceRef<'_>,
    syspath: &Path,
) -> Result<String, CaptureError> {
    let name = device
        .sysname()
        .and_then(|name| name.to_str().ok())
        .filter(|name| !name.is_empty() && !name.contains('/') && !name.trim().is_empty())
        .ok_or_else(|| invalid_attribute(syspath, "topology"))?;
    Ok(format!("{prefix}-{name}"))
}

fn sysfs_text<'bytes>(
    syspath: &Path,
    attribute: &'static str,
    bytes: &'bytes [u8],
) -> Result<&'bytes str, CaptureError> {
    let bytes = bytes.strip_suffix(b"\n").unwrap_or(bytes);
    std::str::from_utf8(bytes).map_err(|_| invalid_attribute(syspath, attribute))
}

fn parse_hex_attribute(
    syspath: &Path,
    attribute: &'static str,
    bytes: &[u8],
) -> Result<u16, CaptureError> {
    let value = sysfs_text(syspath, attribute, bytes)?;
    if value.len() != 4 || !value.bytes().all(|digit| digit.is_ascii_hexdigit()) {
        return Err(invalid_attribute(syspath, attribute));
    }
    u16::from_str_radix(value, 16).map_err(|_| invalid_attribute(syspath, attribute))
}

fn parse_ports(syspath: &Path, bytes: &[u8]) -> Result<Vec<NonZeroU8>, CaptureError> {
    let value = sysfs_text(syspath, "devpath", bytes)?;
    value
        .split('.')
        .map(|port| {
            if port.is_empty() || !port.bytes().all(|digit| digit.is_ascii_digit()) {
                return Err(invalid_attribute(syspath, "devpath"));
            }
            port.parse::<u8>()
                .ok()
                .and_then(NonZeroU8::new)
                .ok_or_else(|| invalid_attribute(syspath, "devpath"))
        })
        .collect()
}

fn parse_serial(syspath: &Path, bytes: &[u8]) -> Result<Option<String>, CaptureError> {
    let value = sysfs_text(syspath, "serial", bytes)?;
    Ok((!value.is_empty()).then(|| value.to_owned()))
}

fn invalid_attribute(syspath: &Path, attribute: &'static str) -> CaptureError {
    CaptureError::InvalidUsbAttribute {
        syspath: syspath.to_owned(),
        attribute,
    }
}

pub(super) fn access_error(
    path: &Path,
    operation: &'static str,
    source: io::Error,
) -> CaptureError {
    if source.kind() == io::ErrorKind::PermissionDenied {
        CaptureError::PermissionDenied {
            path: path.to_owned(),
            operation,
            source,
        }
    } else if source.kind() == io::ErrorKind::NotFound || source.raw_os_error() == Some(19) {
        // Linux ENODEV is 19. Unlike ENOENT, std has no dedicated ErrorKind for
        // it; retain the original OS error rather than replacing it with ENOENT.
        CaptureError::DeviceGone {
            path: path.to_owned(),
            operation,
            source,
        }
    } else {
        CaptureError::Io {
            path: path.to_owned(),
            operation,
            source,
        }
    }
}

pub(super) fn open_node(devnode: &Path) -> Result<File, CaptureError> {
    OpenOptions::new()
        .read(true)
        .open(devnode)
        .map_err(|source| access_error(devnode, "open", source))
}

/// Resolve a fresh udev observation, require exact snapshot linkage and physical
/// identity, then open read-only and detect devnode substitution through rdev.
/// Caller must repeat QUERYCAP on this returned file before interval ioctls.
pub(super) fn revalidate_node(
    syspath: &Path,
    devnode: &Path,
    usb_syspath: &Path,
    identity: &DeviceIdentity,
) -> Result<File, CaptureError> {
    fs::metadata(syspath).map_err(|source| access_error(devnode, "open", source))?;
    let path =
        CString::new(syspath.as_os_str().as_bytes()).map_err(|_| CaptureError::StaleSnapshot {
            path: devnode.to_owned(),
        })?;
    let context = Context::new()?;
    let device = Device::new(&context, &path)?;
    let Some((observed_identity, observed_usb, observed_node)) = observe(device.borrow())? else {
        return Err(CaptureError::StaleSnapshot {
            path: devnode.to_owned(),
        });
    };
    if observed_node.syspath != syspath
        || observed_node.devnode != devnode
        || observed_usb != usb_syspath
        || observed_identity != *identity
    {
        return Err(CaptureError::StaleSnapshot {
            path: devnode.to_owned(),
        });
    }
    let observed_metadata = fs::metadata(&observed_node.devnode)
        .map_err(|source| access_error(devnode, "open", source))?;
    if !observed_metadata.file_type().is_char_device() {
        return Err(CaptureError::StaleSnapshot {
            path: devnode.to_owned(),
        });
    }
    let file = open_node(devnode)?;
    let file_metadata = file
        .metadata()
        .map_err(|source| access_error(devnode, "open", source))?;
    if !file_metadata.file_type().is_char_device()
        || file_metadata.rdev() != observed_metadata.rdev()
    {
        return Err(CaptureError::StaleSnapshot {
            path: devnode.to_owned(),
        });
    }
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(serial: Option<&str>, port: u8) -> DeviceIdentity {
        DeviceIdentity::new(
            0x32ed,
            0x3701,
            UsbTopology::new(
                "pci-0000:16:00.0".into(),
                vec![NonZeroU8::new(port).unwrap()],
            )
            .unwrap(),
            serial.map(str::to_owned),
        )
        .unwrap()
    }

    fn node(name: &str) -> NodeObservation {
        NodeObservation {
            syspath: PathBuf::from(format!("/sys/devices/{name}")),
            devnode: PathBuf::from(format!("/dev/{name}")),
        }
    }

    #[test]
    fn usb_ids_require_exactly_four_hex_digits() {
        let path = Path::new("/sys/usb");
        assert_eq!(
            parse_hex_attribute(path, "idVendor", b"32eD\n").unwrap(),
            0x32ed
        );
        for malformed in [
            b"32e".as_slice(),
            b"032ed",
            b"0x32ed",
            b"32ed ",
            b"32eg",
            b"32ed\n\n",
            b"\xff\xff\xff\xff",
        ] {
            assert!(matches!(
                parse_hex_attribute(path, "idVendor", malformed),
                Err(CaptureError::InvalidUsbAttribute {
                    attribute: "idVendor",
                    ..
                })
            ));
        }
    }

    #[test]
    fn topology_ports_are_positive_decimal_u8_values() {
        let path = Path::new("/sys/usb");
        let ports = parse_ports(path, b"1.2.255\n").unwrap();
        assert_eq!(
            ports.iter().map(|port| port.get()).collect::<Vec<_>>(),
            [1, 2, 255]
        );
        for malformed in [
            "", "0", "1.0", "1..2", ".1", "1.", "256", "+1", "1. 2", "1/2", "1\n\n",
        ] {
            assert!(matches!(
                parse_ports(path, malformed.as_bytes()),
                Err(CaptureError::InvalidUsbAttribute {
                    attribute: "devpath",
                    ..
                })
            ));
        }
    }

    #[test]
    fn serial_preserves_case_spaces_and_only_strips_sysfs_newline() {
        let path = Path::new("/sys/usb");
        assert_eq!(
            parse_serial(path, b" AbC 001 \n").unwrap().as_deref(),
            Some(" AbC 001 ")
        );
        assert_eq!(parse_serial(path, b"\n").unwrap(), None);
        assert_eq!(parse_serial(path, b"").unwrap(), None);
        assert!(matches!(
            parse_serial(path, b"\xff\n"),
            Err(CaptureError::InvalidUsbAttribute {
                attribute: "serial",
                ..
            })
        ));
    }

    #[test]
    fn physical_grouping_ignores_node_names_and_keeps_distinct_usb_devices() {
        let mut devices = Vec::new();
        let usb = PathBuf::from("/sys/devices/usb1/1-1");
        for name in ["video8", "video2", "video19"] {
            add_observation(
                &mut devices,
                identity(Some("SERIAL"), 1),
                usb.clone(),
                node(name),
            )
            .unwrap();
        }
        add_observation(
            &mut devices,
            identity(Some("SERIAL"), 2),
            PathBuf::from("/sys/devices/usb1/1-2"),
            node("video5"),
        )
        .unwrap();
        assert_eq!(devices.len(), 2);
        assert_eq!(devices[0].nodes.len(), 3);
        assert_eq!(devices[1].nodes.len(), 1);
        assert_eq!(devices[0].identity.serial(), devices[1].identity.serial());
    }

    #[test]
    fn changed_identity_with_same_physical_syspath_is_stale() {
        let mut devices = Vec::new();
        let usb = PathBuf::from("/sys/devices/usb1/1-1");
        add_observation(
            &mut devices,
            identity(Some("OLD"), 1),
            usb.clone(),
            node("video0"),
        )
        .unwrap();
        assert!(matches!(
            add_observation(&mut devices, identity(Some("NEW"), 1), usb, node("video1")),
            Err(CaptureError::StaleSnapshot { .. })
        ));
    }

    #[test]
    fn access_errors_keep_errno_and_classify_permission_and_removal() {
        let path = Path::new("/dev/video0");
        for errno in [1, 13] {
            let error = access_error(path, "open", io::Error::from_raw_os_error(errno));
            match error {
                CaptureError::PermissionDenied {
                    source,
                    operation: "open",
                    ..
                } => assert_eq!(source.raw_os_error(), Some(errno)),
                other => panic!("unexpected error: {other:?}"),
            }
        }
        for errno in [2, 19] {
            assert!(matches!(
                access_error(path, "open", io::Error::from_raw_os_error(errno)),
                CaptureError::DeviceGone { .. }
            ));
        }
        match access_error(path, "query_cap", io::Error::from_raw_os_error(5)) {
            CaptureError::Io {
                source,
                operation: "query_cap",
                ..
            } => assert_eq!(source.raw_os_error(), Some(5)),
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn negative_udev_return_preserves_reported_errno() {
        assert!(udev_result(0).is_ok());
        assert!(udev_result(1).is_ok());
        match udev_result(-13) {
            Err(CaptureError::Udev {
                source,
                operation: "udev_enumerate",
            }) => assert_eq!(source.raw_os_error(), Some(13)),
            other => panic!("unexpected result: {other:?}"),
        }
    }
}
