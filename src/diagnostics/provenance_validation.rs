// Shared dependency-free validators for projected provenance scalars.
//
// `build.rs` includes this file to project the trusted media recipe/manifest
// fields into the report, and the `inventory` tests include it to assert that
// personal text is refused, so one vocabulary governs both sides. It must
// compile inside a build script and at a non-initial item position: plain
// comments only, standard library only, no `use` items, no visibility
// modifiers.

/// The pinned media recipe's exact source set. Any other source name — for
/// example a private-looking identifier — is refused, so provenance becomes a
/// typed unavailability rather than an arbitrary-key passthrough. Adding a media
/// source means updating this whitelist.
const KNOWN_SOURCES: [&str; 7] = [
    "ca_certificates",
    "cxx_qt",
    "ffmpeg",
    "libplacebo",
    "mpv",
    "qt",
    "vulkan_headers",
];

fn charset(value: &str, max: usize, allowed: fn(u8) -> bool) -> bool {
    !value.is_empty() && value.len() <= max && value.bytes().all(allowed)
}

fn hex_range(value: &str, min: usize, max: usize) -> bool {
    (min..=max).contains(&value.len()) && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn has_digit(value: &str) -> bool {
    value.bytes().any(|byte| byte.is_ascii_digit())
}

fn starts_numeric(value: &str) -> bool {
    value.as_bytes().first().is_some_and(u8::is_ascii_digit)
}

fn is_source_name(value: &str) -> bool {
    KNOWN_SOURCES.contains(&value)
}

/// A client API version is dotted digits, e.g. `2.5.0`.
fn is_dotted_digits(value: &str) -> bool {
    charset(value, 32, |byte| byte.is_ascii_digit() || byte == b'.')
        && !value.starts_with('.')
        && !value.ends_with('.')
        && !value.contains("..")
}

/// A version token is numeric-leading, optionally `v`-tagged, or the explicit
/// `<name>-v<version>-gitlink` gitlink form. A personal name that merely happens
/// to contain digits or hyphens — such as
/// `alsa_input.usb-GENKI_ShadowCast_3_KT044001-02.analog-stereo` — is refused.
fn is_version(value: &str) -> bool {
    if !charset(value, 64, |byte| {
        byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'+' | b'~' | b'_' | b':')
    }) {
        return false;
    }
    if starts_numeric(value) {
        return true;
    }
    if let Some(tagged) = value.strip_prefix('v') {
        return starts_numeric(tagged);
    }
    match value.rsplit_once("-v") {
        Some((name, rest)) => {
            !name.is_empty()
                && charset(name, 64, |byte| {
                    byte.is_ascii_lowercase()
                        || byte.is_ascii_digit()
                        || byte == b'-'
                        || byte == b'_'
                })
                && rest
                    .strip_suffix("-gitlink")
                    .is_some_and(starts_numeric)
        }
        None => false,
    }
}

/// A Qt build kit is a bounded lowercase Linux platform selector with a digit,
/// e.g. `linux_gcc_64`.
fn is_kit(value: &str) -> bool {
    charset(value, 32, |byte| {
        byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_' || byte == b'-'
    }) && value.starts_with("linux_")
        && has_digit(value)
}

fn is_source_scalar(key: &str, value: &str) -> bool {
    match key {
        "sha256" => hex_range(value, 64, 64),
        "commit" => hex_range(value, 7, 40),
        "version" => is_version(value),
        "client_api" => is_dotted_digits(value),
        "kit" => is_kit(value),
        _ => false,
    }
}

/// An OCI-style image reference that must be digest-pinned: a lowercase relative
/// name plus an exact `@sha256:<64 hex>` suffix. `/home/private-user` and an
/// unpinned `anthodev/furami` are refused.
fn is_container(value: &str) -> bool {
    if value.is_empty() || value.len() > 256 {
        return false;
    }
    let Some((name, digest)) = value.split_once('@') else {
        return false;
    };
    let Some(hex) = digest.strip_prefix("sha256:") else {
        return false;
    };
    !name.is_empty()
        && hex_range(hex, 64, 64)
        && !name.starts_with('/')
        && !name.ends_with('/')
        && !name.contains("//")
        && name.bytes().all(|byte| {
            byte.is_ascii_lowercase()
                || byte.is_ascii_digit()
                || matches!(byte, b'.' | b'/' | b':' | b'-' | b'_')
        })
}

/// An APT snapshot stamp is bounded and digit-bearing, e.g. `20260920T000000Z`.
fn is_snapshot(value: &str) -> bool {
    charset(value, 32, |byte| {
        byte.is_ascii_alphanumeric() || matches!(byte, b':' | b'.' | b'-')
    }) && has_digit(value)
}

/// A platform selector is lowercase and always qualified by a `/`.
fn is_platform(value: &str) -> bool {
    charset(value, 32, |byte| {
        byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'/' | b'_' | b'-')
    }) && value.contains('/')
}
