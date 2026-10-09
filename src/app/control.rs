//! Strict bounded qualification grammar. Draft edits never apply implicitly.

use crate::domain::{
    capture::{
        CandidateId, CaptureMode, CapturedFourCc, DeviceIdentity, FrameRate, FrameSize,
        ObservationEpoch, SelectionToken, UsbTopology, WatchId, WatchStamp,
    },
    filters::FilterChain,
    state::{AttemptId, DraftRevision, FilterPass, ProductPhase},
};
use std::num::NonZeroU8;

pub(crate) const LEGACY_COMMAND_MAX_BYTES: usize = 256;
pub(crate) const FILTER_COMMAND_MAX_BYTES: usize = 65_536;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ExpectedCleanup {
    Complete,
    Draining,
    Blocked,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ExpectedState {
    pub phase: ProductPhase,
    pub filter_pass: Option<FilterPass>,
    pub apply: u64,
    pub attempt: u64,
    pub cleanup: ExpectedCleanup,
}
#[derive(Debug, PartialEq)]
pub(crate) enum Command {
    Snapshot,
    Video(DraftRevision, CaptureMode),
    Identity(DraftRevision, DeviceIdentity),
    Audio(DraftRevision, bool),
    Source(DraftRevision, String),
    Filters(DraftRevision, FilterChain),
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
        Some("ApplyingFilters") => ProductPhase::ApplyingFilters,
        Some("RestoringFilters") => ProductPhase::RestoringFilters,
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
        filter_pass: match phase {
            ProductPhase::ApplyingFilters => Some(FilterPass::LiveCandidate),
            ProductPhase::RestoringFilters => Some(FilterPass::LiveRestore),
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

/// Inspect only the ASCII verb token; JSON labels are not legacy grammar.
pub(crate) fn is_filter_command(line: &str) -> bool {
    line.split_ascii_whitespace().next() == Some("draft-filters")
}

fn parse_filters(line: &str) -> Result<Command, &'static str> {
    if line.len() > FILTER_COMMAND_MAX_BYTES {
        return Err("qualification filter command exceeds 65536 bytes");
    }
    let syntax = "draft-filters requires revision and strict FilterChain JSON";
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
    let chain = serde_json::from_str::<FilterChain>(json)
        .map_err(|_| "draft-filters requires strict FilterChain JSON")?;
    Ok(Command::Filters(revision, chain))
}

pub(crate) fn parse(line: &str) -> Result<Command, &'static str> {
    if is_filter_command(line) {
        return parse_filters(line);
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

    fn filter_chain(label: &str) -> FilterChain {
        use crate::domain::filters::{
            ColorLevels, Filter, FilterEntry, FormatParams, SdrGamma, SdrMatrix,
        };
        FilterChain::new(vec![FilterEntry::new(
            label.to_owned(),
            Filter::Format(FormatParams::new(
                SdrMatrix::Bt709,
                ColorLevels::Limited,
                SdrGamma::Bt1886,
            )),
            true,
        )])
        .unwrap()
    }

    fn filter_line(revision: u64, chain: &FilterChain) -> String {
        format!(
            "draft-filters {revision} {}",
            serde_json::to_string(chain).unwrap()
        )
    }

    #[test]
    fn filters_preserve_literal_unicode_whitespace_before_legacy_checks() {
        let label = format!("SDR\u{a0}{}", "x".repeat(300));
        let chain = filter_chain(&label);
        let line = filter_line(7, &chain);
        assert!(line.len() > LEGACY_COMMAND_MAX_BYTES);
        assert!(line.contains('\u{a0}'));
        assert_eq!(
            parse(&line),
            Ok(Command::Filters(DraftRevision::new(7), chain))
        );
    }

    #[test]
    fn filters_decode_escaped_controls_as_inert_label_text() {
        let chain = filter_chain("line\nnul\0tab\tquote\" slash\\ vf=set;quit");
        let line = filter_line(0, &chain);
        assert!(line.contains(r"\n"));
        assert!(line.contains(r"\u0000"));
        assert!(!line.contains('\n'));
        assert!(!line.contains('\0'));
        assert_eq!(
            parse(&line),
            Ok(Command::Filters(DraftRevision::new(0), chain))
        );
    }

    #[test]
    fn filters_preserve_order_and_disabled_entries_including_all_disabled() {
        use crate::domain::filters::FilterEntry;
        let filter = filter_chain("").entries()[0].filter().clone();
        for enabled in [[true, false, true], [false, false, false]] {
            let chain = FilterChain::new(
                ["first", "middle", "last"]
                    .into_iter()
                    .zip(enabled)
                    .map(|(label, enabled)| {
                        FilterEntry::new(label.to_owned(), filter.clone(), enabled)
                    })
                    .collect(),
            )
            .unwrap();
            assert_eq!(
                parse(&filter_line(3, &chain)),
                Ok(Command::Filters(DraftRevision::new(3), chain))
            );
        }
        assert_eq!(
            parse(&filter_line(3, &FilterChain::default())),
            Ok(Command::Filters(
                DraftRevision::new(3),
                FilterChain::default()
            ))
        );
    }

    #[test]
    fn filters_require_exact_verb_ascii_revision_and_separator() {
        let chain = FilterChain::default();
        let json = serde_json::to_string(&chain).unwrap();
        assert_eq!(
            parse(&format!("\t draft-filters\t18446744073709551615\t {json} ")),
            Ok(Command::Filters(DraftRevision::new(u64::MAX), chain))
        );
        for prefix in [
            "draft-filters",
            "draft-filtersx 0 ",
            "draft-filters-json 0 ",
            "draft-filters\u{a0}0 ",
            "draft-filters 0\u{a0}",
            "draft-filters -1 ",
            "draft-filters +1 ",
            "draft-filters 18446744073709551616 ",
            "draft-filters 0",
        ] {
            assert!(parse(&format!("{prefix}{json}")).is_err(), "{prefix:?}");
        }
        assert!(parse("draft-filters").is_err());
        assert!(parse("draft-filters 0").is_err());
        assert!(!is_filter_command("draft-filtersx 0 {}"));
        assert!(!is_filter_command("draft-filters\u{a0}0 {}"));
        assert!(is_filter_command("\t draft-filters\t0 {}"));
    }

    #[test]
    fn filters_reject_unescaped_controls_and_malformed_json() {
        let line = filter_line(2, &filter_chain("safe"));
        for control in ['\0', '\t', '\r', '\n', '\u{1f}'] {
            let invalid = line.replacen(
                r#""label":"safe""#,
                &format!("\"label\":\"bad{control}label\""),
                1,
            );
            assert!(parse(&invalid).is_err(), "{control:?}");
        }
        for json in [
            "null",
            "[]",
            "{}",
            r#"{"entries":null}"#,
            r#"{"entries":[]} trailing"#,
            r#"{"entries":[],"unknown":0}"#,
            r#"{"entries":["#,
            r#"{"entries":[{"label":"x","enabled":true,"filter":"vf=set"}]}"#,
        ] {
            assert!(parse(&format!("draft-filters 2 {json}")).is_err(), "{json}");
        }
    }

    #[test]
    fn filters_reject_raw_duplicate_fields_without_collapsing_them() {
        let line = filter_line(2, &filter_chain("safe"));
        let mut duplicates = vec![
            "draft-filters 2 {\"entries\":[],\"entries\":[]}".to_owned(),
            line.replacen(r#""label":"safe""#, r#""label":"first","label":"safe""#, 1),
            line.replacen(r#""enabled":true"#, r#""enabled":false,"enabled":true"#, 1),
            line.replacen(
                r#""kind":"format""#,
                r#""kind":"format","kind":"format""#,
                1,
            ),
        ];
        for fields in [
            r#""matrix":"invalid","matrix":"bt709""#,
            r#""matrix":"bt709","matrix":"invalid""#,
            r#""matrix":"bt709","matrix":"bt709""#,
        ] {
            duplicates.push(line.replacen(r#""matrix":"bt709""#, fields, 1));
        }
        for duplicate in duplicates {
            assert!(parse(&duplicate).is_err(), "{duplicate}");
        }
    }

    #[test]
    fn filters_reject_duplicate_labels_and_invalid_domain_parameters() {
        let chain = filter_chain("safe");
        let entry = serde_json::to_string(&chain.entries()[0]).unwrap();
        let duplicate_labels = format!("draft-filters 2 {{\"entries\":[{entry},{entry}]}}");
        assert!(parse(&duplicate_labels).is_err());
        let invalid =
            filter_line(2, &chain).replacen(r#""matrix":"bt709""#, r#""matrix":"not-a-matrix""#, 1);
        assert!(parse(&invalid).is_err());
    }

    #[test]
    fn filters_accept_exact_byte_cap_and_reject_one_more_byte() {
        let fixed_bytes = filter_line(4, &filter_chain("")).len();
        let label = "x".repeat(FILTER_COMMAND_MAX_BYTES - fixed_bytes);
        let chain = filter_chain(&label);
        let exact = filter_line(4, &chain);
        assert_eq!(exact.len(), FILTER_COMMAND_MAX_BYTES);
        assert_eq!(
            parse(&exact),
            Ok(Command::Filters(DraftRevision::new(4), chain))
        );
        let oversize = filter_line(4, &filter_chain(&(label + "x")));
        assert_eq!(oversize.len(), FILTER_COMMAND_MAX_BYTES + 1);
        assert_eq!(
            parse(&oversize),
            Err("qualification filter command exceeds 65536 bytes")
        );
    }

    #[test]
    fn filter_command_cap_counts_utf8_bytes_not_label_characters() {
        let fixed_bytes = filter_line(4, &filter_chain("")).len();
        let label = format!(
            "\u{a0}{}",
            "x".repeat(FILTER_COMMAND_MAX_BYTES - fixed_bytes - 2)
        );
        let exact = filter_line(4, &filter_chain(&label));
        assert_eq!(exact.len(), FILTER_COMMAND_MAX_BYTES);
        assert!(parse(&exact).is_ok());
        let oversize = filter_line(4, &filter_chain(&(label + "x")));
        assert_eq!(oversize.len(), FILTER_COMMAND_MAX_BYTES + 1);
        assert!(parse(&oversize).is_err());
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
    fn filter_phase_implies_pass_without_an_extra_wire_token() {
        for (phase, pass) in [
            ("ApplyingFilters", FilterPass::LiveCandidate),
            ("RestoringFilters", FilterPass::LiveRestore),
        ] {
            let Command::Close(expected) = parse(&format!("close {phase} 7 3 Complete")).unwrap()
            else {
                panic!("close identity")
            };
            assert_eq!(expected.filter_pass, Some(pass));
            assert!(parse(&format!("close {phase} 7 3 Complete {pass:?}")).is_err());
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
                    filter_pass: None,
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
