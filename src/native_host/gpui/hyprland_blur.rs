//! Ephemeral Hyprland blur override for the hidden XWayland chrome companion.

use std::{
    env, fs,
    future::{Future, poll_fn},
    io::{self, Read, Write},
    mem::{MaybeUninit, size_of},
    os::fd::AsRawFd,
    os::unix::{
        fs::{FileTypeExt, MetadataExt},
        net::UnixStream,
    },
    path::{Component, Path, PathBuf},
    pin::pin,
    process,
    task::Poll,
    time::{Duration, Instant},
};

use async_io::{Async, Timer};
use serde::Deserialize;

use super::CHROME_APP_ID;

const DEADLINE: Duration = Duration::from_millis(250);
const PROBE_LIMIT: Duration = Duration::from_millis(50);
const MAP_RETRY: Duration = Duration::from_millis(16);
const RESPONSE_LIMIT: usize = 256;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum BlurOutcome {
    Applied,
    Unavailable,
}

/// Run on the GPUI background executor while the companion is still parked.
pub(super) async fn apply() -> BlurOutcome {
    let deadline = Instant::now() + DEADLINE;
    before(deadline, apply_before(deadline), BlurOutcome::Unavailable).await
}

async fn before<T: Copy>(deadline: Instant, operation: impl Future<Output = T>, expired: T) -> T {
    let mut operation = pin!(operation);
    let mut timeout = pin!(Timer::at(deadline));
    poll_fn(|cx| {
        if timeout.as_mut().poll(cx).is_ready() {
            return Poll::Ready(expired);
        }
        operation.as_mut().poll(cx)
    })
    .await
}

async fn apply_before(deadline: Instant) -> BlurOutcome {
    // SAFETY: geteuid takes no arguments and has no memory preconditions.
    let uid = unsafe { libc::geteuid() };
    let Some(hypr) = hypr_directory(uid) else {
        return BlurOutcome::Unavailable;
    };
    let command = format!(
        r#"/repl return (function()
    local target
    for _, w in ipairs(hl.get_windows({{ class = "{CHROME_APP_ID}", mapped = true }})) do
        if w.xwayland and w.pid == {} then
            if target then return "furami-blur-ambiguous" end
            target = w
        end
    end
    if not target then return "furami-blur-not-mapped" end
    if not target.hidden then return "furami-blur-rejected" end
    local result = hl.dispatch(hl.dsp.window.set_prop({{
        window = target, prop = "no_blur", value = "1",
    }}))
    return result.ok and ("furami-blur-set " .. target.address) or "furami-blur-rejected"
end)()"#,
        process::id(),
    );
    let mut response = [0; RESPONSE_LIMIT];
    let preferred = preferred_instance();
    let preferred_socket = preferred
        .as_ref()
        .and_then(|name| instance_socket(&hypr.join(name), uid));
    let mut retry_preferred = None;
    if let Some(socket) = preferred_socket {
        match probe(&socket, uid, &command, &mut response, deadline).await {
            Attempt::Applied => return BlurOutcome::Applied,
            Attempt::NotMapped => retry_preferred = Some(socket),
            Attempt::Unavailable => {}
        }
    }
    let mut sockets = Vec::new();
    if let Ok(entries) = fs::read_dir(hypr) {
        for entry in entries {
            if Instant::now() >= deadline {
                break;
            }
            if let Ok(entry) = entry
                && preferred.as_ref() != Some(&entry.file_name())
                && let Some(socket) = instance_socket(&entry.path(), uid)
            {
                sockets.push(socket);
            }
        }
    }
    while !sockets.is_empty() || retry_preferred.is_some() {
        let mut index = 0;
        while index < sockets.len() {
            if Instant::now() >= deadline {
                return BlurOutcome::Unavailable;
            }
            match probe(&sockets[index], uid, &command, &mut response, deadline).await {
                Attempt::Applied => return BlurOutcome::Applied,
                Attempt::NotMapped => index += 1,
                Attempt::Unavailable => {
                    sockets.remove(index);
                }
            }
        }
        if let Some(socket) = retry_preferred.take() {
            sockets.insert(0, socket);
        }
        if !sockets.is_empty() {
            Timer::after(MAP_RETRY).await;
        }
    }
    BlurOutcome::Unavailable
}

fn hypr_directory(uid: u32) -> Option<PathBuf> {
    let runtime = PathBuf::from(env::var_os("XDG_RUNTIME_DIR")?);
    if !runtime.is_absolute()
        || !owned_directory(&runtime, uid, 0o077)
        || !fs::canonicalize(&runtime).is_ok_and(|canonical| canonical == runtime)
    {
        return None;
    }
    let hypr = runtime.join("hypr");
    owned_directory(&hypr, uid, 0o022).then_some(hypr)
}

