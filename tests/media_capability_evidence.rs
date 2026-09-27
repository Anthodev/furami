//! FUR-002 parser proof against selected, unchanged raw probe transcripts.
//!
//! Curated records preserve argv, executable/stack identities, status, and raw
//! stdout/stderr bytes from three completed hardware runs. Their tracked hash
//! index binds every fixture byte without requiring local full probe bundles,
//! compiled media binaries, or hardware on CI.

use std::collections::HashMap;
use std::path::Path;

use furami::media::capabilities::{
    Backend, Capability, CapabilityError, CaptureFormat, CatalogFilter, Component, Decoder,
    GpuContext, PixelFormat, Player, ProbeCollection, ProbeRun, RendererOption, VideoOutput,
};

const COMMON_BUNDLE: &str = "common";
const EFFECTIVE_BUNDLE: &str = "effective";
const HIGHRES_BUNDLE: &str = "highres";

/// Expected exact facts shared by both bundles (same build-G stack).
const STACK_SHA256: &str = "bd2e290c5c094eaf19536dc91251e4304dc6d46c546df640fa4ece96bcb03cee";
const MPV_SHA256: &str = "d926eadbdfd76f3dc8cb773f9fb7e3a8ab0047ca4e7148807c9e33b67985f6f2";
const FFMPEG_SHA256: &str = "1c38d09555005425ea30f80e03bfb45c4a65a3880013ed72d56658e2d416744e";
const GPU_DEVICE_NAME: &str = "AMD Radeon RX 7900 XTX (RADV NAVI31)";
const CAPTURE_SOURCE: &str = "alsa_input.usb-GENKI_ShadowCast_3_KT044001-02.analog-stereo";
const K7_SINK: &str = "alsa_output.usb-Fosi_Audio_Fosi_Audio_K7-00.analog-stereo";
const EFFECTIVE_SINK: &str = "easyeffects_sink";

fn sha256(data: &[u8]) -> String {
    // Compact FIPS 180-4 implementation; avoids a new dependency.
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut state = [
        0x6a09e667u32,
        0xbb67ae85,
        0x3c6ef372,
        0xa54ff53a,
        0x510e527f,
        0x9b05688c,
        0x1f83d9ab,
        0x5be0cd19,
    ];
    let mut message = data.to_vec();
    let bit_len = (data.len() as u64) * 8;
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&bit_len.to_be_bytes());
    for block in message.chunks_exact(64) {
        let mut w = [0u32; 64];
        for (i, word) in block.chunks_exact(4).enumerate() {
            w[i] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = state;
        for (i, &ki) in K.iter().enumerate() {
            let t1 = h
                .wrapping_add(e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25))
                .wrapping_add((e & f) ^ ((!e) & g))
                .wrapping_add(ki)
                .wrapping_add(w[i]);
            let t2 = (a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22))
                .wrapping_add((a & b) ^ (a & c) ^ (b & c));
            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        for (slot, value) in state.iter_mut().zip([a, b, c, d, e, f, g, h]) {
            *slot = slot.wrapping_add(value);
        }
    }
    state.iter().map(|word| format!("{word:08x}")).collect()
}

fn read(dir: &Path, relative: &str) -> Vec<u8> {
    std::fs::read(dir.join(relative))
        .unwrap_or_else(|error| panic!("missing curated fixture {relative}: {error}"))
}

