//! Strict bounded qualification grammar. Draft edits never apply implicitly.

use crate::domain::{
    capture::{
        CandidateId, CaptureModeRequest, CapturedFourCc, DeviceIdentity, FrameRate, FrameSize,
        InputFormatPolicy, ObservationEpoch, SelectionToken, UsbTopology, WatchId, WatchStamp,
    },
    render::RendererSettings,
    state::{AttemptId, DraftRevision, ProductPhase, TreatmentPass},
};
use std::num::NonZeroU8;

pub(crate) const LEGACY_COMMAND_MAX_BYTES: usize = 256;
/// Cap for `draft-renderer`, which decodes the untouched remainder as strict JSON.
pub(crate) const STRUCTURED_COMMAND_MAX_BYTES: usize = 65_536;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ExpectedCleanup {
    Complete,
    Draining,
    Blocked,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ExpectedState {
    pub phase: ProductPhase,
    pub treatment_pass: Option<TreatmentPass>,
    pub apply: u64,
    pub attempt: u64,
    pub cleanup: ExpectedCleanup,
}
#[derive(Debug, PartialEq)]
pub(crate) enum Command {
    Snapshot,
    Video(DraftRevision, CaptureModeRequest),
    Identity(DraftRevision, DeviceIdentity),
    Audio(DraftRevision, bool),
    Source(DraftRevision, String),
    Renderer(DraftRevision, RendererSettings),
    Apply(ExpectedState, DraftRevision),
    Restart(ExpectedState),
    Reconnect(ExpectedState),
    /// Explicit recovery choice for an ambiguous target: the token must be
    /// one of the currently offered candidates and the expected state guards
    /// a stale choice.
    Choose(ExpectedState, SelectionToken),
    Close(ExpectedState),
    Quit(ExpectedState),
    Volume(AttemptId, u8),
    Mute(AttemptId, bool),
    Pause(AttemptId),
    Resume(AttemptId),
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
        Some("ApplyingTreatments") => ProductPhase::ApplyingTreatments,
        Some("RestoringTreatments") => ProductPhase::RestoringTreatments,
        Some("PausePending") => ProductPhase::PausePending,
        Some("Paused") => ProductPhase::Paused,
        Some("Validating") => ProductPhase::Validating,
        Some("ClosingOld") => ProductPhase::ClosingOld,
        Some("OpeningCandidate") => ProductPhase::OpeningCandidate,
        Some("CleaningFailedCandidate") => ProductPhase::CleaningFailedCandidate,
        Some("ValidatingPrior") => ProductPhase::ValidatingPrior,
        Some("OpeningRestore") => ProductPhase::OpeningRestore,
        Some("CleaningFailedRestore") => ProductPhase::CleaningFailedRestore,
        Some("ValidatingResume") => ProductPhase::ValidatingResume,
        Some("ClosingResume") => ProductPhase::ClosingResume,
        Some("OpeningResume") => ProductPhase::OpeningResume,
        Some("CleaningFailedResume") => ProductPhase::CleaningFailedResume,
        Some("ErrorWithActiveRestored") => ProductPhase::ErrorWithActiveRestored,
        Some("ErrorWithoutActive") => ProductPhase::ErrorWithoutActive,
        Some("Disconnected") => ProductPhase::Disconnected,
        Some("Recovering") => ProductPhase::Recovering,
        Some("SelectionRequired") => ProductPhase::SelectionRequired,
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
        treatment_pass: match phase {
            ProductPhase::ApplyingTreatments => Some(TreatmentPass::Candidate),
            ProductPhase::RestoringTreatments => Some(TreatmentPass::Restore),
            _ => None,
        },
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

/// Inspect only the ASCII verb token; decode the untouched renderer JSON
/// remainder before the legacy 256-byte lexical gate.
pub(crate) fn is_structured_json_command(line: &str) -> bool {
    matches!(line.split_ascii_whitespace().next(), Some("draft-renderer"))
}

fn parse_structured_json(line: &str) -> Result<Command, &'static str> {
    match line.split_ascii_whitespace().next() {
        Some("draft-renderer") => parse_renderer(line),
        _ => Err("unknown qualification command"),
    }
}

fn parse_renderer(line: &str) -> Result<Command, &'static str> {
    if line.len() > STRUCTURED_COMMAND_MAX_BYTES {
        return Err("qualification renderer command exceeds 65536 bytes");
    }
    let syntax = "draft-renderer requires revision and strict RendererSettings JSON";
    let (_, remainder) = line
        .trim_start_matches(|value: char| value.is_ascii_whitespace())
        .split_once(|value: char| value.is_ascii_whitespace())
        .ok_or(syntax)?;
    let (revision, json) = remainder
        .trim_start_matches(|value: char| value.is_ascii_whitespace())
        .split_once(|value: char| value.is_ascii_whitespace())
        .ok_or(syntax)?;
    let revision = DraftRevision::new(unsigned(Some(revision))?);
    // Deserialize the untouched remainder directly: no tokenization, lossy
    // normalization or Value buffering that could collapse duplicate keys.
    let renderer = serde_json::from_str::<RendererSettings>(json)
        .map_err(|_| "draft-renderer requires strict RendererSettings JSON")?;
    Ok(Command::Renderer(revision, renderer))
}