fn preferred_instance() -> Option<std::ffi::OsString> {
    env::var_os("HYPRLAND_INSTANCE_SIGNATURE").filter(|name| {
        let mut components = Path::new(name).components();
        matches!(components.next(), Some(Component::Normal(component)) if component == name.as_os_str())
            && components.next().is_none()
    })
}

fn owned_directory(path: &Path, uid: u32, forbidden_mode: u32) -> bool {
    fs::symlink_metadata(path)
        .is_ok_and(|info| info.is_dir() && info.uid() == uid && info.mode() & forbidden_mode == 0)
}

fn instance_socket(directory: &Path, uid: u32) -> Option<PathBuf> {
    if !owned_directory(directory, uid, 0o022) {
        return None;
    }
    let socket = directory.join(".socket.sock");
    owned_socket(&socket, uid).then_some(socket)
}

fn owned_socket(path: &Path, uid: u32) -> bool {
    fs::symlink_metadata(path).is_ok_and(|info| info.file_type().is_socket() && info.uid() == uid)
}

#[derive(Clone, Copy)]
enum Attempt {
    Applied,
    NotMapped,
    Unavailable,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Readback {
    no_blur: bool,
}

// Cap the full setter/readback exchange so an unresponsive instance cannot use
// the entire discovery deadline before another instance is tried.
async fn probe(
    socket: &Path,
    uid: u32,
    setter: &str,
    response: &mut [u8; RESPONSE_LIMIT],
    deadline: Instant,
) -> Attempt {
    let deadline = deadline.min(Instant::now() + PROBE_LIMIT);
    before(
        deadline,
        try_socket(socket, uid, setter, response, deadline),
        Attempt::Unavailable,
    )
    .await
}

async fn try_socket(
    socket: &Path,
    uid: u32,
    setter: &str,
    response: &mut [u8; RESPONSE_LIMIT],
    deadline: Instant,
) -> Attempt {
    let Ok(reply) = request(socket, uid, setter, response, deadline).await else {
        return Attempt::Unavailable;
    };
    let Ok(reply) = std::str::from_utf8(reply) else {
        return Attempt::Unavailable;
    };
    let reply = reply.trim();
    if reply == "furami-blur-not-mapped" {
        return Attempt::NotMapped;
    }
    let Some(address) = reply.strip_prefix("furami-blur-set ") else {
        return Attempt::Unavailable;
    };
    let Some(hex) = address.strip_prefix("0x") else {
        return Attempt::Unavailable;
    };
    if !(1..=16).contains(&hex.len()) || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Attempt::Unavailable;
    }
    let readback = format!("j/getprop address:{address} no_blur");
    if let Ok(reply) = request(socket, uid, &readback, response, deadline).await
        && serde_json::from_slice::<Readback>(reply).is_ok_and(|value| value.no_blur)
    {
        return Attempt::Applied;
    }
    Attempt::Unavailable
}

async fn request<'a>(
    socket: &Path,
    uid: u32,
    command: &str,
    response: &'a mut [u8; RESPONSE_LIMIT],
    deadline: Instant,
) -> io::Result<&'a [u8]> {
    if !owned_socket(socket, uid) {
        return Err(io::ErrorKind::PermissionDenied.into());
    }
    check_deadline(deadline)?;
    let stream = Async::<UnixStream>::connect(socket).await?;
    verify_peer(stream.get_ref(), uid)?;
    check_deadline(deadline)?;
    let command = command.as_bytes();
    if stream.write_with(|mut socket| socket.write(command)).await? != command.len() {
        return Err(io::ErrorKind::WriteZero.into());
    }
    let mut len = 0;
    let mut excess = [0];
    loop {
        check_deadline(deadline)?;
        let buffer = if len == response.len() {
            &mut excess[..]
        } else {
            &mut response[len..]
        };
        let count = stream.read_with(|mut socket| socket.read(buffer)).await?;
        check_deadline(deadline)?;
        if count == 0 {
            return Ok(&response[..len]);
        }
        if len == response.len() {
            return Err(io::ErrorKind::InvalidData.into());
        }
        len += count;
    }
}

fn verify_peer(stream: &UnixStream, uid: u32) -> io::Result<()> {
    let mut credentials = MaybeUninit::<libc::ucred>::uninit();
    let expected_len = size_of::<libc::ucred>() as libc::socklen_t;
    let mut len = expected_len;
    // SAFETY: credentials points to writable storage of `len` bytes, and
    // the live stream owns the fd for the duration of the call.
    let result = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            credentials.as_mut_ptr().cast(),
            &mut len,
        )
    };
    if result == -1 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a successful SO_PEERCRED with the full length initialized ucred.
    if len != expected_len || unsafe { credentials.assume_init() }.uid != uid {
        return Err(io::ErrorKind::PermissionDenied.into());
    }
    Ok(())
}

fn check_deadline(deadline: Instant) -> io::Result<()> {
    if Instant::now() >= deadline {
        Err(io::ErrorKind::TimedOut.into())
    } else {
        Ok(())
    }
}
