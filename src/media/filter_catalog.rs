//! Qualified-build filter registration and typed media-only syntax construction.
//!
//! This on-demand query opens no media input and does not attest an existing live
//! libmpv handle. Artifact digests and dependency paths remain the FUR-002 frozen
//! prefix's qualification precondition. Registration is not playback proof.

use crate::domain::filters::{
    BwdifMode, ColorLevels, DeinterlaceSelection, FieldParity, Filter, FilterChain, FilterKind,
    SdrGamma, SdrMatrix,
};
use std::fmt::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use thiserror::Error;

const QUALIFIED_MPV_VERSION: &str = "v0.41.0";
const QUALIFIED_FFMPEG_VERSION: &str = "8.1.2";
const NATIVE_HEADER: &str = "Available video filters:";
const LINKED_HEADER: &str = "Available libavfilter filters:";
const REGISTRY_FOOTER: [&str; 3] = [
    "If libavfilter filters clash with builtin mpv filters,",
    "prefix them with lavfi- to select the libavfilter one.",
    "Get help on individual entries via: --vf=entry=help",
];

/// The two fixed, input-free queries used to observe the frozen build.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CatalogQuery {
    Version,
    VideoFilters,
}

/// The origin-specific registration needed by a product filter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FilterRequirement {
    NativeFilter,
    LavfiBridge,
    LinkedFfmpegFilter,
}

/// Query failures are not evidence that a filter is absent.
#[derive(Debug, Error)]
pub enum FilterCatalogError {
    #[error("invalid qualified media prefix: {detail}")]
    InvalidPrefix { detail: String },
    #[error("{query:?} query could not execute: {source}")]
    QueryExecution {
        query: CatalogQuery,
        #[source]
        source: std::io::Error,
    },
    #[error("{query:?} query failed with status {status:?}: {stderr}")]
    QueryFailed {
        query: CatalogQuery,
        status: Option<i32>,
        stderr: String,
    },
    #[error("{query:?} query output is not UTF-8")]
    InvalidQueryEncoding { query: CatalogQuery },
    #[error("malformed {query:?} query output: {reason}")]
    MalformedQuery {
        query: CatalogQuery,
        reason: &'static str,
    },
    #[error("qualified {component} version mismatch: expected {expected}, got {actual}")]
    QualificationMismatch {
        component: &'static str,
        expected: &'static str,
        actual: String,
    },
    #[error("{filter:?} is missing required {requirement:?} registration")]
    MissingCapability {
        filter: FilterKind,
        requirement: FilterRequirement,
    },
}

#[derive(Debug, Default)]
struct RegisteredFilters {
    native_format: bool,
    native_lavfi: bool,
    linked_eq: bool,
    linked_unsharp: bool,
    linked_hqdn3d: bool,
    linked_bwdif: bool,
}

/// Immutable observed build facts. Only a genuine successful query creates
/// this value publicly; it cannot be deserialized or populated by a caller.
///
/// The CLI and its linked registry are queried, not the application's live
/// libmpv handle. Matching versions alone does not establish artifact identity.
#[derive(Debug)]
pub struct FilterCapabilities {
    prefix: PathBuf,
    executable: PathBuf,
    mpv_version: String,
    ffmpeg_version: String,
    registered: RegisteredFilters,
}

impl FilterCapabilities {
    /// Whether all origin-specific prerequisites for this kind were observed.
    pub fn supports(&self, kind: FilterKind) -> bool {
        require_filter(self, kind).is_ok()
    }

    /// Canonical absolute prefix used by the successful query.
    pub fn prefix(&self) -> &Path {
        &self.prefix
    }

    /// Canonical executable confined to the queried prefix.
    pub fn executable(&self) -> &Path {
        &self.executable
    }

    /// Exact qualified mpv version token, including its `v` prefix.
    pub fn mpv_version(&self) -> &str {
        &self.mpv_version
    }

    /// Exact FFmpeg version token reported by this mpv executable.
    pub fn ffmpeg_version(&self) -> &str {
        &self.ffmpeg_version
    }
}

/// Media-owned syntax produced exclusively from validated domain parameters.
///
/// There is no raw constructor, string conversion or deserializer:
///
/// ```compile_fail
/// use furami::media::filter_catalog::CompiledFilterChain;
/// let compiled: CompiledFilterChain = "lavfi=[movie=/tmp/input]".into();
/// ```
///
/// ```compile_fail
/// use furami::media::filter_catalog::CompiledFilterChain;
/// let compiled = serde_json::from_str::<CompiledFilterChain>("\"raw graph\"");
/// ```
#[derive(Clone, PartialEq)]
pub struct CompiledFilterChain {
    // Siblings in media may consume this ownership representation for FUR-015.
    // No syntax accessor is public to domain/UI/external callers.
    pub(super) vf: String,
    emitted_entries: usize,
    pub(super) entries: Vec<crate::domain::failure::FilterEntryMetadata>,
}

impl CompiledFilterChain {
    /// Number of emitted entries, excluding disabled domain entries.
    pub fn len(&self) -> usize {
        self.emitted_entries
    }

    /// Whether this value represents an empty mpv filter chain.
    pub fn is_empty(&self) -> bool {
        self.vf.is_empty()
    }
}

