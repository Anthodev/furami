//! Strict bounded qualification grammar. Draft edits never apply implicitly.

use crate::domain::{
    capture::{CaptureMode, CapturedFourCc, DeviceIdentity, FrameRate, FrameSize, UsbTopology},
    state::{AttemptId, DraftRevision, ProductPhase},
};
use std::num::NonZeroU8;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ExpectedCleanup {
    Complete,
    Draining,
    Blocked,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ExpectedState {
    pub phase: ProductPhase,
    pub apply: u64,
    pub attempt: u64,
    pub cleanup: ExpectedCleanup,
}
#[derive(Debug, Eq, PartialEq)]
pub(crate) enum Command {
    Snapshot,
    Video(DraftRevision, CaptureMode),
    Identity(DraftRevision, DeviceIdentity),
    Audio(DraftRevision, bool),
    Source(DraftRevision, String),
    Apply(ExpectedState, DraftRevision),
    Restart(ExpectedState),
    Reconnect(ExpectedState),
    Close(ExpectedState),
    Quit(ExpectedState),
    Volume(AttemptId, u8),
    Mute(AttemptId, bool),
}

fn unsigned(value: Option<&str>) -> Result<u64, &'static str> {
    value
        .filter(|value| !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()))
        .and_then(|value| value.parse().ok())
        .ok_or("unsigned u64 required")
}
fn part(value: &str) -> Result<u32, &'static str> {
    u32::try_from(unsigned(Some(value))?)
        .ok()
        .filter(|value| *value > 0)
        .ok_or("positive u32 required")
}
fn pair(value: Option<&str>, separator: char) -> Result<(u32, u32), &'static str> {
    let (a, b) = value
        .and_then(|value| value.split_once(separator))
        .ok_or("exact positive pair required")?;
    Ok((part(a)?, part(b)?))
}
fn expected(
    fields: &mut std::str::SplitAsciiWhitespace<'_>,
) -> Result<ExpectedState, &'static str> {
    let phase = match fields.next() {
        Some("Stopped") => ProductPhase::Stopped,
        Some("Active") => ProductPhase::Active,
        Some("Validating") => ProductPhase::Validating,
        Some("ClosingOld") => ProductPhase::ClosingOld,
        Some("OpeningCandidate") => ProductPhase::OpeningCandidate,
        Some("CleaningFailedCandidate") => ProductPhase::CleaningFailedCandidate,
        Some("ValidatingPrior") => ProductPhase::ValidatingPrior,
        Some("OpeningRestore") => ProductPhase::OpeningRestore,
        Some("CleaningFailedRestore") => ProductPhase::CleaningFailedRestore,
        Some("ErrorWithActiveRestored") => ProductPhase::ErrorWithActiveRestored,
        Some("ErrorWithoutActive") => ProductPhase::ErrorWithoutActive,
        Some("Stopping") => ProductPhase::Stopping,
        Some("ShutdownReady") => ProductPhase::ShutdownReady,
        _ => return Err("exact product phase required"),
    };
    let apply = unsigned(fields.next())?;
    let attempt = unsigned(fields.next())?;
    let cleanup = match fields.next() {
        Some("Complete") => ExpectedCleanup::Complete,
        Some("Draining") => ExpectedCleanup::Draining,
        Some("Blocked") => ExpectedCleanup::Blocked,
        _ => return Err("exact cleanup state required"),
    };
    Ok(ExpectedState {
        phase,
        apply,
        attempt,
        cleanup,
    })
}
fn hex(value: Option<&str>) -> Result<u16, &'static str> {
    value
        .filter(|value| value.len() == 4 && value.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .and_then(|value| u16::from_str_radix(value, 16).ok())
        .ok_or("four hexadecimal VID/PID digits required")
}