pub(crate) fn parse(line: &str) -> Result<Command, &'static str> {
    if is_structured_json_command(line) {
        return parse_structured_json(line);
    }
    if line.len() > LEGACY_COMMAND_MAX_BYTES {
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
            let value = fields.next().ok_or("auto or FourCC required")?;
            let format_policy = if value == "auto" {
                InputFormatPolicy::Auto
            } else {
                if value.len() != 4 || !value.bytes().all(|byte| byte.is_ascii_graphic()) {
                    return Err("FourCC requires four printable ASCII bytes");
                }
                let bytes: [u8; 4] = value.as_bytes().try_into().map_err(|_| "invalid FourCC")?;
                InputFormatPolicy::Manual {
                    fourcc: CapturedFourCc::from_bytes(bytes),
                }
            };
            let (width, height) = pair(fields.next(), 'x')?;
            let (numerator, denominator) = pair(fields.next(), '/')?;
            Command::Video(
                revision,
                CaptureModeRequest {
                    format_policy,
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
        "choose" => {
            let state = expected(&mut fields)?;
            let token_error = "choose requires nonzero watch, epoch and candidate identifiers";
            let token = SelectionToken {
                stamp: WatchStamp {
                    watch: WatchId::new(unsigned(fields.next())?).ok_or(token_error)?,
                    epoch: ObservationEpoch::new(unsigned(fields.next())?).ok_or(token_error)?,
                },
                candidate: CandidateId::new(unsigned(fields.next())?).ok_or(token_error)?,
            };
            Command::Choose(state, token)
        }
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
        "pause" => Command::Pause(
            AttemptId::new(unsigned(fields.next())?).ok_or("nonzero attempt required")?,
        ),
        "resume" => Command::Resume(
            AttemptId::new(unsigned(fields.next())?).ok_or("nonzero attempt required")?,
        ),
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
    fn retired_filter_command_is_not_recognized() {
        for line in [
            "draft-filters",
            "draft-filters 0 {}",
            " draft-filters\t0 null",
        ] {
            assert!(!is_structured_json_command(line));
            assert_eq!(parse(line), Err("unknown qualification command"));
        }
    }

    #[test]
    fn legacy_byte_and_lexical_limits_remain_unchanged() {
        let exact = format!(
            "snapshot{}",
            " ".repeat(LEGACY_COMMAND_MAX_BYTES - "snapshot".len())
        );
        assert_eq!(exact.len(), LEGACY_COMMAND_MAX_BYTES);
        assert_eq!(parse(&exact), Ok(Command::Snapshot));
        assert_eq!(
            parse(&(exact + " ")),
            Err("qualification command exceeds 256 bytes")
        );
        for line in ["snapshot\0", "snapshot\u{a0}", "draft-source 0 a\u{a0}b"] {
            assert_eq!(parse(line), Err("NUL or non-ASCII whitespace forbidden"));
        }
    }

    #[test]
    fn treatment_phase_implies_pass_without_an_extra_wire_token() {
        for (phase, pass) in [
            ("ApplyingTreatments", TreatmentPass::Candidate),
            ("RestoringTreatments", TreatmentPass::Restore),
        ] {
            let Command::Close(expected) = parse(&format!("close {phase} 7 3 Complete")).unwrap()
            else {
                panic!("close identity")
            };
            assert_eq!(expected.treatment_pass, Some(pass));
            assert!(parse(&format!("close {phase} 7 3 Complete {pass:?}")).is_err());
        }
        for retired_phase in ["ApplyingFilters", "RestoringFilters"] {
            assert!(parse(&format!("close {retired_phase} 7 3 Complete")).is_err());
        }
    }
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
    fn correlated_close_and_restart_accept_pause_presentation_phases() {
        for phase in ["PausePending", "Paused"] {
            assert!(parse(&format!("close {phase} 1 1 Complete")).is_ok());
            assert!(parse(&format!("restart {phase} 1 1 Complete")).is_ok());
        }
    }
    #[test]
    fn recovery_states_are_first_class_expected_phases() {
        for phase in ["Disconnected", "Recovering", "SelectionRequired"] {
            assert!(parse(&format!("reconnect {phase} 4 5 Complete")).is_ok());
            assert!(parse(&format!("restart {phase} 4 5 Complete")).is_ok());
            assert!(parse(&format!("close {phase} 4 5 Complete")).is_ok());
            assert!(parse(&format!("quit {phase} 4 5 Complete")).is_ok());
        }
    }
    #[test]
    fn choose_requires_expected_state_and_nonzero_token_identifiers() {
        let expected = "SelectionRequired 4 5 Complete";
        assert_eq!(
            parse(&format!("choose {expected} 2 3 7")),
            Ok(Command::Choose(
                ExpectedState {
                    phase: ProductPhase::SelectionRequired,
                    treatment_pass: None,
                    apply: 4,
                    attempt: 5,
                    cleanup: ExpectedCleanup::Complete,
                },
                SelectionToken {
                    stamp: WatchStamp {
                        watch: WatchId::new(2).unwrap(),
                        epoch: ObservationEpoch::new(3).unwrap(),
                    },
                    candidate: CandidateId::new(7).unwrap(),
                },
            ))
        );
        for line in [
            format!("choose {expected}"),
            format!("choose {expected} 0 3 7"),
            format!("choose {expected} 2 0 7"),
            format!("choose {expected} 2 3 0"),
            format!("choose {expected} 2 3"),
            format!("choose {expected} 2 3 7 extra"),
            "choose Stopped 2 3 7".to_owned(),
        ] {
            assert!(parse(&line).is_err(), "{line}");
        }
    }
    #[test]
    fn explicit_playback_commands_require_single_nonzero_attempt() {
        assert_eq!(
            parse("pause 7"),
            Ok(Command::Pause(AttemptId::new(7).unwrap()))
        );
        assert_eq!(
            parse("resume 9"),
            Ok(Command::Resume(AttemptId::new(9).unwrap()))
        );
        for line in [
            "pause",
            "pause 0",
            "pause 7 extra",
            "resume",
            "resume 0",
            "resume 9 extra",
        ] {
            assert!(parse(line).is_err(), "{line}");
        }
    }
    #[test]
    fn correlated_close_and_restart_accept_resume_presentation_phases() {
        for phase in [
            "ValidatingResume",
            "ClosingResume",
            "OpeningResume",
            "CleaningFailedResume",
        ] {
            assert!(parse(&format!("close {phase} 1 1 Complete")).is_ok());
            assert!(parse(&format!("restart {phase} 1 1 Complete")).is_ok());
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
    fn draft_video_auto_is_intent_only_and_manual_bgr3_is_preserved() {
        let Command::Video(_, auto) = parse("draft-video 0 auto 1920x1080 60000/1001").unwrap()
        else {
            panic!("video command")
        };
        assert_eq!(auto.format_policy, InputFormatPolicy::Auto);
        assert_eq!(auto.rate, FrameRate::new(60000, 1001).unwrap());
        let Command::Video(_, manual) = parse("draft-video 1 BGR3 1920x1080 60/1").unwrap() else {
            panic!("video command")
        };
        assert_eq!(
            manual.format_policy,
            InputFormatPolicy::Manual {
                fourcc: CapturedFourCc::from_bytes(*b"BGR3")
            }
        );
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

    fn reference_renderer_json() -> &'static str {
        include_str!("../../tests/fixtures/FUR-016/reference-renderer.json").trim()
    }
    fn reference_renderer() -> RendererSettings {
        serde_json::from_str(reference_renderer_json()).unwrap()
    }
    fn renderer_line(revision: u64, json: &str) -> String {
        format!("draft-renderer {revision} {json}")
    }

    #[test]
    fn renderer_decodes_strict_json_with_exact_verb_revision_and_separator() {
        let renderer = reference_renderer();
        assert_eq!(
            parse(&format!(
                "\t draft-renderer\t18446744073709551615\t{json} ",
                json = reference_renderer_json()
            )),
            Ok(Command::Renderer(
                DraftRevision::new(u64::MAX),
                renderer.clone()
            ))
        );
        for prefix in [
            "draft-renderer",
            "draft-rendererx 0 ",
            "draft-renderers 0 ",
            "draft-renderer\u{a0}0 ",
            "draft-renderer 0\u{a0}",
            "draft-renderer -1 ",
            "draft-renderer +1 ",
            "draft-renderer 18446744073709551616 ",
            "draft-renderer 0",
        ] {
            assert!(
                parse(&format!("{prefix}{}", reference_renderer_json())).is_err(),
                "{prefix:?}"
            );
        }
        assert!(parse("draft-renderer").is_err());
        assert!(parse("draft-renderer 0").is_err());
        for prefix in ["", " ", "\t", "  "] {
            assert!(
                parse(&format!(
                    "{prefix}draft-renderer 0 {renderer_json}",
                    renderer_json = reference_renderer_json()
                ))
                .is_ok(),
                "{prefix:?}"
            );
        }
        // Only the surviving exact JSON verb gets structured-command handling.
        assert!(!is_structured_json_command("\t draft-filters\t0 {}"));
        assert!(is_structured_json_command("\t draft-renderer\t0 {}"));
        assert!(!is_structured_json_command("draft-filtersx 0 {}"));
        assert!(!is_structured_json_command("draft-renderers 0 {}"));
        assert!(!is_structured_json_command("snapshot draft-renderer 0 {}"));
        // A recognized verb with trailing junk still fails decoding, not the gate.
        assert!(is_structured_json_command("draft-renderer 0 {} extra"));
        assert!(parse("draft-renderer 0 {} extra").is_err());
    }

    #[test]
    fn renderer_rejects_malformed_null_duplicates_and_unknown_fields() {
        for json in [
            "null",
            "[]",
            "{}",
            r#"{"scale":"lanczos"}"#,
            r#"{"scale":"lanczos","cscale":"auto","dscale":"auto","scale_antiring":0.0,"dither":"fruit","deband":{"enabled":false,"iterations":1,"grain":32},"interpolation":false,"extra":0}"#,
            r#"{"scale":"lanczos","cscale":"auto","dscale":"auto","scale_antiring":0.0,"dither":"fruit","deband":[false,1,32],"interpolation":false}"#,
            r#"{"scale":"not-a-kernel","cscale":"auto","dscale":"auto","scale_antiring":0.0,"dither":"fruit","deband":{"enabled":false,"iterations":1,"grain":32},"interpolation":false}"#,
            r#"{"scale":"lanczos","cscale":"auto"}"#,
        ] {
            assert!(parse(&renderer_line(2, json)).is_err(), "{json}");
        }
    }

    #[test]
    fn renderer_rejects_raw_duplicate_fields_without_collapsing_them() {
        let json = reference_renderer_json();
        let duplicates = [
            json.replace(
                r#""scale":"ewa_lanczossharp""#,
                r#""scale":"ewa_lanczossharp","scale":"bilinear""#,
            ),
            json.replace(
                r#""interpolation":false"#,
                r#""interpolation":false,"interpolation":true"#,
            ),
            json.replace(
                r#"{"enabled":true,"iterations":2,"grain":16}"#,
                r#"{"enabled":true,"enabled":true,"iterations":2,"grain":16}"#,
            ),
            // Invalid value first, valid last: a Value-buffer collapse would hide it.
            json.replace(
                r#""scale_antiring":0.7"#,
                r#""scale_antiring":2.0,"scale_antiring":0.7"#,
            ),
            // Valid value first, invalid last.
            json.replace(
                r#""scale_antiring":0.7"#,
                r#""scale_antiring":0.7,"scale_antiring":2.0"#,
            ),
        ];
        for duplicate in duplicates {
            assert!(parse(&renderer_line(2, &duplicate)).is_err(), "{duplicate}");
        }
    }

    #[test]
    fn renderer_shares_the_65536_cap_and_skips_the_legacy_gate() {
        let json = reference_renderer_json();
        let prefix = "draft-renderer 4 ";
        let pad = STRUCTURED_COMMAND_MAX_BYTES - prefix.len() - json.len();
        let exact = format!("{prefix}{}{json}", " ".repeat(pad));
        assert_eq!(exact.len(), STRUCTURED_COMMAND_MAX_BYTES);
        assert!(exact.len() > LEGACY_COMMAND_MAX_BYTES);
        assert_eq!(
            parse(&exact),
            Ok(Command::Renderer(
                DraftRevision::new(4),
                reference_renderer()
            ))
        );
        let oversize = format!("{exact} ");
        assert_eq!(oversize.len(), STRUCTURED_COMMAND_MAX_BYTES + 1);
        assert_eq!(
            parse(&oversize),
            Err("qualification renderer command exceeds 65536 bytes")
        );
        // The legacy lexical gate is unchanged for non-structured verbs.
        assert!(parse(&format!("snapshot{}", " ".repeat(256))).is_err());
    }
}
