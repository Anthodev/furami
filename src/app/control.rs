//! Strict, value-only qualification commands. Every command names its attempt.

use crate::media::controller::Generation;

#[derive(Debug, Eq, PartialEq)]
pub(super) enum Command {
    Open(u64),
    Restart(Generation),
    Audio(Generation, bool),
    Volume(Generation, u8),
    Mute(Generation, bool),
    Close(Generation),
    Quit(Generation),
}

pub(super) fn parse(line: &str) -> Result<Command, &'static str> {
    if line.len() > 256 {
        return Err("qualification command exceeds 256 bytes");
    }
    let mut fields = line.split_ascii_whitespace();
    let verb = fields.next().ok_or("missing qualification command")?;
    let expected = fields
        .next()
        .filter(|value| value.bytes().all(|byte| byte.is_ascii_digit()))
        .and_then(|value| value.parse::<u64>().ok())
        .ok_or("qualification command requires unsigned generation")?;
    let command = if verb == "open" {
        Command::Open(expected)
    } else {
        let generation =
            Generation::new(expected).ok_or("qualification command requires nonzero generation")?;
        match verb {
            "restart" => Command::Restart(generation),
            "enable" => Command::Audio(generation, true),
            "disable" => Command::Audio(generation, false),
            "close" => Command::Close(generation),
            "quit" => Command::Quit(generation),
            "volume" => {
                let volume = fields
                    .next()
                    .filter(|value| value.bytes().all(|byte| byte.is_ascii_digit()))
                    .and_then(|value| value.parse::<u8>().ok())
                    .filter(|value| *value <= 100)
                    .ok_or("volume requires playback percent 0..100")?;
                Command::Volume(generation, volume)
            }
            "mute" => Command::Mute(
                generation,
                match fields.next() {
                    Some("on") => true,
                    Some("off") => false,
                    _ => return Err("mute requires on or off"),
                },
            ),
            _ => return Err("unknown qualification command"),
        }
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
    fn commands_require_nonzero_generation_and_exact_arity() {
        for line in [
            "restart",
            "restart 0",
            "restart -1",
            "restart 1 extra",
            "enable",
            "disable 0",
            "quit",
            "close",
            "volume 1",
            "mute 1",
            "unknown 1",
        ] {
            assert!(parse(line).is_err(), "{line}");
        }
        let generation = Generation::new(42).unwrap();
        assert_eq!(parse("restart 42").unwrap(), Command::Restart(generation));
        assert_eq!(
            parse("enable 42").unwrap(),
            Command::Audio(generation, true)
        );
        assert_eq!(
            parse("disable 42").unwrap(),
            Command::Audio(generation, false)
        );
        assert_eq!(parse("close 42").unwrap(), Command::Close(generation));
        assert_eq!(parse("quit 42").unwrap(), Command::Quit(generation));
    }

    #[test]
    fn gain_values_are_playback_only_bounded_and_explicit() {
        let generation = Generation::new(7).unwrap();
        assert_eq!(parse("volume 7 0").unwrap(), Command::Volume(generation, 0));
        assert_eq!(
            parse("volume 7 100").unwrap(),
            Command::Volume(generation, 100)
        );
        assert_eq!(parse("mute 7 on").unwrap(), Command::Mute(generation, true));
        assert_eq!(
            parse("mute 7 off").unwrap(),
            Command::Mute(generation, false)
        );
        for line in [
            "volume 7 101",
            "volume 7 -1",
            "volume 7 nan",
            "mute 7 toggle",
            "mute 7 1",
            "mute 7 off extra",
        ] {
            assert!(parse(line).is_err(), "{line}");
        }
    }

    #[test]
    fn attempt_ids_preserve_full_u64_and_overlong_input_is_rejected() {
        let last = Generation::new(u64::MAX).unwrap();
        assert_eq!(
            parse("restart 18446744073709551615").unwrap(),
            Command::Restart(last)
        );
        assert!(parse("restart 18446744073709551616").is_err());
        assert!(parse(&format!("restart 1{}", " ".repeat(256))).is_err());
    }

    #[test]
    fn only_open_accepts_zero_as_initial_expected_last_generation() {
        assert_eq!(parse("open 0").unwrap(), Command::Open(0));
        assert_eq!(parse("open 42").unwrap(), Command::Open(42));
        for line in [
            "open",
            "open -1",
            "open 0 extra",
            "open 18446744073709551616",
            "restart 0",
            "enable 0",
            "disable 0",
            "close 0",
            "quit 0",
            "volume 0 50",
            "mute 0 on",
        ] {
            assert!(parse(line).is_err(), "{line}");
        }
    }
}