pub(crate) fn parse(line: &str) -> Result<Command, &'static str> {
    if line.len() > 256 {
        return Err("qualification command exceeds 256 bytes");
    }
    if line
        .chars()
        .any(|value| value == '\0' || (value.is_whitespace() && !value.is_ascii()))
    {
        return Err("NUL or non-ASCII whitespace forbidden");
    }
    let mut fields = line.split_ascii_whitespace();
    let verb = fields.next().ok_or("missing qualification command")?;
    let command = match verb {
        "snapshot" => Command::Snapshot,
        "draft-video" => {
            let revision = DraftRevision::new(unsigned(fields.next())?);
            let fourcc = fields
                .next()
                .filter(|value| {
                    value.len() == 4 && value.bytes().all(|byte| byte.is_ascii_graphic())
                })
                .ok_or("FourCC requires four printable ASCII bytes")?;
            let bytes: [u8; 4] = fourcc.as_bytes().try_into().map_err(|_| "invalid FourCC")?;
            let (width, height) = pair(fields.next(), 'x')?;
            let (numerator, denominator) = pair(fields.next(), '/')?;
            Command::Video(
                revision,
                CaptureMode {
                    captured_fourcc: CapturedFourCc::from_bytes(bytes),
                    size: FrameSize::new(width, height).map_err(|_| "invalid size")?,
                    rate: FrameRate::new(numerator, denominator).map_err(|_| "invalid rate")?,
                },
            )
        }
        "draft-identity" => {
            let revision = DraftRevision::new(unsigned(fields.next())?);
            let vendor = hex(fields.next())?;
            let product = hex(fields.next())?;
            let controller = fields
                .next()
                .ok_or("technical controller required")?
                .to_owned();
            let ports = fields
                .next()
                .ok_or("nonzero port chain required")?
                .split('.')
                .map(|port| {
                    u8::try_from(unsigned(Some(port))?)
                        .ok()
                        .and_then(NonZeroU8::new)
                        .ok_or("nonzero u8 port required")
                })
                .collect::<Result<Vec<_>, _>>()?;
            let serial = match fields.next() {
                Some("-") => None,
                Some(value) => Some(value.to_owned()),
                None => return Err("serial or - required"),
            };
            let topology =
                UsbTopology::new(controller, ports).map_err(|_| "invalid technical topology")?;
            Command::Identity(
                revision,
                DeviceIdentity::new(vendor, product, topology, serial)
                    .map_err(|_| "invalid identity")?,
            )
        }
        "draft-audio" => {
            let revision = DraftRevision::new(unsigned(fields.next())?);
            let enabled = match fields.next() {
                Some("enable") => true,
                Some("disable") => false,
                _ => return Err("audio requires enable or disable"),
            };
            Command::Audio(revision, enabled)
        }
        "draft-source" => Command::Source(
            DraftRevision::new(unsigned(fields.next())?),
            fields
                .next()
                .ok_or("enumerated source name required")?
                .to_owned(),
        ),
        "apply" | "open" => Command::Apply(
            expected(&mut fields)?,
            DraftRevision::new(unsigned(fields.next())?),
        ),
        "restart" => Command::Restart(expected(&mut fields)?),
        "reconnect" => Command::Reconnect(expected(&mut fields)?),
        "close" => Command::Close(expected(&mut fields)?),
        "quit" => Command::Quit(expected(&mut fields)?),
        "volume" => {
            let attempt =
                AttemptId::new(unsigned(fields.next())?).ok_or("nonzero attempt required")?;
            let volume = u8::try_from(unsigned(fields.next())?)
                .ok()
                .filter(|value| *value <= 100)
                .ok_or("volume requires playback percent 0..100")?;
            Command::Volume(attempt, volume)
        }
        "mute" => {
            let attempt =
                AttemptId::new(unsigned(fields.next())?).ok_or("nonzero attempt required")?;
            let muted = match fields.next() {
                Some("on") => true,
                Some("off") => false,
                _ => return Err("mute requires on or off"),
            };
            Command::Mute(attempt, muted)
        }
        _ => return Err("unknown qualification command"),
    };
    if fields.next().is_some() {
        return Err("unexpected qualification command argument");
    }
    Ok(command)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn apply_and_lifecycle_commands_require_exact_state_and_revision() {
        assert_eq!(
            parse("open Stopped 0 0 Complete 0"),
            parse("apply Stopped 0 0 Complete 0")
        );
        for command in ["restart", "reconnect", "close", "quit"] {
            assert!(parse(&format!("{command} ErrorWithoutActive 2 3 Complete")).is_ok());
            for suffix in [
                "",
                " 2",
                " Active 2 3",
                " Active 2 3 Invalid",
                " Active 2 3 Complete extra",
            ] {
                assert!(parse(&format!("{command}{suffix}")).is_err());
            }
        }
        for line in [
            "open 0",
            "apply Active 1 1 Complete",
            "apply active 1 1 Complete 0",
            "apply Active -1 1 Complete 0",
            "apply Active 1 1 Complete 0 extra",
        ] {
            assert!(parse(line).is_err(), "{line}");
        }
    }
    #[test]
    fn draft_tuple_is_exact_positive_and_identity_free() {
        assert!(matches!(
            parse("draft-video 0 YUYV 2560x1440 60000/1001"),
            Ok(Command::Video(_, _))
        ));
        for line in [
            "draft-video 0 YUYV 0x1440 60/1",
            "draft-video 0 NV12 1280x720 60/0",
            "draft-video 0 NV12 1280x720 60/1 extra",
            "draft-video 0 RGB 1280x720 60/1",
            "draft-video 0 NV12 /dev/video0 60/1",
        ] {
            assert!(parse(line).is_err(), "{line}");
        }
    }
    #[test]
    fn technical_identity_rejects_node_aliases_and_invalid_ports() {
        assert!(matches!(
            parse("draft-identity 4 32ed 3701 0000:0c:00.0 3.1 serial"),
            Ok(Command::Identity(_, _))
        ));
        assert!(parse("draft-identity 4 32ed 3701 controller 3 -").is_ok());
        for line in [
            "draft-identity 4 32ed 3701 controller 0 -",
            "draft-identity 4 32ed 3701 controller 256 -",
            "draft-identity 4 32ed 3701 controller 3. -",
            "draft-identity 4 0x32ed 3701 controller 3 -",
            "draft-identity 4 /dev/video0",
        ] {
            assert!(parse(line).is_err(), "{line}");
        }
    }
    #[test]
    fn gain_nonzero_attempt_percent_and_explicit_mute_remain_bounded() {
        for line in ["volume 7 0", "volume 7 100", "mute 7 on", "mute 7 off"] {
            assert!(parse(line).is_ok());
        }
        for line in [
            "volume 0 50",
            "volume 7 101",
            "volume 7 -1",
            "mute 7 toggle",
            "mute 7 off extra",
        ] {
            assert!(parse(line).is_err());
        }
        assert!(parse("volume 18446744073709551615 100").is_ok());
        assert!(parse("volume 18446744073709551616 100").is_err());
    }
    #[test]
    fn audio_edits_require_revision_and_no_legacy_restart_alias() {
        assert!(parse("draft-audio 0 enable").is_ok());
        assert!(parse("draft-audio 1 disable").is_ok());
        assert!(parse("draft-source 2 alsa_input.explicit").is_ok());
        for line in [
            "enable 1",
            "disable 1",
            "draft-audio 0 on",
            "draft-source 1",
            "draft-audio 1 enable extra",
        ] {
            assert!(parse(line).is_err());
        }
    }
    #[test]
    fn malformed_and_oversize_lines_never_parse_as_fragments() {
        assert_eq!(parse("snapshot"), Ok(Command::Snapshot));
        for line in ["snapshot extra", "snapshot\0", "snapshot\u{2003}", ""] {
            assert!(parse(line).is_err());
        }
        assert!(parse(&format!("snapshot{}", " ".repeat(256))).is_err());
    }
}
