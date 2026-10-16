//! Responsibility: Qualified build inventory and build/source provenance.
//! Allowed dependencies: explicit DTOs and embedded build-time projections only.
//!
//! The report's `build_inventory` is assembled here from three embedded inputs,
//! with no runtime I/O:
//!
//! * `qualified-build.json` — a cleaned projection of the tracked FUR-002
//!   qualified bundles, i.e. the *reference* binding for the packaged media
//!   artifacts (`mpv`, `ffmpeg`, `libmpv`). It is a historical reference only;
//!   it never qualifies the current build, the Qt/libplacebo/provider set or any
//!   live session.
//! * `diagnostics_media_provider.json` — the frozen media prefix's `manifest.json`
//!   projected at build time by `build.rs` (versions/commits/allowed source
//!   hashes plus the three real media file digests). Absent or malformed input is
//!   a typed unavailability, never a guessed identity.
//! * `diagnostics_source_recipe.json` — the currently selected
//!   `packaging/media/stack.lock.json` projected at build time: the *wanted*
//!   source provenance, which never certifies the provider that produced the
//!   running build.
//!
//! The historical `stack_identity_sha256` identifies `stack_identity.txt`, not
//! the current lock; the two are never compared.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Exactly the packaged media artifacts whose digests can bind the reference.
pub const BINDING_SCOPE: [&str; 3] = ["mpv", "ffmpeg", "libmpv"];

const REFERENCE_JSON: &str = include_str!("qualified-build.json");
const MEDIA_PROVIDER_JSON: &str =
    include_str!(concat!(env!("OUT_DIR"), "/diagnostics_media_provider.json"));
const SOURCE_RECIPE_JSON: &str =
    include_str!(concat!(env!("OUT_DIR"), "/diagnostics_source_recipe.json"));

/// Why a projected or observed fact is unavailable. A code, never free text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InventoryUnavailable {
    /// No frozen media provider manifest was found next to `FURAMI_MEDIA_PREFIX`.
    ManifestAbsent,
    /// The frozen media provider manifest was present but unusable.
    ManifestMalformed,
    /// The selected media stack recipe could not be read at build time.
    RecipeAbsent,
    /// The selected media stack recipe was present but unusable.
    RecipeMalformed,
    /// The tracked FUR-002 fixtures do not record this fact.
    NotRecordedInFixture,
    /// The build provider projection itself is unavailable.
    ProviderUnavailable,
    /// The provider was observed, but one of the binding artifacts is missing.
    MediaArtifactsMissing,
}

/// An observed value or the reason it could not be observed. `None` never
/// becomes an invented default.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Availability<T> {
    Observed(T),
    Unavailable { reason: InventoryUnavailable },
}

/// The bundle and probe names that prove one fact. No argv, path or transcript.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Evidence {
    pub bundles: Vec<String>,
    pub probes: Vec<String>,
}

/// A presence claim and, only when a functional probe proves it, an operational
/// claim.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Fact {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub listed: Option<Evidence>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operational: Option<Evidence>,
}

/// A named capability row (`gpu-next`, `v4l2`, `nv12`, `mjpeg`, …).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NamedFact {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub listed: Option<Evidence>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operational: Option<Evidence>,
}

/// A catalog filter row, with the origin that decides its listing probe.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FilterFact {
    pub name: String,
    pub origin: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub listed: Option<Evidence>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operational: Option<Evidence>,
}

/// One renderer option and the exact value the reference render probe tested.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RendererValueFact {
    pub option: String,
    pub value: String,
    pub evidence: Evidence,
}

/// One capture format opened through one player.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LiveInputFact {
    pub format: String,
    pub player: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub listed: Option<Evidence>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operational: Option<Evidence>,
}

/// Audio facts proven by the reference bundles. Source and sink identities are
/// deliberately absent; only the capability itself is recorded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AudioCapabilities {
    pub source_capture: Fact,
    pub sink_monitor: Fact,
    pub source_to_sink_playback: Fact,
}

/// The reference capability catalog, each entry carrying its own evidence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReferenceCapabilities {
    pub video_outputs: Vec<NamedFact>,
    pub gpu_contexts: Vec<NamedFact>,
    pub backends: Vec<NamedFact>,
    pub pixel_formats: Vec<NamedFact>,
    pub decoders: Vec<NamedFact>,
    pub filters: Vec<FilterFact>,
    pub renderer_values: Vec<RendererValueFact>,
    pub live_inputs: Vec<LiveInputFact>,
    pub audio: AudioCapabilities,
}