/// Verify the curated record and every selected raw stream against the tracked
/// index before reconstructing the public collection. The source manifest
/// digest is provenance, not a claim that absent full logs were replayed.
fn load_bundle(name: &str) -> ProbeCollection {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/FUR-002");
    let index: serde_json::Value =
        serde_json::from_slice(&read(&root, "index.json")).expect("valid fixture index");
    let expected = &index["bundles"][name];
    assert!(expected.is_object(), "unindexed bundle: {name}");
    let dir = root.join(name);
    let record_bytes = read(&dir, "record.json");
    assert_eq!(
        sha256(&record_bytes),
        expected["record_sha256"].as_str().unwrap(),
        "{name} record differs from tracked index"
    );
    let record: serde_json::Value =
        serde_json::from_slice(&record_bytes).expect("valid curated record");
    assert_eq!(record["source_bundle"], expected["source_bundle"]);
    assert_eq!(
        record["source_manifest_sha256"],
        expected["source_manifest_sha256"]
    );
    assert_eq!(record["source_status"], "complete");
    assert_eq!(record["source_probes"], expected["source_probe_counts"]);
    assert_eq!(record["source_probes"]["planned"], 31);
    assert_eq!(record["source_probes"]["ran"], 31);
    assert_eq!(record["source_probes"]["not_run"], 0);

    let mpv = &record["stack"]["bins"]["mpv"];
    let ffmpeg = &record["stack"]["bins"]["ffmpeg"];
    assert_eq!(
        record["stack"]["identity_file"]["sha256"],
        expected["stack_sha256"]
    );
    assert_eq!(mpv["sha256"], expected["mpv_sha256"]);
    assert_eq!(ffmpeg["sha256"], expected["ffmpeg_sha256"]);
    assert_eq!(
        record["stack"]["bins"]["libmpv"]["sha256"],
        expected["libmpv_sha256"]
    );
    let indexed_probes = expected["probes"].as_object().unwrap();
    let commands = record["commands"].as_object().unwrap();
    assert_eq!(
        commands.len(),
        indexed_probes.len(),
        "missing indexed probe"
    );
    ProbeCollection {
        stack_sha256: expected["stack_sha256"].as_str().unwrap().to_owned(),
        mpv_sha256: expected["mpv_sha256"].as_str().unwrap().to_owned(),
        ffmpeg_sha256: expected["ffmpeg_sha256"].as_str().unwrap().to_owned(),
        ffmpeg_path: ffmpeg["path"].as_str().unwrap().to_owned(),
        commands: commands
            .iter()
            .map(|(id, command)| {
                let indexed = &indexed_probes[id];
                assert!(indexed.is_object(), "unindexed probe {name}/{id}");
                let mut transcript = HashMap::new();
                for stream in ["stdout", "stderr"] {
                    let descriptor = &command[stream];
                    let count = descriptor["bytes"].as_u64().unwrap() as usize;
                    // Empty raw streams have the standard SHA-256 empty digest,
                    // without adding dozens of zero-byte files to the fixture.
                    let bytes = if count == 0 {
                        Vec::new()
                    } else {
                        read(&dir, descriptor["path"].as_str().unwrap())
                    };
                    assert_eq!(bytes.len(), count, "{name}/{id}/{stream} byte count");
                    assert_eq!(
                        sha256(&bytes),
                        descriptor["sha256"].as_str().unwrap(),
                        "{name}/{id}/{stream} digest"
                    );
                    assert_eq!(
                        descriptor["sha256"],
                        indexed[format!("{stream}_sha256")],
                        "{name}/{id}/{stream} differs from tracked index"
                    );
                    transcript.insert(stream, bytes);
                }
                let executable = command["executable"].as_str().unwrap();
                let executable_sha256 = command["executable_sha256"].as_str().unwrap().to_owned();
                if executable == mpv["path"].as_str().unwrap() {
                    assert_eq!(executable_sha256, mpv["sha256"].as_str().unwrap());
                }
                if executable == ffmpeg["path"].as_str().unwrap() {
                    assert_eq!(executable_sha256, ffmpeg["sha256"].as_str().unwrap());
                }
                (
                    id.clone(),
                    ProbeRun {
                        argv: command["argv"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .map(|arg| arg.as_str().unwrap().to_owned())
                            .collect(),
                        executable_sha256,
                        ran: command["ran"].as_bool().unwrap(),
                        exit_code: command["exit_code"].as_i64().map(|code| code as i32),
                        timed_out: command["timed_out"].as_bool().unwrap(),
                        stdout: transcript.remove("stdout").unwrap(),
                        stderr: transcript.remove("stderr").unwrap(),
                    },
                )
            })
            .collect(),
    }
}

fn capabilities(bundle: &str) -> ProbeCollection {
    let probes = load_bundle(bundle);
    Capabilities::from_probes(&probes).expect("recorded stack identity parses");
    probes
}

use furami::media::capabilities::Capabilities;

fn assert_stack_alignment(common: &ProbeCollection, effective: &ProbeCollection) {
    assert_eq!(common.stack_sha256, STACK_SHA256);
    assert_eq!(effective.stack_sha256, STACK_SHA256);
    assert_eq!(common.mpv_sha256, MPV_SHA256);
    assert_eq!(effective.mpv_sha256, MPV_SHA256);
    assert_eq!(common.ffmpeg_sha256, FFMPEG_SHA256);
    assert_eq!(effective.ffmpeg_sha256, FFMPEG_SHA256);
    assert_eq!(common.ffmpeg_path, effective.ffmpeg_path);
}

/// Positive consumer-visible facts proven by the common bundle.
#[test]
fn common_bundle_proves_positive_stack_facts() {
    let probes = capabilities(COMMON_BUNDLE);
    let caps = Capabilities::from_probes(&probes).unwrap();
    let stack = STACK_SHA256;

    for capability in [
        Capability::VideoOutput(VideoOutput::GpuNext),
        Capability::GpuContext(GpuContext::X11Vk),
        Capability::ListedFilter(CatalogFilter::Format),
        Capability::ListedFilter(CatalogFilter::Eq),
        Capability::ListedFilter(CatalogFilter::Unsharp),
        Capability::ListedFilter(CatalogFilter::Hqdn3d),
        Capability::ListedFilter(CatalogFilter::Bwdif),
        Capability::OperationalFilter(CatalogFilter::Format),
        Capability::OperationalFilter(CatalogFilter::Eq),
        Capability::OperationalFilter(CatalogFilter::Unsharp),
        Capability::OperationalFilter(CatalogFilter::Hqdn3d),
        Capability::OperationalFilter(CatalogFilter::Bwdif),
        Capability::Backend(Backend::V4l2),
        Capability::Backend(Backend::Pulse),
        Capability::PixelFormat(PixelFormat::Nv12),
        Capability::PixelFormat(PixelFormat::Yuyv422),
        Capability::Decoder(Decoder::Mjpeg),
        Capability::VulkanRender,
        Capability::RendererValue(RendererOption::Scale, "ewa_lanczossharp"),
        Capability::RendererValue(RendererOption::Cscale, "ewa_lanczossharp"),
        Capability::RendererValue(RendererOption::Dscale, "mitchell"),
        Capability::RendererValue(RendererOption::ScaleAntiring, "0.7"),
        Capability::RendererValue(RendererOption::Dither, "error-diffusion"),
        Capability::RendererValue(RendererOption::Deband, "yes"),
        Capability::RendererValue(RendererOption::DebandIterations, "2"),
        Capability::RendererValue(RendererOption::DebandGrain, "16"),
        Capability::LiveInput {
            format: CaptureFormat::Nv12,
            player: Player::Ffmpeg,
        },
        Capability::LiveInput {
            format: CaptureFormat::Yuyv422,
            player: Player::Ffmpeg,
        },
        Capability::LiveInput {
            format: CaptureFormat::Mjpeg,
            player: Player::Ffmpeg,
        },
        Capability::LiveInput {
            format: CaptureFormat::Nv12,
            player: Player::Mpv,
        },
        Capability::LiveInput {
            format: CaptureFormat::Yuyv422,
            player: Player::Mpv,
        },
        Capability::PulseSourceCapture,
        Capability::PulseSinkMonitor,
    ] {
        caps.require(stack, capability)
            .unwrap_or_else(|error| panic!("{capability:?}: {error}"));
    }

    // Exact selected hardware identity from the recorded render transcript.
    let render =
        String::from_utf8(probes.commands["mpv_render_gpu_next_x11vk"].stdout.clone()).unwrap();
    assert!(
        render.contains(&format!(
            "[vo/gpu-next/libplacebo]     Device Name: {GPU_DEVICE_NAME}"
        )),
        "recorded render did not select {GPU_DEVICE_NAME}"
    );
}

/// The K7-sink bundle recorded a silent/intercepted direct source relay: the
/// parser must refuse PulseSourcePlayback there instead of counting the row.
#[test]
fn common_bundle_refuses_pulse_source_playback() {
    let probes = capabilities(COMMON_BUNDLE);
    let caps = Capabilities::from_probes(&probes).unwrap();
    let error = caps
        .require(
            STACK_SHA256,
            Capability::PulseSourcePlayback {
                source: CAPTURE_SOURCE,
                sink: K7_SINK,
            },
        )
        .expect_err("silent relay must not prove source playback");
    assert!(
        matches!(error, CapabilityError::Unverified { .. }),
        "expected Unverified, got {error:?}"
    );
}

/// The audible bundle proves effective-sink playback, and its busy V4L2 rows
/// must NOT count as capture capability.
#[test]
fn effective_bundle_proves_playback_and_rejects_busy_rows() {
    let probes = capabilities(EFFECTIVE_BUNDLE);
    let caps = Capabilities::from_probes(&probes).unwrap();
    let stack = STACK_SHA256;

    caps.require(
        stack,
        Capability::PulseSourcePlayback {
            source: CAPTURE_SOURCE,
            sink: EFFECTIVE_SINK,
        },
    )
    .expect("audible relay must prove source playback");
    caps.require(stack, Capability::PulseSourceCapture)
        .expect("audible bundle recorded a live source capture");
    caps.require(stack, Capability::PulseSinkMonitor)
        .expect("audible bundle recorded a monitor probe");
    caps.require(stack, Capability::VulkanRender)
        .expect("same stack renders");

    // FFmpeg reported a busy card. mpv failed to open the same device during
    // this run, but did not expose a device-specific cause in its transcript.
    for id in [
        "v4l2_open_nv12_ffmpeg",
        "v4l2_open_yuyv_ffmpeg",
        "v4l2_open_mjpeg_ffmpeg",
    ] {
        let run = &probes.commands[id];
        assert_eq!(
            run.exit_code,
            Some(240),
            "{id} did not record the busy failure"
        );
        assert!(String::from_utf8_lossy(&run.stderr).contains("Device or resource busy"));
    }
    for id in ["v4l2_open_nv12_mpv", "v4l2_open_yuyv_mpv"] {
        let run = &probes.commands[id];
        assert_eq!(run.exit_code, Some(2), "{id} did not record a failed open");
        assert!(String::from_utf8_lossy(&run.stdout).contains("avformat_open_input() failed"));
    }
    for format in [
        CaptureFormat::Nv12,
        CaptureFormat::Yuyv422,
        CaptureFormat::Mjpeg,
    ] {
        for player in [Player::Ffmpeg, Player::Mpv] {
            let error = caps
                .require(stack, Capability::LiveInput { format, player })
                .expect_err(&format!("busy bundle must not prove {format:?}/{player:?}"));
            assert!(
                matches!(error, CapabilityError::Unverified { .. }),
                "expected Unverified for busy {format:?}/{player:?}, got {error:?}"
            );
        }
    }
}

/// Stack/executable identity must align across the two bundles before either
/// is interpreted, and a foreign stack hash is a typed StackMismatch.
#[test]
fn bundles_share_stack_identity_and_mismatch_is_typed() {
    let common = capabilities(COMMON_BUNDLE);
    let effective = capabilities(EFFECTIVE_BUNDLE);
    assert_stack_alignment(&common, &effective);

    let caps = Capabilities::from_probes(&common).unwrap();
    let error = caps
        .require(
            "0000000000000000000000000000000000000000000000000000000000000000",
            Capability::VideoOutput(VideoOutput::GpuNext),
        )
        .expect_err("foreign stack hash must not match recorded probes");
    assert_eq!(
        error,
        CapabilityError::StackMismatch {
            expected: "0000000000000000000000000000000000000000000000000000000000000000".into(),
            actual: STACK_SHA256.into(),
        }
    );
}

/// Missing evidence must surface as the parser's typed errors, not a panic.
#[test]
fn missing_probe_keeps_typed_error_boundary() {
    let mut probes = capabilities(COMMON_BUNDLE);
    probes.commands.remove("mpv_vo_list");
    let caps = Capabilities::from_probes(&probes).unwrap();
    let error = caps
        .require(STACK_SHA256, Capability::VideoOutput(VideoOutput::GpuNext))
        .expect_err("dropped probe must not pass");
    assert_eq!(
        error,
        CapabilityError::Unverified {
            probe: "mpv_vo_list",
            reason: "probe not recorded"
        }
    );
}

/// A missing component row inside a real listing is Missing, not Unverified.
#[test]
fn absent_component_row_is_missing() {
    let mut probes = capabilities(COMMON_BUNDLE);
    let run = probes.commands.get_mut("mpv_vf_list").unwrap();
    let stdout = String::from_utf8(std::mem::take(&mut run.stdout)).unwrap();
    let stripped = stdout.replacen("  format           force output format", "", 1);
    run.stdout = stripped.into_bytes();
    let caps = Capabilities::from_probes(&probes).unwrap();
    let error = caps
        .require(
            STACK_SHA256,
            Capability::ListedFilter(CatalogFilter::Format),
        )
        .expect_err("removed row must not be found");
    assert_eq!(
        error,
        CapabilityError::Missing {
            component: Component::MpvNativeFilter,
            probe: "mpv_vf_list"
        }
    );
}

/// The high-resolution run proves negotiated formats, not merely requested
/// tuples. In particular, YUYV 1440p60 must not be advertised as observed.
#[test]
fn highres_yuyv_request_is_renegotiated_to_50_fps() {
    let probes = capabilities(HIGHRES_BUNDLE);
    assert_eq!(probes.stack_sha256, STACK_SHA256);
    assert_eq!(probes.mpv_sha256, MPV_SHA256);
    assert_eq!(probes.ffmpeg_sha256, FFMPEG_SHA256);

    let nv12 = &probes.commands["v4l2_open_nv12_ffmpeg"];
    let yuyv = &probes.commands["v4l2_open_yuyv_ffmpeg"];
    let mjpeg = &probes.commands["v4l2_open_mjpeg_ffmpeg"];
    for run in [nv12, yuyv, mjpeg] {
        assert_eq!(run.exit_code, Some(0));
        assert!(
            run.argv
                .windows(2)
                .any(|args| args[0] == "-video_size" && args[1] == "2560x1440")
        );
        assert!(
            run.argv
                .windows(2)
                .any(|args| args[0] == "-framerate" && args[1] == "60")
        );
        assert!(String::from_utf8_lossy(&run.stderr).contains("frame=    8"));
    }
    let nv12_log = String::from_utf8_lossy(&nv12.stderr);
    assert!(nv12_log.contains("nv12, 2560x1440, 2654208 kb/s, 60 fps"));
    assert!(String::from_utf8_lossy(&mjpeg.stderr).contains("2560x1440, 60 fps"));
    let yuyv_log = String::from_utf8_lossy(&yuyv.stderr);
    assert!(yuyv_log.contains("The driver changed the time per frame from 1/60 to 1/50"));
    assert!(yuyv_log.contains("yuyv422, 2560x1440, 2949120 kb/s, 50 fps"));

    let nv12_mpv = &probes.commands["v4l2_open_nv12_mpv"];
    let yuyv_mpv = &probes.commands["v4l2_open_yuyv_mpv"];
    assert_eq!(nv12_mpv.exit_code, Some(0));
    assert_eq!(yuyv_mpv.exit_code, Some(0));
    assert!(String::from_utf8_lossy(&nv12_mpv.stdout).contains("rawvideo 2560x1440 60 fps"));
    assert!(String::from_utf8_lossy(&yuyv_mpv.stdout).contains("rawvideo 2560x1440 50 fps"));
}