impl fmt::Debug for CompiledFilterChain {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CompiledFilterChain")
            .field("emitted_entries", &self.emitted_entries)
            .finish_non_exhaustive()
    }
}

/// Query the qualified CLI on demand, with no config, PATH lookup or media input.
///
/// Canonicalizes an absolute existing prefix and rejects `bin/mpv` symlinks
/// escaping that prefix. The child environment is cleared, then receives only
/// `LD_LIBRARY_PATH=prefix/lib` and `LC_ALL=C`. This checks the pinned mpv
/// v0.41.0 / FFmpeg 8.1.2 build's native and linked registrations.
pub fn query_qualified_capabilities(
    prefix: &Path,
) -> Result<FilterCapabilities, FilterCatalogError> {
    if !prefix.is_absolute() || !prefix.is_dir() {
        return Err(FilterCatalogError::InvalidPrefix {
            detail: "prefix must be an absolute existing directory".to_owned(),
        });
    }
    let prefix = prefix
        .canonicalize()
        .map_err(|error| FilterCatalogError::InvalidPrefix {
            detail: error.to_string(),
        })?;
    let executable = prefix.join("bin/mpv").canonicalize().map_err(|error| {
        FilterCatalogError::InvalidPrefix {
            detail: format!("bin/mpv: {error}"),
        }
    })?;
    if !executable.starts_with(&prefix) || !executable.is_file() {
        return Err(FilterCatalogError::InvalidPrefix {
            detail: "bin/mpv must resolve to a file within the qualified prefix".to_owned(),
        });
    }
    let version = run_query(&prefix, &executable, CatalogQuery::Version)?;
    let (mpv_version, ffmpeg_version) = parse_versions(&version)?;
    let registry = run_query(&prefix, &executable, CatalogQuery::VideoFilters)?;
    let registered = parse_registry(&registry)?;
    Ok(FilterCapabilities {
        prefix,
        executable,
        mpv_version,
        ffmpeg_version,
        registered,
    })
}

fn qualified_command(prefix: &Path, executable: &Path, query: CatalogQuery) -> Command {
    let mut command = Command::new(executable);
    command
        .arg("--no-config")
        .arg(match query {
            CatalogQuery::Version => "--version",
            CatalogQuery::VideoFilters => "--vf=help",
        })
        .env_clear()
        .env("LD_LIBRARY_PATH", prefix.join("lib"))
        .env("LC_ALL", "C");
    command
}

fn run_query(
    prefix: &Path,
    executable: &Path,
    query: CatalogQuery,
) -> Result<String, FilterCatalogError> {
    let output = qualified_command(prefix, executable, query)
        .output()
        .map_err(|source| FilterCatalogError::QueryExecution { query, source })?;
    query_text(query, output)
}