/// The three `binding_scope` artifact digests of the qualified reference.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReferenceArtifacts {
    pub mpv: String,
    pub ffmpeg: String,
    pub libmpv: String,
}

/// One tracked reference bundle and its recorded digests.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReferenceBundle {
    pub name: String,
    pub source_bundle: String,
    pub record_sha256: String,
    pub source_manifest_sha256: String,
}

/// A reference version observed in the tracked fixtures.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VersionFacts {
    pub value: String,
    pub evidence: Evidence,
}

/// The reference identity: digests, bundles and the versions the fixtures prove.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReferenceIdentity {
    pub stack_identity_sha256: String,
    pub artifacts: ReferenceArtifacts,
    pub bundles: Vec<ReferenceBundle>,
    pub versions: BTreeMap<String, Availability<VersionFacts>>,
}

/// The cleaned qualified-build reference, embedded from `qualified-build.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReferenceSnapshot {
    pub schema: u32,
    pub kind: String,
    pub description: String,
    pub reference: ReferenceIdentity,
    pub capabilities: ReferenceCapabilities,
}

/// One allowed source's provenance scalars.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceVersion {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_api: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kit: Option<String>,
}

/// One real media artifact of the running build provider.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderArtifact {
    pub path: String,
    pub sha256: String,
}

/// The projected facts of the frozen media provider that produced this build.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BuildProviderFacts {
    pub lock_sha256: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub apt_snapshot: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub sources: BTreeMap<String, SourceVersion>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub artifacts: BTreeMap<String, ProviderArtifact>,
}

/// The projected facts of the currently selected media stack recipe.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceRecipeFacts {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<u64>,
    pub lock_sha256: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub apt_snapshot: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_date_epoch: Option<u64>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub sources: BTreeMap<String, SourceVersion>,
}

/// How the running build's media artifacts relate to the qualified reference.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReferenceBinding {
    /// All three `binding_scope` file digests match the reference.
    MatchingQualifiedMediaArtifacts,
    /// A complete three-file comparison that differs from the reference.
    DifferentMediaArtifacts,
    /// The comparison could not be completed.
    Unavailable { reason: InventoryUnavailable },
}

/// The report's `build_inventory`: reference, provider, recipe and binding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BuildInventory {
    pub reference_snapshot: ReferenceSnapshot,
    pub build_provider: Availability<BuildProviderFacts>,
    pub source_recipe: Availability<SourceRecipeFacts>,
    pub reference_binding: ReferenceBinding,
    pub binding_scope: Vec<String>,
}

/// Parse the embedded qualified-build reference.
///
/// The bytes are a compile-time constant validated by the module's own tests, so
/// a failure here is a build defect, not a runtime condition.
pub fn reference_snapshot() -> ReferenceSnapshot {
    serde_json::from_str(REFERENCE_JSON).expect("embedded qualified-build.json is well formed")
}

/// Assemble the report's build inventory. Pure: no filesystem, environment or
/// subprocess access.
pub fn build_inventory() -> BuildInventory {
    let reference_snapshot = reference_snapshot();
    let build_provider: Availability<BuildProviderFacts> =
        serde_json::from_str(MEDIA_PROVIDER_JSON)
            .expect("generated media provider projection is well formed");
    let source_recipe: Availability<SourceRecipeFacts> = serde_json::from_str(SOURCE_RECIPE_JSON)
        .expect("generated source recipe projection is well formed");
    let reference_binding =
        reference_binding(&reference_snapshot.reference.artifacts, &build_provider);
    BuildInventory {
        reference_snapshot,
        build_provider,
        source_recipe,
        reference_binding,
        binding_scope: BINDING_SCOPE
            .iter()
            .map(|name| (*name).to_owned())
            .collect(),
    }
}

/// Compare the three `binding_scope` file digests of the running build provider
/// against the qualified reference. Only a complete three-file comparison can
/// yield `DifferentMediaArtifacts`; anything incomplete stays `Unavailable`.
fn reference_binding(
    reference: &ReferenceArtifacts,
    provider: &Availability<BuildProviderFacts>,
) -> ReferenceBinding {
    let Availability::Observed(facts) = provider else {
        return ReferenceBinding::Unavailable {
            reason: InventoryUnavailable::ProviderUnavailable,
        };
    };
    let expected = [
        ("mpv", &reference.mpv),
        ("ffmpeg", &reference.ffmpeg),
        ("libmpv", &reference.libmpv),
    ];
    let mut matching = 0usize;
    for (logical, digest) in expected {
        let Some(artifact) = facts.artifacts.get(logical) else {
            return ReferenceBinding::Unavailable {
                reason: InventoryUnavailable::MediaArtifactsMissing,
            };
        };
        if artifact.sha256 == *digest {
            matching += 1;
        }
    }
    if matching == BINDING_SCOPE.len() {
        ReferenceBinding::MatchingQualifiedMediaArtifacts
    } else {
        ReferenceBinding::DifferentMediaArtifacts
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::test_support::capabilities::{
        Backend, Capabilities, Capability, CaptureFormat, CatalogFilter, Decoder, GpuContext,
        PixelFormat, Player, ProbeCollection, RendererOption, VideoOutput,
    };
    use crate::test_support::media_capability_evidence as evidence;

    // The same validator vocabulary `build.rs` enforces, included here so the
    // tests assert exactly what is enforced.
    include!("provenance_validation.rs");

    fn hex64(value: &str) -> bool {
        hex_range(value, 64, 64)
    }

    fn reference() -> ReferenceSnapshot {
        reference_snapshot()
    }

    fn capability_for_named(list: &str, name: &str) -> Capability<'static> {
        match (list, name) {
            ("video_outputs", "gpu-next") => Capability::VideoOutput(VideoOutput::GpuNext),
            ("gpu_contexts", "x11vk") => Capability::GpuContext(GpuContext::X11Vk),
            ("backends", "v4l2") => Capability::Backend(Backend::V4l2),
            ("backends", "pulse") => Capability::Backend(Backend::Pulse),
            ("pixel_formats", "nv12") => Capability::PixelFormat(PixelFormat::Nv12),
            ("pixel_formats", "yuyv422") => Capability::PixelFormat(PixelFormat::Yuyv422),
            ("decoders", "mjpeg") => Capability::Decoder(Decoder::Mjpeg),
            other => panic!("unmapped named fact {other:?}"),
        }
    }

    /// The functional capability a listing row's *open* actually proves. The
    /// FUR-002 parser has no operational backend/pixel-format/decoder step, so
    /// the open that proves usable capture is what an `operational` claim means.
    fn functional_capability_for_named(list: &str, name: &str) -> Capability<'static> {
        match (list, name) {
            ("backends", "v4l2") | ("pixel_formats", "nv12") => Capability::LiveInput {
                format: CaptureFormat::Nv12,
                player: Player::Ffmpeg,
            },
            ("pixel_formats", "yuyv422") => Capability::LiveInput {
                format: CaptureFormat::Yuyv422,
                player: Player::Ffmpeg,
            },
            ("decoders", "mjpeg") => Capability::LiveInput {
                format: CaptureFormat::Mjpeg,
                player: Player::Ffmpeg,
            },
            ("backends", "pulse") => Capability::PulseSourceCapture,
            other => panic!("unmapped functional fact {other:?}"),
        }
    }

    fn capability_for_filter(filter: &CatalogFilter) -> Capability<'static> {
        Capability::OperationalFilter(*filter)
    }

    fn catalog_filter(name: &str) -> CatalogFilter {
        match name {
            "format" => CatalogFilter::Format,
            "eq" => CatalogFilter::Eq,
            "unsharp" => CatalogFilter::Unsharp,
            "hqdn3d" => CatalogFilter::Hqdn3d,
            "bwdif" => CatalogFilter::Bwdif,
            other => panic!("unmapped filter {other}"),
        }
    }

    fn renderer_option(name: &str) -> RendererOption {
        match name {
            "scale" => RendererOption::Scale,
            "cscale" => RendererOption::Cscale,
            "dscale" => RendererOption::Dscale,
            "scale-antiring" => RendererOption::ScaleAntiring,
            "dither" => RendererOption::Dither,
            "deband" => RendererOption::Deband,
            "deband-iterations" => RendererOption::DebandIterations,
            "deband-grain" => RendererOption::DebandGrain,
            other => panic!("unmapped renderer option {other}"),
        }
    }

    fn capture_format(name: &str) -> CaptureFormat {
        match name {
            "nv12" => CaptureFormat::Nv12,
            "yuyv422" => CaptureFormat::Yuyv422,
            "mjpeg" => CaptureFormat::Mjpeg,
            other => panic!("unmapped capture format {other}"),
        }
    }

    fn player(name: &str) -> Player {
        match name {
            "ffmpeg" => Player::Ffmpeg,
            "mpv" => Player::Mpv,
            other => panic!("unmapped player {other}"),
        }
    }

    /// Merge the listing prerequisites recorded by the shared-stack common bundle
    /// into another bundle's own transcript. Guarded by exact stack and
    /// executable identity, and it only inserts commands the bundle did not
    /// record, so no recorded open is ever replaced.
    fn merge_stack_prerequisites(
        common: &ProbeCollection,
        bundle: &ProbeCollection,
    ) -> ProbeCollection {
        assert_eq!(
            common.stack_sha256, bundle.stack_sha256,
            "reference bundles must share the stack identity"
        );
        assert_eq!(common.mpv_sha256, bundle.mpv_sha256);
        assert_eq!(common.ffmpeg_sha256, bundle.ffmpeg_sha256);
        assert_eq!(common.ffmpeg_path, bundle.ffmpeg_path);
        let mut merged = bundle.clone();
        for (id, run) in &common.commands {
            merged
                .commands
                .entry(id.clone())
                .or_insert_with(|| run.clone());
        }
        merged
    }

    /// The tracked bundles, each with its own transcript, the identity-checked
    /// merge used for the parser's listing prerequisites, and a name index.
    struct BundleSet {
        recorded: Vec<ProbeCollection>,
        merged: Vec<ProbeCollection>,
        index: std::collections::BTreeMap<String, usize>,
    }

    fn bundle_set(names: &[&str]) -> BundleSet {
        let recorded: Vec<ProbeCollection> = names
            .iter()
            .map(|name| evidence::capabilities(name))
            .collect();
        // The high-res run recorded only its V4L2 opens; the parser requires the
        // stack listings first, so the identity-checked common prerequisites are
        // merged in for the capability checks. Citations are still verified
        // against `recorded`, never against this merge.
        let merged: Vec<ProbeCollection> = recorded
            .iter()
            .map(|bundle| merge_stack_prerequisites(&recorded[0], bundle))
            .collect();
        let index = names
            .iter()
            .enumerate()
            .map(|(position, name)| ((*name).to_owned(), position))
            .collect();
        BundleSet {
            recorded,
            merged,
            index,
        }
    }

    /// Every entry that claims evidence must be accepted by the existing FUR-002
    /// parser for exactly the recorded bundles. This is the mapping check that
    /// keeps the cleaned projection honest about what the fixtures prove.
    #[test]
    fn every_reference_claim_is_proved_by_the_fur002_parser() {
        fn require_all(
            bundles: &BundleSet,
            evidence: &Evidence,
            capability: Capability<'_>,
            label: &str,
        ) {
            assert!(
                !evidence.bundles.is_empty(),
                "{label}: evidence must name a bundle"
            );
            assert!(
                !evidence.probes.is_empty(),
                "{label}: evidence must name a probe"
            );
            for bundle in &evidence.bundles {
                let position = *bundles
                    .index
                    .get(bundle)
                    .unwrap_or_else(|| panic!("{label}: unknown bundle {bundle}"));
                for probe in &evidence.probes {
                    // A citation must be recorded by the named bundle itself,
                    // never by the merged prerequisite set.
                    let run = bundles.recorded[position]
                        .commands
                        .get(probe)
                        .unwrap_or_else(|| {
                            panic!("{label} / {bundle}: probe {probe} is not recorded there")
                        });
                    assert!(run.ran, "{label} / {bundle}: probe {probe} did not run");
                    // The cited probe must be load-bearing: dropping it from the
                    // merged transcript must make the parser refuse the claim.
                    let mut without = bundles.merged[position].clone();
                    without.commands.remove(probe);
                    let without_capabilities =
                        Capabilities::from_probes(&without).expect("identity still parses");
                    assert!(
                        without_capabilities
                            .require(evidence::STACK_SHA256, capability)
                            .is_err(),
                        "{label} / {bundle}: cited probe {probe} is not part of the proof"
                    );
                }
                Capabilities::from_probes(&bundles.merged[position])
                    .expect("recorded stack identity parses")
                    .require(evidence::STACK_SHA256, capability)
                    .unwrap_or_else(|error| panic!("{label} / {bundle}: {error}"));
            }
        }

        let snapshot = reference();
        let names = ["common", "effective", "highres"];
        let bundles = bundle_set(&names);

        for fact in &snapshot.capabilities.video_outputs {
            if let Some(evidence) = &fact.listed {
                require_all(
                    &bundles,
                    evidence,
                    Capability::VideoOutput(VideoOutput::GpuNext),
                    &fact.name,
                );
            }
            if let Some(evidence) = &fact.operational {
                require_all(&bundles, evidence, Capability::VulkanRender, &fact.name);
            }
        }
        for fact in &snapshot.capabilities.gpu_contexts {
            if let Some(evidence) = &fact.listed {
                require_all(
                    &bundles,
                    evidence,
                    Capability::GpuContext(GpuContext::X11Vk),
                    &fact.name,
                );
            }
            if let Some(evidence) = &fact.operational {
                require_all(&bundles, evidence, Capability::VulkanRender, &fact.name);
            }
        }
        for fact in &snapshot.capabilities.backends {
            if let Some(evidence) = &fact.listed {
                require_all(
                    &bundles,
                    evidence,
                    capability_for_named("backends", &fact.name),
                    &fact.name,
                );
            }
            if let Some(evidence) = &fact.operational {
                require_all(
                    &bundles,
                    evidence,
                    functional_capability_for_named("backends", &fact.name),
                    &fact.name,
                );
            }
        }
        for fact in &snapshot.capabilities.pixel_formats {
            if let Some(evidence) = &fact.listed {
                require_all(
                    &bundles,
                    evidence,
                    capability_for_named("pixel_formats", &fact.name),
                    &fact.name,
                );
            }
            if let Some(evidence) = &fact.operational {
                require_all(
                    &bundles,
                    evidence,
                    functional_capability_for_named("pixel_formats", &fact.name),
                    &fact.name,
                );
            }
        }
        for fact in &snapshot.capabilities.decoders {
            if let Some(evidence) = &fact.listed {
                require_all(
                    &bundles,
                    evidence,
                    capability_for_named("decoders", &fact.name),
                    &fact.name,
                );
            }
            if let Some(evidence) = &fact.operational {
                require_all(
                    &bundles,
                    evidence,
                    functional_capability_for_named("decoders", &fact.name),
                    &fact.name,
                );
            }
        }
        for fact in &snapshot.capabilities.filters {
            let filter = catalog_filter(&fact.name);
            if let Some(evidence) = &fact.listed {
                require_all(
                    &bundles,
                    evidence,
                    Capability::ListedFilter(filter),
                    &fact.name,
                );
            }
            if let Some(evidence) = &fact.operational {
                require_all(
                    &bundles,
                    evidence,
                    capability_for_filter(&filter),
                    &fact.name,
                );
            }
        }
        for fact in &snapshot.capabilities.renderer_values {
            require_all(
                &bundles,
                &fact.evidence,
                Capability::RendererValue(renderer_option(&fact.option), &fact.value),
                &fact.option,
            );
        }
        for fact in &snapshot.capabilities.live_inputs {
            let capability = Capability::LiveInput {
                format: capture_format(&fact.format),
                player: player(&fact.player),
            };
            for evidence in fact.listed.iter().chain(fact.operational.iter()) {
                require_all(&bundles, evidence, capability, &fact.format);
            }
        }
        let audio = &snapshot.capabilities.audio;
        require_all(
            &bundles,
            audio.source_capture.operational.as_ref().unwrap(),
            Capability::PulseSourceCapture,
            "audio.source_capture",
        );
        require_all(
            &bundles,
            audio.sink_monitor.operational.as_ref().unwrap(),
            Capability::PulseSinkMonitor,
            "audio.sink_monitor",
        );
        // The audible relay needs the recorded identities to be checked; they
        // live only in the test support, never in the report projection.
        require_all(
            &bundles,
            audio.source_to_sink_playback.operational.as_ref().unwrap(),
            Capability::PulseSourcePlayback {
                source: evidence::CAPTURE_SOURCE,
                sink: evidence::EFFECTIVE_SINK,
            },
            "audio.source_to_sink_playback",
        );
    }

    /// The reference digests must equal the exact tracked fixture index, so
    /// swapping in any other valid-looking SHA-256 fails here.
    #[test]
    fn reference_matches_the_tracked_fixture_index() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/FUR-002");
        let index: serde_json::Value = serde_json::from_slice(
            &std::fs::read(root.join("index.json")).expect("fixture index is readable"),
        )
        .expect("fixture index is valid JSON");
        let snapshot = reference();
        let reference = &snapshot.reference;
        let indexed = &index["bundles"];
        let common = &indexed["common"];
        assert_eq!(
            reference.artifacts.mpv,
            common["mpv_sha256"].as_str().unwrap()
        );
        assert_eq!(
            reference.artifacts.ffmpeg,
            common["ffmpeg_sha256"].as_str().unwrap()
        );
        assert_eq!(
            reference.artifacts.libmpv,
            common["libmpv_sha256"].as_str().unwrap()
        );
        assert_eq!(
            reference.stack_identity_sha256,
            common["stack_sha256"].as_str().unwrap()
        );
        assert_eq!(reference.bundles.len(), indexed.as_object().unwrap().len());
        for bundle in &reference.bundles {
            let entry = &indexed[bundle.name.as_str()];
            assert_eq!(
                bundle.source_bundle,
                entry["source_bundle"].as_str().unwrap()
            );
            assert_eq!(
                bundle.record_sha256,
                entry["record_sha256"].as_str().unwrap()
            );
            assert_eq!(
                bundle.source_manifest_sha256,
                entry["source_manifest_sha256"].as_str().unwrap()
            );
            // Every tracked bundle shares the reference stack identity.
            assert_eq!(entry["stack_sha256"], common["stack_sha256"]);
            assert_eq!(entry["mpv_sha256"], common["mpv_sha256"]);
            assert_eq!(entry["ffmpeg_sha256"], common["ffmpeg_sha256"]);
        }
    }

    /// An observed reference version must be cited by a recorded transcript that
    /// actually contains it.
    #[test]
    fn observed_reference_versions_cite_real_transcript_evidence() {
        let snapshot = reference();
        let mut collections = std::collections::BTreeMap::new();
        for name in ["common", "effective"] {
            collections.insert(name.to_owned(), evidence::capabilities(name));
        }
        for (name, needle) in [
            ("libplacebo", "libplacebo v7.360.1"),
            ("libplacebo_api", "(API v360)"),
        ] {
            let facts = match snapshot.reference.versions.get(name) {
                Some(Availability::Observed(facts)) => facts,
                other => panic!("{name} must be observed, got {other:?}"),
            };
            assert!(!facts.evidence.bundles.is_empty());
            for bundle in &facts.evidence.bundles {
                let probes = collections.get(bundle).expect("cited bundle is tracked");
                for probe in &facts.evidence.probes {
                    let run = probes.commands.get(probe).expect("cited probe is recorded");
                    let stdout = String::from_utf8_lossy(&run.stdout);
                    assert!(
                        stdout.contains(needle),
                        "{name} cited {bundle}/{probe} without {needle:?}"
                    );
                    assert!(stdout.contains(&facts.value));
                }
            }
        }
    }

    /// The parser must refuse a capability the cited bundle does not prove, so a
    /// mis-cited evidence entry cannot silently pass.
    #[test]
    fn parser_refuses_uncited_capabilities() {
        let common = evidence::capabilities("common");
        let caps = Capabilities::from_probes(&common).expect("common parses");
        // The common bundle's relay was silent/intercepted: the playback
        // capability is only cited by the audible effective bundle.
        assert!(
            caps.require(
                evidence::STACK_SHA256,
                Capability::PulseSourcePlayback {
                    source: evidence::CAPTURE_SOURCE,
                    sink: evidence::EFFECTIVE_SINK,
                },
            )
            .is_err(),
            "common must not prove the audible relay"
        );
        // A bare high-res bundle records no listings, so an open alone is not
        // enough: the identity-checked merge is what makes its evidence usable.
        let highres = evidence::capabilities("highres");
        let highres_caps = Capabilities::from_probes(&highres).expect("highres parses");
        assert!(
            highres_caps
                .require(
                    evidence::STACK_SHA256,
                    Capability::LiveInput {
                        format: CaptureFormat::Nv12,
                        player: Player::Ffmpeg,
                    },
                )
                .is_err(),
            "an unmerged high-res bundle must not prove a live input"
        );
    }

    #[test]
    fn reference_identity_is_bounded_and_evidence_backed() {
        let snapshot = reference();
        assert_eq!(snapshot.schema, 1);
        assert_eq!(snapshot.kind, "furami-qualified-media-reference");
        assert!(hex64(&snapshot.reference.stack_identity_sha256));
        for digest in [
            &snapshot.reference.artifacts.mpv,
            &snapshot.reference.artifacts.ffmpeg,
            &snapshot.reference.artifacts.libmpv,
        ] {
            assert!(hex64(digest), "reference artifact digest is not SHA-256");
        }
        assert_eq!(snapshot.reference.bundles.len(), 3);
        let mut names: Vec<&str> = snapshot
            .reference
            .bundles
            .iter()
            .map(|bundle| bundle.name.as_str())
            .collect();
        names.sort_unstable();
        assert_eq!(names, ["common", "effective", "highres"]);
        for bundle in &snapshot.reference.bundles {
            assert!(hex64(&bundle.record_sha256));
            assert!(hex64(&bundle.source_manifest_sha256));
        }
        // Versions observed only from the render transcript are the only
        // reference media versions the tracked fixtures can prove.
        for (name, value) in [("libplacebo", "v7.360.1"), ("libplacebo_api", "v360")] {
            assert!(
                matches!(
                    snapshot.reference.versions.get(name),
                    Some(Availability::Observed(VersionFacts { value: observed, .. }))
                        if observed == value
                ),
                "{name} must be observed as {value}"
            );
        }
        for unproven in ["mpv", "ffmpeg", "libmpv_client_api"] {
            assert!(
                matches!(
                    snapshot.reference.versions.get(unproven),
                    Some(Availability::Unavailable {
                        reason: InventoryUnavailable::NotRecordedInFixture
                    })
                ),
                "{unproven} must stay an explicit unavailability"
            );
        }
    }

    #[test]
    fn binding_scope_is_exactly_the_three_media_artifacts() {
        let inventory = build_inventory();
        assert_eq!(inventory.binding_scope, BINDING_SCOPE.map(str::to_owned));
    }

    #[test]
    fn reference_binding_requires_a_complete_file_comparison() {
        let reference = ReferenceArtifacts {
            mpv: "a".repeat(64),
            ffmpeg: "b".repeat(64),
            libmpv: "c".repeat(64),
        };
        let artifact = |path: &str, sha256: String| ProviderArtifact {
            path: path.to_owned(),
            sha256,
        };
        let complete = |mpv: String, ffmpeg: String, libmpv: String| {
            let mut artifacts = BTreeMap::new();
            artifacts.insert("mpv".to_owned(), artifact("bin/mpv", mpv));
            artifacts.insert("ffmpeg".to_owned(), artifact("bin/ffmpeg", ffmpeg));
            artifacts.insert("libmpv".to_owned(), artifact("lib/libmpv.so.2.5.0", libmpv));
            Availability::Observed(BuildProviderFacts {
                lock_sha256: "d".repeat(64),
                container: None,
                apt_snapshot: None,
                sources: BTreeMap::new(),
                artifacts,
            })
        };

        assert_eq!(
            reference_binding(
                &reference,
                &Availability::Unavailable {
                    reason: InventoryUnavailable::ManifestAbsent
                }
            ),
            ReferenceBinding::Unavailable {
                reason: InventoryUnavailable::ProviderUnavailable
            }
        );
        assert_eq!(
            reference_binding(
                &reference,
                &complete(
                    reference.mpv.clone(),
                    reference.ffmpeg.clone(),
                    reference.libmpv.clone()
                )
            ),
            ReferenceBinding::MatchingQualifiedMediaArtifacts
        );
        assert_eq!(
            reference_binding(
                &reference,
                &complete(
                    reference.mpv.clone(),
                    reference.ffmpeg.clone(),
                    "c".repeat(63) + "d"
                )
            ),
            ReferenceBinding::DifferentMediaArtifacts
        );

        let mut partial = match complete(
            reference.mpv.clone(),
            reference.ffmpeg.clone(),
            reference.libmpv.clone(),
        ) {
            Availability::Observed(facts) => facts,
            Availability::Unavailable { .. } => unreachable!(),
        };
        partial.artifacts.remove("libmpv");
        assert_eq!(
            reference_binding(&reference, &Availability::Observed(partial)),
            ReferenceBinding::Unavailable {
                reason: InventoryUnavailable::MediaArtifactsMissing
            }
        );
    }

    /// The validators must refuse the actual recorded personal audio identity and
    /// other private-looking text, while accepting every pinned value. These are
    /// the same functions `build.rs` enforces.
    #[test]
    fn provenance_validators_refuse_personal_text() {
        assert!(!is_version(evidence::CAPTURE_SOURCE));
        assert!(!is_version("alsa_input_private1"));
        assert!(!is_kit("alsa_input_private1"));
        assert!(!is_source_name("alsa_input_private"));
        assert!(!is_container("/home/private-user"));
        assert!(!is_container("anthodev/furami"));
        assert!(!is_dotted_digits("/home/1.2"));
        assert!(!is_platform("linux"));
        assert!(!is_snapshot("alsa_input_private1"));

        for version in [
            "v0.41.0",
            "8.1.2",
            "v7.360.1",
            "libplacebo-v7.360.1-gitlink",
            "20260601~24.04.1",
            "6.11.2",
            "0.10.0",
        ] {
            assert!(is_version(version), "pinned version {version} must pass");
        }
        assert!(is_kit("linux_gcc_64"));
        assert!(is_dotted_digits("2.5.0"));
        assert!(is_snapshot("20260920T000000Z"));
        assert!(is_platform("linux/amd64"));
        assert!(is_container(
            "docker.io/library/ubuntu@sha256:008173c23f95b170204355c12626cb5a965d779a7e1283b09e9cffbb1bf33ca3"
        ));
        for name in KNOWN_SOURCES {
            assert!(is_source_name(name));
        }
        assert!(is_source_scalar("sha256", &"a".repeat(64)));
        let short = "a".repeat(63);
        for rejected in ["", "onsize", short.as_str()] {
            assert!(!is_source_scalar("sha256", rejected));
        }
    }

    /// Allowed source scalars stay inside their fixed domain; a home path, a
    /// personal audio identity or any unvalidated text can never survive.
    fn assert_bounded_sources(sources: &BTreeMap<String, SourceVersion>) {
        for (name, source) in sources {
            assert!(is_source_name(name), "unknown source key {name:?}");
            if let Some(version) = &source.version {
                assert!(is_version(version), "not a version token: {version:?}");
            }
            if let Some(commit) = &source.commit {
                assert!(hex_range(commit, 7, 40), "not a git hex id: {commit:?}");
            }
            if let Some(digest) = &source.sha256 {
                assert!(hex_range(digest, 64, 64), "source sha256 must be SHA-256");
            }
            if let Some(client_api) = &source.client_api {
                assert!(
                    is_dotted_digits(client_api),
                    "client_api must be dotted digits: {client_api:?}"
                );
            }
            if let Some(kit) = &source.kit {
                assert!(is_kit(kit), "kit must be a Linux platform token: {kit:?}");
            }
        }
    }

    /// The embedded projections keep only allowed scalars: file digests for the
    /// three media artifacts, never symlink target hashes, and no machine path.
    #[test]
    fn provider_and_recipe_projections_are_bounded() {
        let provider: Availability<BuildProviderFacts> =
            serde_json::from_str(MEDIA_PROVIDER_JSON).expect("provider projection parses");
        if let Availability::Observed(facts) = &provider {
            assert!(hex64(&facts.lock_sha256));
            assert_bounded_sources(&facts.sources);
            for (logical, artifact) in &facts.artifacts {
                assert!(
                    BINDING_SCOPE.contains(&logical.as_str()),
                    "unexpected provider artifact {logical}"
                );
                assert!(hex64(&artifact.sha256));
                assert!(
                    !artifact.path.starts_with('/') && !artifact.path.contains(".."),
                    "artifact path must stay relative: {}",
                    artifact.path
                );
                assert!(
                    !artifact.path.ends_with("/libmpv.so")
                        && !artifact.path.ends_with("/libmpv.so.2"),
                    "a symlink path must never be recorded: {}",
                    artifact.path
                );
            }
        }

        let recipe: Availability<SourceRecipeFacts> =
            serde_json::from_str(SOURCE_RECIPE_JSON).expect("recipe projection parses");
        if let Availability::Observed(facts) = &recipe {
            assert!(hex64(&facts.lock_sha256));
            assert!(facts.sources.contains_key("mpv"));
            let mut keys: Vec<&str> = facts.sources.keys().map(String::as_str).collect();
            keys.sort_unstable();
            let mut expected = KNOWN_SOURCES.to_vec();
            expected.sort_unstable();
            assert_eq!(keys, expected, "recipe source set must be exactly known");
            assert_bounded_sources(&facts.sources);
        }

        let inventory = build_inventory();
        let rendered = serde_json::to_string(&inventory).expect("inventory serializes");
        for forbidden in [
            "/home/",
            "/dev/video",
            "alsa_",
            "XDG_",
            "DISPLAY",
            "WAYLAND",
            "argv",
            "serial",
        ] {
            assert!(
                !rendered.contains(forbidden),
                "rendered inventory leaked {forbidden}"
            );
        }
    }

    #[test]
    fn embedded_binding_agrees_with_the_projected_provider() {
        let inventory = build_inventory();
        let expected = reference_binding(
            &inventory.reference_snapshot.reference.artifacts,
            &inventory.build_provider,
        );
        assert_eq!(inventory.reference_binding, expected);
    }
}