fn query_text(query: CatalogQuery, output: Output) -> Result<String, FilterCatalogError> {
    if !output.status.success() {
        return Err(FilterCatalogError::QueryFailed {
            query,
            status: output.status.code(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }
    String::from_utf8(output.stdout).map_err(|_| FilterCatalogError::InvalidQueryEncoding { query })
}

fn malformed(query: CatalogQuery, reason: &'static str) -> FilterCatalogError {
    FilterCatalogError::MalformedQuery { query, reason }
}

fn parse_versions(text: &str) -> Result<(String, String), FilterCatalogError> {
    let mut mpv = None;
    let mut ffmpeg = None;
    for line in text.lines() {
        if line == "mpv" || line.starts_with("mpv ") {
            if mpv.is_some() {
                return Err(malformed(CatalogQuery::Version, "duplicate mpv version"));
            }
            mpv = line.split_ascii_whitespace().nth(1);
            if mpv.is_none() {
                return Err(malformed(
                    CatalogQuery::Version,
                    "missing mpv version token",
                ));
            }
        } else if let Some(version) = line.strip_prefix("FFmpeg version:") {
            if ffmpeg.is_some() {
                return Err(malformed(CatalogQuery::Version, "duplicate FFmpeg version"));
            }
            let mut tokens = version.split_ascii_whitespace();
            ffmpeg = tokens.next();
            if ffmpeg.is_none() || tokens.next().is_some() {
                return Err(malformed(
                    CatalogQuery::Version,
                    "invalid FFmpeg version token",
                ));
            }
        }
    }
    let mpv = mpv.ok_or_else(|| malformed(CatalogQuery::Version, "missing mpv version"))?;
    let ffmpeg =
        ffmpeg.ok_or_else(|| malformed(CatalogQuery::Version, "missing FFmpeg version"))?;
    for (component, expected, actual) in [
        ("mpv", QUALIFIED_MPV_VERSION, mpv),
        ("FFmpeg", QUALIFIED_FFMPEG_VERSION, ffmpeg),
    ] {
        if actual != expected {
            return Err(FilterCatalogError::QualificationMismatch {
                component,
                expected,
                actual: actual.to_owned(),
            });
        }
    }
    Ok((mpv.to_owned(), ffmpeg.to_owned()))
}

fn parse_registry(text: &str) -> Result<RegisteredFilters, FilterCatalogError> {
    // 0 = before native header, 1 = native, 2 = linked, 3 = fixed footer.
    // Required headers occur once, in order; arbitrary text is never a fallback.
    let mut section = 0;
    let mut footer_line = 0;
    let mut registered = RegisteredFilters::default();
    for line in text.lines() {
        if line.is_empty() {
            continue;
        }
        if line == NATIVE_HEADER {
            if section != 0 {
                return Err(malformed(
                    CatalogQuery::VideoFilters,
                    "duplicate or misplaced native header",
                ));
            }
            section = 1;
            continue;
        }
        if line == LINKED_HEADER {
            if section != 1 {
                return Err(malformed(
                    CatalogQuery::VideoFilters,
                    "duplicate or misplaced linked header",
                ));
            }
            section = 2;
            continue;
        }
        if (section == 2 || section == 3)
            && footer_line < REGISTRY_FOOTER.len()
            && line == REGISTRY_FOOTER[footer_line]
        {
            section = 3;
            footer_line += 1;
            continue;
        }
        if !(section == 1 || section == 2) || !line.starts_with("  ") {
            return Err(malformed(
                CatalogQuery::VideoFilters,
                "unexpected registry line",
            ));
        }
        let row = &line[2..];
        let (name, description) = row.split_once(char::is_whitespace).ok_or_else(|| {
            malformed(CatalogQuery::VideoFilters, "registry row lacks description")
        })?;
        if name.is_empty()
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
            || description.trim().is_empty()
        {
            return Err(malformed(
                CatalogQuery::VideoFilters,
                "malformed registry row",
            ));
        }
        let observed = match (section, name) {
            (1, "format") => Some(&mut registered.native_format),
            (1, "lavfi") => Some(&mut registered.native_lavfi),
            (2, "eq") => Some(&mut registered.linked_eq),
            (2, "unsharp") => Some(&mut registered.linked_unsharp),
            (2, "hqdn3d") => Some(&mut registered.linked_hqdn3d),
            (2, "bwdif") => Some(&mut registered.linked_bwdif),
            _ => None,
        };
        if let Some(observed) = observed {
            if *observed {
                return Err(malformed(
                    CatalogQuery::VideoFilters,
                    "duplicate catalog registration",
                ));
            }
            *observed = true;
        }
    }
    if section < 2 {
        return Err(malformed(
            CatalogQuery::VideoFilters,
            "missing required registry section",
        ));
    }
    if section == 3 && footer_line != REGISTRY_FOOTER.len() {
        return Err(malformed(
            CatalogQuery::VideoFilters,
            "incomplete registry footer",
        ));
    }
    Ok(registered)
}

fn require_filter(
    capabilities: &FilterCapabilities,
    kind: FilterKind,
) -> Result<(), FilterCatalogError> {
    let registered = &capabilities.registered;
    let missing = if kind == FilterKind::Format {
        (!registered.native_format).then_some(FilterRequirement::NativeFilter)
    } else if !registered.native_lavfi {
        Some(FilterRequirement::LavfiBridge)
    } else {
        let linked = match kind {
            FilterKind::Eq => registered.linked_eq,
            FilterKind::Unsharp => registered.linked_unsharp,
            FilterKind::Hqdn3d => registered.linked_hqdn3d,
            FilterKind::Bwdif => registered.linked_bwdif,
            FilterKind::Format => unreachable!("native format handled above"),
        };
        (!linked).then_some(FilterRequirement::LinkedFfmpegFilter)
    };
    match missing {
        Some(requirement) => Err(FilterCatalogError::MissingCapability {
            filter: kind,
            requirement,
        }),
        None => Ok(()),
    }
}

/// Require the entire closed catalog, independently of any particular chain.
pub fn check_catalog(capabilities: &FilterCapabilities) -> Result<(), FilterCatalogError> {
    for kind in FilterKind::ALL {
        require_filter(capabilities, kind)?;
    }
    Ok(())
}

/// Check every entry, including disabled entries, then emit enabled entries in
/// their original order. Display labels never enter syntax. An empty chain
/// requires no registrations and compiles to empty syntax.
///
/// Free text is not accepted at this boundary:
///
/// ```compile_fail
/// use furami::media::filter_catalog::{compile_chain, FilterCapabilities};
/// fn inject(capabilities: &FilterCapabilities) {
///     compile_chain("lavfi=[movie=/tmp/input]", capabilities);
/// }
/// ```
pub fn compile_chain(
    chain: &FilterChain,
    capabilities: &FilterCapabilities,
) -> Result<CompiledFilterChain, FilterCatalogError> {
    for entry in chain.entries() {
        require_filter(capabilities, entry.kind())?;
    }
    let mut vf = String::new();
    let mut emitted_entries = 0;
    for (ordinal, entry) in chain.entries().iter().enumerate() {
        if !entry.enabled() {
            continue;
        }
        if emitted_entries != 0 {
            vf.push(',');
        }
        write!(vf, "@furami_{ordinal}:").expect("writing into a String cannot fail");
        append_filter(&mut vf, entry.filter()).expect("writing into a String cannot fail");
        emitted_entries += 1;
    }
    Ok(CompiledFilterChain {
        vf,
        emitted_entries,
        entries: chain
            .entries()
            .iter()
            .enumerate()
            .map(
                |(ordinal, entry)| crate::domain::failure::FilterEntryMetadata {
                    ordinal,
                    label: entry.label().to_owned(),
                    enabled: entry.enabled(),
                },
            )
            .collect(),
    })
}

fn append_filter(vf: &mut String, filter: &Filter) -> fmt::Result {
    match filter {
        Filter::Format(params) => write!(
            vf,
            "format=colormatrix={}:colorlevels={}:gamma={}:convert=no",
            match params.matrix() {
                SdrMatrix::Auto => "auto",
                SdrMatrix::Bt601 => "bt.601",
                SdrMatrix::Bt709 => "bt.709",
                SdrMatrix::Smpte240m => "smpte-240m",
                SdrMatrix::Rgb => "rgb",
            },
            match params.levels() {
                ColorLevels::Auto => "auto",
                ColorLevels::Limited => "limited",
                ColorLevels::Full => "full",
            },
            match params.gamma() {
                SdrGamma::Auto => "auto",
                SdrGamma::Bt1886 => "bt.1886",
                SdrGamma::Srgb => "srgb",
                SdrGamma::Linear => "linear",
                SdrGamma::Gamma18 => "gamma1.8",
                SdrGamma::Gamma20 => "gamma2.0",
                SdrGamma::Gamma22 => "gamma2.2",
                SdrGamma::Gamma24 => "gamma2.4",
                SdrGamma::Gamma26 => "gamma2.6",
                SdrGamma::Gamma28 => "gamma2.8",
            },
        ),
        Filter::Eq(params) => write!(
            vf,
            "lavfi=[eq=contrast={}:brightness={}:saturation={}:gamma={}:gamma_r={}:gamma_g={}:gamma_b={}:gamma_weight={}:eval=init]",
            params.contrast(),
            params.brightness(),
            params.saturation(),
            params.gamma(),
            params.gamma_r(),
            params.gamma_g(),
            params.gamma_b(),
            params.gamma_weight(),
        ),
        Filter::Unsharp(params) => write!(
            vf,
            "lavfi=[unsharp=luma_msize_x={}:luma_msize_y={}:luma_amount={}:chroma_msize_x={}:chroma_msize_y={}:chroma_amount={}:alpha_msize_x={}:alpha_msize_y={}:alpha_amount={}]",
            params.luma_msize_x(),
            params.luma_msize_y(),
            params.luma_amount(),
            params.chroma_msize_x(),
            params.chroma_msize_y(),
            params.chroma_amount(),
            params.alpha_msize_x(),
            params.alpha_msize_y(),
            params.alpha_amount(),
        ),
        Filter::Hqdn3d(params) => write!(
            vf,
            "lavfi=[hqdn3d=luma_spatial={}:chroma_spatial={}:luma_tmp={}:chroma_tmp={}]",
            params.luma_spatial(),
            params.chroma_spatial(),
            params.luma_tmp(),
            params.chroma_tmp(),
        ),
        Filter::Bwdif(params) => write!(
            vf,
            "lavfi=[bwdif=mode={}:parity={}:deint={}]",
            match params.mode() {
                BwdifMode::SendFrame => "send_frame",
                BwdifMode::SendField => "send_field",
            },
            match params.parity() {
                FieldParity::Auto => "auto",
                FieldParity::TopFieldFirst => "tff",
                FieldParity::BottomFieldFirst => "bff",
            },
            match params.deint() {
                DeinterlaceSelection::All => "all",
                DeinterlaceSelection::Interlaced => "interlaced",
            },
        ),
    }
}

#[cfg(test)]
pub(crate) fn fixture_capabilities() -> FilterCapabilities {
    tests::capabilities(tests::REGISTRY)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::filters::{
        BwdifParams, EqParams, EqValues, FilterEntry, FormatParams, Hqdn3dParams, Hqdn3dValues,
        UnsharpParams, UnsharpValues,
    };
    use std::os::unix::process::ExitStatusExt;
    use std::process::{ExitStatus, Output};

    pub(super) const REGISTRY: &str = "Available video filters:\n  format           force output format\n  lavfi            libavfilter bridge\n  lavfi-bridge     explicit bridge\n\nAvailable libavfilter filters:\n  bwdif            Deinterlace the input image.\n  eq               Adjust brightness.\n  format           Convert pixel formats.\n  hqdn3d           Denoise.\n  unsharp          Sharpen.\n\nIf libavfilter filters clash with builtin mpv filters,\nprefix them with lavfi- to select the libavfilter one.\n\nGet help on individual entries via: --vf=entry=help\n";
    const VERSION: &str = "mpv v0.41.0 Copyright © 2000-2025 mpv/MPlayer/mplayer2 projects\nlibplacebo version: v7.360.1\nFFmpeg version: 8.1.2\nFFmpeg library versions:\n   libavfilter     11.14.102\n";

    pub(super) fn capabilities(registry: &str) -> FilterCapabilities {
        let (mpv_version, ffmpeg_version) = parse_versions(VERSION).unwrap();
        FilterCapabilities {
            prefix: PathBuf::from("/qualified/prefix"),
            executable: PathBuf::from("/qualified/prefix/bin/mpv"),
            mpv_version,
            ffmpeg_version,
            registered: parse_registry(registry).unwrap(),
        }
    }

    fn filters() -> [Filter; 5] {
        [
            Filter::Format(FormatParams::new(
                SdrMatrix::Auto,
                ColorLevels::Auto,
                SdrGamma::Auto,
            )),
            Filter::Eq(
                EqParams::new(EqValues {
                    contrast: 1.0,
                    brightness: 0.0,
                    saturation: 1.0,
                    gamma: 1.0,
                    gamma_r: 1.0,
                    gamma_g: 1.0,
                    gamma_b: 1.0,
                    gamma_weight: 1.0,
                })
                .unwrap(),
            ),
            Filter::Unsharp(
                UnsharpParams::new(UnsharpValues {
                    luma_msize_x: 5,
                    luma_msize_y: 5,
                    luma_amount: 1.0,
                    chroma_msize_x: 5,
                    chroma_msize_y: 5,
                    chroma_amount: 0.0,
                    alpha_msize_x: 5,
                    alpha_msize_y: 5,
                    alpha_amount: 0.0,
                })
                .unwrap(),
            ),
            Filter::Hqdn3d(
                Hqdn3dParams::new(Hqdn3dValues {
                    luma_spatial: 0.0,
                    chroma_spatial: 0.0,
                    luma_tmp: 0.0,
                    chroma_tmp: 0.0,
                })
                .unwrap(),
            ),
            Filter::Bwdif(BwdifParams::new(
                BwdifMode::SendField,
                FieldParity::Auto,
                DeinterlaceSelection::All,
            )),
        ]
    }

    fn chain(entries: Vec<(Filter, bool)>) -> FilterChain {
        FilterChain::new(
            entries
                .into_iter()
                .enumerate()
                .map(|(index, (filter, enabled))| {
                    FilterEntry::new(format!("display {index}"), filter, enabled)
                })
                .collect(),
        )
        .unwrap()
    }

    // Golden grammar from mpv v0.41.0 vf.rst and the pinned n8.1.2
    // canonical AVOptions. These assertions do not apply a graph to a stream.
    const GOLDENS: [&str; 5] = [
        "@furami_0:format=colormatrix=auto:colorlevels=auto:gamma=auto:convert=no",
        "@furami_0:lavfi=[eq=contrast=1:brightness=0:saturation=1:gamma=1:gamma_r=1:gamma_g=1:gamma_b=1:gamma_weight=1:eval=init]",
        "@furami_0:lavfi=[unsharp=luma_msize_x=5:luma_msize_y=5:luma_amount=1:chroma_msize_x=5:chroma_msize_y=5:chroma_amount=0:alpha_msize_x=5:alpha_msize_y=5:alpha_amount=0]",
        "@furami_0:lavfi=[hqdn3d=luma_spatial=0:chroma_spatial=0:luma_tmp=0:chroma_tmp=0]",
        "@furami_0:lavfi=[bwdif=mode=send_field:parity=auto:deint=all]",
    ];

    #[test]
    fn each_filter_has_its_native_or_individual_lavfi_golden() {
        let capabilities = capabilities(REGISTRY);
        for (filter, expected) in filters().into_iter().zip(GOLDENS) {
            let compiled = compile_chain(&chain(vec![(filter, true)]), &capabilities).unwrap();
            assert_eq!(compiled.vf, expected);
            assert_eq!(compiled.len(), 1);
            assert!(!compiled.is_empty());
        }
    }

    #[test]
    fn all_filters_preserve_order_and_individual_envelopes() {
        let compiled = compile_chain(
            &chain(filters().into_iter().map(|filter| (filter, true)).collect()),
            &capabilities(REGISTRY),
        )
        .unwrap();
        let expected = GOLDENS
            .iter()
            .enumerate()
            .map(|(index, golden)| golden.replacen("@furami_0:", &format!("@furami_{index}:"), 1))
            .collect::<Vec<_>>()
            .join(",");
        assert_eq!(compiled.vf, expected);
        assert_eq!(compiled.len(), 5);
    }

    #[test]
    fn disabled_entries_keep_original_ordinals_and_repeated_kinds_keep_order() {
        let [format, eq, _, _, _] = filters();
        let compiled = compile_chain(
            &chain(vec![(eq.clone(), true), (format, false), (eq, true)]),
            &capabilities(REGISTRY),
        )
        .unwrap();
        assert_eq!(
            compiled.vf,
            format!(
                "{},{}",
                GOLDENS[1],
                GOLDENS[1].replacen("@furami_0:", "@furami_2:", 1)
            )
        );
        assert_eq!(compiled.len(), 2);
    }

    #[test]
    fn empty_and_all_disabled_chains_emit_no_graph() {
        let no_filters =
            capabilities("Available video filters:\n\nAvailable libavfilter filters:\n");
        let empty = compile_chain(&chain(vec![]), &no_filters).unwrap();
        assert!(empty.vf.is_empty());
        assert!(empty.is_empty());
        assert_eq!(empty.len(), 0);
        let disabled = compile_chain(
            &chain(
                filters()
                    .into_iter()
                    .map(|filter| (filter, false))
                    .collect(),
            ),
            &capabilities(REGISTRY),
        )
        .unwrap();
        assert!(disabled.is_empty());
        assert_eq!(disabled.len(), 0);
    }

    #[test]
    fn display_labels_are_accepted_but_never_interpolated() {
        for label in [
            "],lavfi=[movie=/tmp/evil],@furami_0:",
            "quotes'\"\\,;[]:=",
            "Unicode é\n\0",
        ] {
            let chain = FilterChain::new(vec![FilterEntry::new(
                label.to_owned(),
                filters()[1].clone(),
                true,
            )])
            .unwrap();
            let compiled = compile_chain(&chain, &capabilities(REGISTRY)).unwrap();
            assert_eq!(compiled.vf, GOLDENS[1]);
            assert!(!compiled.vf.contains(label));
        }
    }

    #[test]
    fn catalog_and_compiler_reject_each_missing_capability_even_when_disabled() {
        for (kind, token, requirement) in [
            (
                FilterKind::Format,
                "  format           force output format\n",
                FilterRequirement::NativeFilter,
            ),
            (
                FilterKind::Eq,
                "  eq               Adjust brightness.\n",
                FilterRequirement::LinkedFfmpegFilter,
            ),
            (
                FilterKind::Unsharp,
                "  unsharp          Sharpen.\n",
                FilterRequirement::LinkedFfmpegFilter,
            ),
            (
                FilterKind::Hqdn3d,
                "  hqdn3d           Denoise.\n",
                FilterRequirement::LinkedFfmpegFilter,
            ),
            (
                FilterKind::Bwdif,
                "  bwdif            Deinterlace the input image.\n",
                FilterRequirement::LinkedFfmpegFilter,
            ),
        ] {
            let capabilities = capabilities(&REGISTRY.replace(token, ""));
            assert!(!capabilities.supports(kind));
            assert!(
                matches!(check_catalog(&capabilities), Err(FilterCatalogError::MissingCapability { filter, requirement: actual }) if filter == kind && actual == requirement)
            );
            let filter = filters()
                .into_iter()
                .find(|filter| filter.kind() == kind)
                .unwrap();
            for enabled in [true, false] {
                assert!(
                    matches!(compile_chain(&chain(vec![(filter.clone(), enabled)]), &capabilities), Err(FilterCatalogError::MissingCapability { filter, requirement: actual }) if filter == kind && actual == requirement)
                );
            }
        }
    }

    #[test]
    fn native_lavfi_bridge_is_required_for_each_ffmpeg_entry() {
        let capabilities =
            capabilities(&REGISTRY.replace("  lavfi            libavfilter bridge\n", ""));
        assert!(capabilities.supports(FilterKind::Format));
        for filter in filters().into_iter().skip(1) {
            let kind = filter.kind();
            assert!(!capabilities.supports(kind));
            assert!(
                matches!(compile_chain(&chain(vec![(filter, false)]), &capabilities), Err(FilterCatalogError::MissingCapability { filter, requirement: FilterRequirement::LavfiBridge }) if filter == kind)
            );
        }
        assert!(matches!(
            check_catalog(&capabilities),
            Err(FilterCatalogError::MissingCapability {
                filter: FilterKind::Eq,
                requirement: FilterRequirement::LavfiBridge
            })
        ));
    }

    #[test]
    fn complete_tokens_and_section_origins_cannot_be_confused() {
        let registry = "Available video filters:\n  format_extra     not format\n  lavfi-bridge     not lavfi\n  eq               native only\n\nAvailable libavfilter filters:\n  format           linked only\n  eqevil           not eq\n  unsharp_extra    not unsharp\n  hqdn3d_extra     not hqdn3d\n  bwdif_extra      not bwdif\n";
        let capabilities = capabilities(registry);
        for kind in FilterKind::ALL {
            assert!(!capabilities.supports(kind));
        }
    }

    #[test]
    fn malformed_registry_is_not_reported_as_missing_filters() {
        for registry in [
            "",
            "Available video filters:\n  format description\n",
            "Available libavfilter filters:\n  eq description\n",
            "Available libavfilter filters:\nAvailable video filters:\n",
            "Available video filters:\nAvailable video filters:\nAvailable libavfilter filters:\n",
            "Available video filters:\nAvailable libavfilter filters:\nAvailable libavfilter filters:\n",
            "Available video filters: extra\nAvailable libavfilter filters:\n",
            "Available video filters:\n  format\nAvailable libavfilter filters:\n",
            "Available video filters:\n  [format] invalid name\nAvailable libavfilter filters:\n",
            "Available video filters:\n  format description\nUnexpected section:\nAvailable libavfilter filters:\n",
            "warning before registry\nAvailable video filters:\nAvailable libavfilter filters:\n",
        ] {
            assert!(
                matches!(
                    parse_registry(registry),
                    Err(FilterCatalogError::MalformedQuery {
                        query: CatalogQuery::VideoFilters,
                        ..
                    })
                ),
                "{registry:?}"
            );
        }
    }

    #[test]
    fn version_query_requires_exact_unique_pinned_versions() {
        let (mpv, ffmpeg) = parse_versions(VERSION).unwrap();
        assert_eq!(mpv, "v0.41.0");
        assert_eq!(ffmpeg, "8.1.2");
        for (from, to, component) in [
            ("v0.41.0", "v0.41.01", "mpv"),
            ("8.1.2", "8.1.20", "FFmpeg"),
        ] {
            assert!(
                matches!(parse_versions(&VERSION.replace(from, to)), Err(FilterCatalogError::QualificationMismatch { component: actual, .. }) if actual == component)
            );
        }
        for text in [
            "",
            "mpv v0.41.0\n",
            "FFmpeg version: 8.1.2\n",
            "mpv v0.41.0\nmpv v0.41.0\nFFmpeg version: 8.1.2\n",
            "mpv v0.41.0\nFFmpeg version: 8.1.2\nFFmpeg version: 8.1.2\n",
            "mpv\nFFmpeg version: 8.1.2\n",
            "mpv v0.41.0\nFFmpeg version:\n",
        ] {
            assert!(matches!(
                parse_versions(text),
                Err(FilterCatalogError::MalformedQuery {
                    query: CatalogQuery::Version,
                    ..
                })
            ));
        }
    }

    #[test]
    fn failed_status_signal_and_encoding_are_distinct_from_parsing() {
        for query in [CatalogQuery::Version, CatalogQuery::VideoFilters] {
            for raw_status in [7 << 8, 9] {
                let output = Output {
                    status: ExitStatus::from_raw(raw_status),
                    stdout: REGISTRY.as_bytes().to_vec(),
                    stderr: b"real failure".to_vec(),
                };
                let expected_status = if raw_status == 9 { None } else { Some(7) };
                assert!(
                    matches!(query_text(query, output), Err(FilterCatalogError::QueryFailed { query: actual, status, stderr }) if actual == query && status == expected_status && stderr == "real failure")
                );
            }
            let output = Output {
                status: ExitStatus::from_raw(0),
                stdout: vec![0xff],
                stderr: vec![],
            };
            assert!(
                matches!(query_text(query, output), Err(FilterCatalogError::InvalidQueryEncoding { query: actual }) if actual == query)
            );
        }
    }

    #[test]
    fn qualified_inventory_fixture_preserves_native_and_linked_origins() {
        // Historical transcript has user-config errors; production must not
        // accept that preamble. Only its genuine registry section is a parser
        // fixture here, not evidence of a new successful runtime query.
        let transcript =
            include_str!("../../tests/fixtures/FUR-002/common/probes/mpv_vf_list.stdout");
        assert!(parse_registry(transcript).is_err());
        let (_, registry) = transcript.split_once(NATIVE_HEADER).unwrap();
        check_catalog(&capabilities(&format!("{NATIVE_HEADER}{registry}"))).unwrap();
    }

    #[test]
    fn explicit_scalars_rectangular_sizes_and_sdr_values_have_canonical_syntax() {
        let entries = vec![
            (
                Filter::Format(FormatParams::new(
                    SdrMatrix::Bt709,
                    ColorLevels::Limited,
                    SdrGamma::Bt1886,
                )),
                true,
            ),
            (
                Filter::Eq(
                    EqParams::new(EqValues {
                        contrast: 1.06,
                        brightness: -0.125,
                        saturation: 1.04,
                        gamma: 2.2,
                        gamma_r: 0.1,
                        gamma_g: 10.0,
                        gamma_b: 0.75,
                        gamma_weight: 0.125,
                    })
                    .unwrap(),
                ),
                true,
            ),
            (
                Filter::Unsharp(
                    UnsharpParams::new(UnsharpValues {
                        luma_msize_x: 3,
                        luma_msize_y: 23,
                        luma_amount: -2.0,
                        chroma_msize_x: 7,
                        chroma_msize_y: 9,
                        chroma_amount: 5.0,
                        alpha_msize_x: 11,
                        alpha_msize_y: 13,
                        alpha_amount: -0.0,
                    })
                    .unwrap(),
                ),
                true,
            ),
            (
                Filter::Hqdn3d(
                    Hqdn3dParams::new(Hqdn3dValues {
                        luma_spatial: 252.0,
                        chroma_spatial: 1.5,
                        luma_tmp: 6.0,
                        chroma_tmp: 4.5,
                    })
                    .unwrap(),
                ),
                true,
            ),
            (
                Filter::Bwdif(BwdifParams::new(
                    BwdifMode::SendFrame,
                    FieldParity::TopFieldFirst,
                    DeinterlaceSelection::Interlaced,
                )),
                true,
            ),
        ];
        assert_eq!(
            compile_chain(&chain(entries), &capabilities(REGISTRY))
                .unwrap()
                .vf,
            concat!(
                "@furami_0:format=colormatrix=bt.709:colorlevels=limited:gamma=bt.1886:convert=no,",
                "@furami_1:lavfi=[eq=contrast=1.06:brightness=-0.125:saturation=1.04:gamma=2.2:gamma_r=0.1:gamma_g=10:gamma_b=0.75:gamma_weight=0.125:eval=init],",
                "@furami_2:lavfi=[unsharp=luma_msize_x=3:luma_msize_y=23:luma_amount=-2:chroma_msize_x=7:chroma_msize_y=9:chroma_amount=5:alpha_msize_x=11:alpha_msize_y=13:alpha_amount=0],",
                "@furami_3:lavfi=[hqdn3d=luma_spatial=252:chroma_spatial=1.5:luma_tmp=6:chroma_tmp=4.5],",
                "@furami_4:lavfi=[bwdif=mode=send_frame:parity=tff:deint=interlaced]",
            ),
        );
    }

    #[test]
    fn all_closed_format_and_parity_enums_map_to_pinned_tokens() {
        let capabilities = capabilities(REGISTRY);
        for (matrix, token) in [
            (SdrMatrix::Auto, "auto"),
            (SdrMatrix::Bt601, "bt.601"),
            (SdrMatrix::Bt709, "bt.709"),
            (SdrMatrix::Smpte240m, "smpte-240m"),
            (SdrMatrix::Rgb, "rgb"),
        ] {
            let compiled = compile_chain(
                &chain(vec![(
                    Filter::Format(FormatParams::new(matrix, ColorLevels::Full, SdrGamma::Auto)),
                    true,
                )]),
                &capabilities,
            )
            .unwrap();
            assert_eq!(
                compiled.vf,
                format!(
                    "@furami_0:format=colormatrix={token}:colorlevels=full:gamma=auto:convert=no"
                )
            );
        }
        for (gamma, token) in [
            (SdrGamma::Auto, "auto"),
            (SdrGamma::Bt1886, "bt.1886"),
            (SdrGamma::Srgb, "srgb"),
            (SdrGamma::Linear, "linear"),
            (SdrGamma::Gamma18, "gamma1.8"),
            (SdrGamma::Gamma20, "gamma2.0"),
            (SdrGamma::Gamma22, "gamma2.2"),
            (SdrGamma::Gamma24, "gamma2.4"),
            (SdrGamma::Gamma26, "gamma2.6"),
            (SdrGamma::Gamma28, "gamma2.8"),
        ] {
            let compiled = compile_chain(
                &chain(vec![(
                    Filter::Format(FormatParams::new(SdrMatrix::Auto, ColorLevels::Auto, gamma)),
                    true,
                )]),
                &capabilities,
            )
            .unwrap();
            assert_eq!(
                compiled.vf,
                format!(
                    "@furami_0:format=colormatrix=auto:colorlevels=auto:gamma={token}:convert=no"
                )
            );
        }
        let compiled = compile_chain(
            &chain(vec![(
                Filter::Bwdif(BwdifParams::new(
                    BwdifMode::SendField,
                    FieldParity::BottomFieldFirst,
                    DeinterlaceSelection::All,
                )),
                true,
            )]),
            &capabilities,
        )
        .unwrap();
        assert_eq!(
            compiled.vf,
            "@furami_0:lavfi=[bwdif=mode=send_field:parity=bff:deint=all]"
        );
    }

    #[test]
    fn first_unavailable_entry_wins_in_original_order_even_if_disabled() {
        let capabilities = capabilities(
            "Available video filters:\n  lavfi bridge\nAvailable libavfilter filters:\n  bwdif deinterlace\n",
        );
        let [format, eq, unsharp, _, _] = filters();
        let error = compile_chain(
            &chain(vec![(unsharp, false), (format, true), (eq, true)]),
            &capabilities,
        )
        .unwrap_err();
        assert!(matches!(
            error,
            FilterCatalogError::MissingCapability {
                filter: FilterKind::Unsharp,
                requirement: FilterRequirement::LinkedFfmpegFilter,
            }
        ));
    }

    #[test]
    fn incomplete_footer_and_duplicate_tokens_fail_closed() {
        for registry in [
            REGISTRY.replace(
                "prefix them with lavfi- to select the libavfilter one.\n",
                "",
            ),
            REGISTRY.replace("Get help on individual entries via: --vf=entry=help\n", ""),
            REGISTRY.replace(
                "  eq               Adjust brightness.\n",
                "  eq Adjust brightness.\n  eq duplicate.\n",
            ),
            REGISTRY.replace(
                "  lavfi            libavfilter bridge\n",
                "  lavfi bridge\n  lavfi duplicate.\n",
            ),
        ] {
            assert!(matches!(
                parse_registry(&registry),
                Err(FilterCatalogError::MalformedQuery {
                    query: CatalogQuery::VideoFilters,
                    ..
                })
            ));
        }
    }

    #[test]
    fn relative_missing_prefixes_and_unexecutable_queries_are_typed() {
        for prefix in [
            Path::new("."),
            Path::new("/furami-nonexistent-qualified-prefix"),
        ] {
            assert!(matches!(
                query_qualified_capabilities(prefix),
                Err(FilterCatalogError::InvalidPrefix { .. })
            ));
        }
        assert!(matches!(
            run_query(
                Path::new("/qualified/prefix"),
                Path::new("/furami-nonexistent-mpv"),
                CatalogQuery::Version
            ),
            Err(FilterCatalogError::QueryExecution {
                query: CatalogQuery::Version,
                ..
            })
        ));
    }

    #[test]
    fn canonical_executable_cannot_escape_its_prefix() {
        use std::os::unix::fs::symlink;
        use std::time::{SystemTime, UNIX_EPOCH};

        struct OwnedDirectory(PathBuf);
        impl Drop for OwnedDirectory {
            fn drop(&mut self) {
                // Only this test's successfully created unique directory is
                // removed; neither the symlink target nor temp root is owned.
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let path = std::env::temp_dir().join(format!(
            "furami-filter-catalog-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
        ));
        std::fs::create_dir(&path).unwrap();
        let directory = OwnedDirectory(path);
        let prefix = directory.0.join("prefix");
        std::fs::create_dir_all(prefix.join("bin")).unwrap();
        let outside = directory.0.join("outside-mpv");
        std::fs::write(&outside, b"never execute this outside-prefix file").unwrap();
        symlink(&outside, prefix.join("bin/mpv")).unwrap();
        assert!(matches!(
            query_qualified_capabilities(&prefix),
            Err(FilterCatalogError::InvalidPrefix { .. })
        ));
    }
}
