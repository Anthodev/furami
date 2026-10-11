//! Qualified gpu-next capabilities and immutable native SDR settings.
//! Announced choices are intersected with the pinned mpv/libplacebo mappings;
//! capability discovery is not visual certification of an algorithm.
//! Effective scaler mapping: mpv 41f6a645068483470267271e1d09966ca3b9f413
//! `video/out/vo_gpu_next.c::map_scaler`, libplacebo
//! cee9b076f2c63104ccfd497fa79c39a867293ec4 `src/filters.c` / `src/filters.h`.
//! The captured fixture provenance records the binary identity separately.
use super::filter_catalog::{self, FilterCapabilities, FilterCatalogError};
use crate::domain::render::{
    DitherMode, RendererProperty, RendererSettings, ScalerChoice, ScalerKernel,
};
use std::{
    ffi::CStr,
    path::{Path, PathBuf},
    process::Output,
};
use thiserror::Error;

#[derive(Debug)]
pub struct QualifiedTreatmentCapabilities {
    pub filters: Result<FilterCapabilities, FilterCatalogError>,
    pub renderer: Result<RendererCapabilities, RendererCatalogError>,
}
impl QualifiedTreatmentCapabilities {
    pub fn query(prefix: &Path) -> Self {
        Self {
            filters: filter_catalog::query_qualified_capabilities(prefix),
            renderer: query_qualified_capabilities(prefix),
        }
    }
}

#[derive(Debug, Error)]
pub enum RendererCatalogError {
    #[error("renderer qualification failed: {0}")]
    Qualification(#[from] FilterCatalogError),
    #[error("renderer {argument} query could not execute: {source}")]
    QueryExecution {
        argument: &'static str,
        #[source]
        source: std::io::Error,
    },
    #[error("renderer {argument} query failed ({status:?}): {stderr}")]
    QueryFailed {
        argument: &'static str,
        status: Option<i32>,
        stderr: String,
    },
    #[error("renderer {argument} query is not UTF-8")]
    InvalidEncoding { argument: &'static str },
    #[error("renderer catalog {}: {reason}", catalog_property_name(*property))]
    Malformed {
        property: Option<RendererProperty>,
        reason: &'static str,
    },
    #[error("unsupported renderer {} value {value}", native_name(*property).to_string_lossy())]
    UnsupportedValue {
        property: RendererProperty,
        value: &'static str,
    },
}

#[derive(Debug)]
pub struct RendererCapabilities {
    prefix: PathBuf,
    executable: PathBuf,
}
impl RendererCapabilities {
    pub fn prefix(&self) -> &Path {
        &self.prefix
    }
    pub fn executable(&self) -> &Path {
        &self.executable
    }
}

const PROPERTIES: [RendererProperty; 9] = [
    RendererProperty::Scale,
    RendererProperty::Cscale,
    RendererProperty::Dscale,
    RendererProperty::ScaleAntiring,
    RendererProperty::Dither,
    RendererProperty::DebandIterations,
    RendererProperty::DebandGrain,
    RendererProperty::Interpolation,
    RendererProperty::Deband,
];
pub(super) fn native_name(property: RendererProperty) -> &'static CStr {
    match property {
        RendererProperty::Scale => c"scale",
        RendererProperty::Cscale => c"cscale",
        RendererProperty::Dscale => c"dscale",
        RendererProperty::ScaleAntiring => c"scale-antiring",
        RendererProperty::Dither => c"dither",
        RendererProperty::Deband => c"deband",
        RendererProperty::DebandIterations => c"deband-iterations",
        RendererProperty::DebandGrain => c"deband-grain",
        RendererProperty::Interpolation => c"interpolation",
    }
}
fn catalog_property_name(property: Option<RendererProperty>) -> &'static str {
    property
        .map(|property| native_name(property).to_str().expect("static ASCII name"))
        .unwrap_or("unknown property")
}
fn malformed(property: Option<RendererProperty>, reason: &'static str) -> RendererCatalogError {
    RendererCatalogError::Malformed { property, reason }
}
fn query_text(argument: &'static str, output: Output) -> Result<String, RendererCatalogError> {
    if !output.status.success() {
        return Err(RendererCatalogError::QueryFailed {
            argument,
            status: output.status.code(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }
    if std::str::from_utf8(&output.stderr).is_err() {
        return Err(RendererCatalogError::InvalidEncoding { argument });
    }
    String::from_utf8(output.stdout).map_err(|_| RendererCatalogError::InvalidEncoding { argument })
}
fn run(
    prefix: &Path,
    executable: &Path,
    argument: &'static str,
) -> Result<String, RendererCatalogError> {
    let output = filter_catalog::qualified_argument_command(prefix, executable, argument)
        .output()
        .map_err(|source| RendererCatalogError::QueryExecution { argument, source })?;
    query_text(argument, output)
}
pub fn query_qualified_capabilities(
    prefix: &Path,
) -> Result<RendererCapabilities, RendererCatalogError> {
    let (prefix, executable) = filter_catalog::qualified_executable(prefix)?;
    filter_catalog::parse_versions(&run(&prefix, &executable, "--version")?)?;
    let list = run(&prefix, &executable, "--list-options")?;
    let helps = [
        run(&prefix, &executable, "--scale=help")?,
        run(&prefix, &executable, "--cscale=help")?,
        run(&prefix, &executable, "--dscale=help")?,
        run(&prefix, &executable, "--dither=help")?,
    ];
    parse_catalog(&list, [&helps[0], &helps[1], &helps[2], &helps[3]])?;
    Ok(RendererCapabilities { prefix, executable })
}

fn parse_catalog(list: &str, helps: [&str; 4]) -> Result<(), RendererCatalogError> {
    for property in PROPERTIES {
        let name = native_name(property).to_str().expect("static ASCII name");
        let mut entries = list.lines().filter_map(|line| {
            let line = line.strip_prefix(" --")?;
            let end = line.find(char::is_whitespace)?;
            (line[..end] == *name).then_some(line[end..].trim_start())
        });
        let entry = entries
            .next()
            .ok_or_else(|| malformed(Some(property), "missing property"))?;
        if entries.next().is_some() {
            return Err(malformed(Some(property), "duplicate property"));
        }
        let (kind, default) = entry
            .rsplit_once(" (default: ")
            .ok_or_else(|| malformed(Some(property), "missing default"))?;
        let default = default
            .strip_suffix(')')
            .ok_or_else(|| malformed(Some(property), "malformed default"))?;
        let expected_default = match property {
            RendererProperty::Scale => "lanczos",
            RendererProperty::Cscale => "",
            RendererProperty::Dscale => "hermite",
            RendererProperty::Dither => "fruit",
            RendererProperty::ScaleAntiring => "0",
            RendererProperty::DebandIterations => "1",
            RendererProperty::DebandGrain => "32",
            RendererProperty::Deband | RendererProperty::Interpolation => "no",
        };
        if default != expected_default {
            return Err(malformed(Some(property), "qualified default mismatch"));
        }
        match property {
            RendererProperty::Scale
            | RendererProperty::Cscale
            | RendererProperty::Dscale
            | RendererProperty::Dither => {
                let choices = kind
                    .strip_prefix("Choices: ")
                    .ok_or_else(|| malformed(Some(property), "expected choices type"))?;
                let inherit = matches!(
                    property,
                    RendererProperty::Cscale | RendererProperty::Dscale
                );
                if choices.starts_with(' ') != inherit {
                    return Err(malformed(Some(property), "inherit choice mismatch"));
                }
                let tokens = choices.split_ascii_whitespace();
                let ordered = if property == RendererProperty::Dither {
                    tokens.eq(DitherMode::ALL.iter().map(|mode| mode.as_str()))
                } else {
                    tokens.eq(ScalerKernel::ALL.iter().map(|kernel| kernel.as_str()))
                };
                if !ordered {
                    return Err(malformed(
                        Some(property),
                        "choices missing, duplicated or inconsistent with pinned sources",
                    ));
                }
            }
            _ => {
                let expected = match property {
                    RendererProperty::ScaleAntiring => "Float (0 to 1)",
                    RendererProperty::DebandIterations => "Integer (0 to 16)",
                    RendererProperty::DebandGrain => "Float (0 to 4096)",
                    _ => "Flag",
                };
                if kind != expected {
                    return Err(malformed(Some(property), "type or bounds mismatch"));
                }
            }
        }
    }
    for (property, text) in [
        RendererProperty::Scale,
        RendererProperty::Cscale,
        RendererProperty::Dscale,
        RendererProperty::Dither,
    ]
    .into_iter()
    .zip(helps)
    {
        let name = native_name(property).to_str().expect("static ASCII name");
        let mut lines = text.lines();
        if lines.next() != Some(format!("Valid values for option {name} are:").as_str()) {
            return Err(malformed(Some(property), "invalid help header"));
        }
        if matches!(
            property,
            RendererProperty::Cscale | RendererProperty::Dscale
        ) && lines.next() != Some("    (passing nothing)")
        {
            return Err(malformed(Some(property), "missing help inherit"));
        }
        let dither = property == RendererProperty::Dither;
        let mut expected_dither = DitherMode::ALL.iter().map(|mode| mode.as_str());
        let mut expected_scaler = ScalerKernel::ALL.iter().map(|kernel| kernel.as_str());
        for line in lines {
            let token = line
                .strip_prefix("    ")
                .ok_or_else(|| malformed(Some(property), "malformed help choice"))?;
            let matched = if dither {
                expected_dither.next() == Some(token)
            } else {
                expected_scaler.next() == Some(token)
            };
            if !matched {
                return Err(malformed(Some(property), "help choices inconsistent"));
            }
        }
        let exhausted = if dither {
            expected_dither.next().is_none()
        } else {
            expected_scaler.next().is_none()
        };
        if !exhausted {
            return Err(malformed(Some(property), "missing help choices"));
        }
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum NativeRendererValue {
    Choice(&'static CStr),
    Flag(bool),
    Int(i64),
    Float(f64),
}
#[derive(Clone, Debug, PartialEq)]
pub struct CompiledRendererSettings {
    requested: RendererSettings,
    native: [(RendererProperty, NativeRendererValue); 9],
}
impl CompiledRendererSettings {
    pub fn requested(&self) -> &RendererSettings {
        &self.requested
    }
    pub(super) fn native(&self) -> &[(RendererProperty, NativeRendererValue); 9] {
        &self.native
    }
}
fn supported(
    property: RendererProperty,
    kernel: ScalerKernel,
) -> Result<&'static CStr, RendererCatalogError> {
    if matches!(
        kernel,
        ScalerKernel::EwaLanczosSoft
            | ScalerKernel::HaasnSoft
            | ScalerKernel::Bartlett
            | ScalerKernel::Tukey
    ) {
        return Err(RendererCatalogError::UnsupportedValue {
            property,
            value: kernel.as_str(),
        });
    }
    Ok(kernel_native(kernel))
}
fn choice(
    property: RendererProperty,
    value: ScalerChoice,
) -> Result<NativeRendererValue, RendererCatalogError> {
    Ok(NativeRendererValue::Choice(match value {
        ScalerChoice::Inherit => c"",
        ScalerChoice::Kernel(kernel) => supported(property, kernel)?,
    }))
}
pub fn compile_renderer(
    settings: &RendererSettings,
    _capabilities: &RendererCapabilities,
) -> Result<CompiledRendererSettings, RendererCatalogError> {
    let v = settings.values();
    let native = [
        (
            RendererProperty::Scale,
            NativeRendererValue::Choice(supported(RendererProperty::Scale, v.scale)?),
        ),
        (
            RendererProperty::Cscale,
            choice(RendererProperty::Cscale, v.cscale)?,
        ),
        (
            RendererProperty::Dscale,
            choice(RendererProperty::Dscale, v.dscale)?,
        ),
        (
            RendererProperty::ScaleAntiring,
            NativeRendererValue::Float(f64::from(v.scale_antiring as f32)),
        ),
        (
            RendererProperty::Dither,
            NativeRendererValue::Choice(match v.dither {
                DitherMode::Fruit => c"fruit",
                DitherMode::Ordered => c"ordered",
                DitherMode::ErrorDiffusion => c"error-diffusion",
                DitherMode::No => c"no",
            }),
        ),
        (
            RendererProperty::DebandIterations,
            NativeRendererValue::Int(i64::from(v.deband.iterations)),
        ),
        (
            RendererProperty::DebandGrain,
            NativeRendererValue::Float(f64::from(v.deband.grain as f32)),
        ),
        (
            RendererProperty::Interpolation,
            NativeRendererValue::Flag(v.interpolation),
        ),
        (
            RendererProperty::Deband,
            NativeRendererValue::Flag(v.deband.enabled),
        ),
    ];
    Ok(CompiledRendererSettings {
        requested: settings.clone(),
        native,
    })
}
fn kernel_native(kernel: ScalerKernel) -> &'static CStr {
    match kernel {
        ScalerKernel::Bilinear => c"bilinear",
        ScalerKernel::BicubicFast => c"bicubic_fast",
        ScalerKernel::Oversample => c"oversample",
        ScalerKernel::Spline16 => c"spline16",
        ScalerKernel::Spline36 => c"spline36",
        ScalerKernel::Spline64 => c"spline64",
        ScalerKernel::Sinc => c"sinc",
        ScalerKernel::Lanczos => c"lanczos",
        ScalerKernel::Ginseng => c"ginseng",
        ScalerKernel::Bicubic => c"bicubic",
        ScalerKernel::Hermite => c"hermite",
        ScalerKernel::CatmullRom => c"catmull_rom",
        ScalerKernel::Mitchell => c"mitchell",
        ScalerKernel::Robidoux => c"robidoux",
        ScalerKernel::RobidouxSharp => c"robidouxsharp",
        ScalerKernel::Box => c"box",
        ScalerKernel::Nearest => c"nearest",
        ScalerKernel::Triangle => c"triangle",
        ScalerKernel::Gaussian => c"gaussian",
        ScalerKernel::Jinc => c"jinc",
        ScalerKernel::EwaLanczos => c"ewa_lanczos",
        ScalerKernel::EwaHanning => c"ewa_hanning",
        ScalerKernel::EwaGinseng => c"ewa_ginseng",
        ScalerKernel::EwaLanczosSharp => c"ewa_lanczossharp",
        ScalerKernel::EwaLanczos4Sharpest => c"ewa_lanczos4sharpest",
        ScalerKernel::EwaLanczosSoft => c"ewa_lanczossoft",
        ScalerKernel::HaasnSoft => c"haasnsoft",
        ScalerKernel::EwaRobidoux => c"ewa_robidoux",
        ScalerKernel::EwaRobidouxSharp => c"ewa_robidouxsharp",
        ScalerKernel::Bartlett => c"bartlett",
        ScalerKernel::Cosine => c"cosine",
        ScalerKernel::Hanning => c"hanning",
        ScalerKernel::Tukey => c"tukey",
        ScalerKernel::Hamming => c"hamming",
        ScalerKernel::Quadric => c"quadric",
        ScalerKernel::Welch => c"welch",
        ScalerKernel::Kaiser => c"kaiser",
        ScalerKernel::Blackman => c"blackman",
        ScalerKernel::Sphinx => c"sphinx",
    }
}

#[cfg(test)]
pub(crate) fn fixture_capabilities() -> RendererCapabilities {
    parse_catalog(LIST, HELPS).expect("genuine captured qualified renderer inventory");
    RendererCapabilities {
        prefix: PathBuf::from("/qualified"),
        executable: PathBuf::from("/qualified/bin/mpv"),
    }
}
#[cfg(test)]
const LIST: &str = include_str!("../../tests/fixtures/FUR-016/catalog/list-options.stdout");
#[cfg(test)]
const HELPS: [&str; 4] = [
    include_str!("../../tests/fixtures/FUR-016/catalog/scale-help.stdout"),
    include_str!("../../tests/fixtures/FUR-016/catalog/cscale-help.stdout"),
    include_str!("../../tests/fixtures/FUR-016/catalog/dscale-help.stdout"),
    include_str!("../../tests/fixtures/FUR-016/catalog/dither-help.stdout"),
];

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn genuine_inventory_includes_plain_flags_and_empty_inherit() {
        let capabilities = fixture_capabilities();
        let compiled = compile_renderer(&RendererSettings::default(), &capabilities).unwrap();
        assert_eq!(
            compiled.native()[1],
            (RendererProperty::Cscale, NativeRendererValue::Choice(c""))
        );
        assert_eq!(
            compiled.native()[7],
            (
                RendererProperty::Interpolation,
                NativeRendererValue::Flag(false)
            )
        );
    }
    #[test]
    fn missing_duplicate_types_bounds_defaults_and_choices_are_rejected() {
        for (from, to) in [
            (" --interpolation ", " --not-interpolation "),
            ("Flag (default: no)", "Choices: no yes (default: no)"),
            ("Float (0 to 4096)", "Float (0 to 4095)"),
            (
                "Integer (0 to 16) (default: 1)",
                "Integer (0 to 16) (default: 2)",
            ),
            ("bilinear bicubic_fast", "bilinear bilinear bicubic_fast"),
            ("Choices:  bilinear", "Choices: bilinear"),
        ] {
            assert!(
                parse_catalog(&LIST.replace(from, to), HELPS).is_err(),
                "{from}"
            );
        }
        let duplicate = format!("{LIST}\n --interpolation                  Flag (default: no)\n");
        assert!(matches!(
            parse_catalog(&duplicate, HELPS),
            Err(RendererCatalogError::Malformed {
                property: Some(RendererProperty::Interpolation),
                ..
            })
        ));
        let mut helps = HELPS;
        helps[0] = "Valid values for option scale are:\n    bilinear\n";
        assert!(parse_catalog(LIST, helps).is_err());
    }
    #[test]
    fn effective_mapping_rejects_exactly_four_kernels_for_every_scaler() {
        let capabilities = fixture_capabilities();
        for kernel in ScalerKernel::ALL {
            for property in [
                RendererProperty::Scale,
                RendererProperty::Cscale,
                RendererProperty::Dscale,
            ] {
                let mut values = RendererSettings::default().values();
                match property {
                    RendererProperty::Scale => values.scale = *kernel,
                    RendererProperty::Cscale => values.cscale = ScalerChoice::Kernel(*kernel),
                    _ => values.dscale = ScalerChoice::Kernel(*kernel),
                }
                let result =
                    compile_renderer(&RendererSettings::new(values).unwrap(), &capabilities);
                if matches!(
                    kernel,
                    ScalerKernel::EwaLanczosSoft
                        | ScalerKernel::HaasnSoft
                        | ScalerKernel::Bartlett
                        | ScalerKernel::Tukey
                ) {
                    assert!(
                        matches!(result, Err(RendererCatalogError::UnsupportedValue { property: actual, value }) if actual == property && value == kernel.as_str())
                    );
                } else {
                    assert!(result.is_ok(), "{kernel:?}/{property:?}");
                }
            }
        }
    }
    #[test]
    fn compilation_keeps_requested_precision_and_canonicalizes_binary32() {
        let settings: RendererSettings = serde_json::from_str(include_str!(
            "../../tests/fixtures/FUR-016/reference-renderer.json"
        ))
        .unwrap();
        let compiled = compile_renderer(&settings, &fixture_capabilities()).unwrap();
        assert_eq!(compiled.requested().values().scale_antiring, 0.7);
        assert_eq!(
            compiled.native()[3].1,
            NativeRendererValue::Float(f64::from(0.7_f32))
        );
        assert_eq!(compiled.native().map(|(property, _)| property), PROPERTIES);
        let mut values = settings.values();
        values.deband.enabled = false;
        values.deband.grain = 4097.0;
        assert!(RendererSettings::new(values).is_err());
        for mode in DitherMode::ALL {
            let mut values = settings.values();
            values.dither = *mode;
            values.interpolation = true;
            assert!(
                compile_renderer(
                    &RendererSettings::new(values).unwrap(),
                    &fixture_capabilities()
                )
                .is_ok()
            );
        }
    }
    #[test]
    fn failed_and_invalid_utf8_queries_never_create_capabilities() {
        use std::os::unix::process::ExitStatusExt;
        let output = |code, stdout| Output {
            status: std::process::ExitStatus::from_raw(code),
            stdout,
            stderr: vec![],
        };
        assert!(matches!(
            query_text("--list-options", output(256, vec![])),
            Err(RendererCatalogError::QueryFailed { .. })
        ));
        assert!(matches!(
            query_text("--list-options", output(0, vec![255])),
            Err(RendererCatalogError::InvalidEncoding { .. })
        ));
    }
}

#[cfg(test)]
mod boundary_tests {
    use super::*;
    #[test]
    fn invalid_prefix_is_refused_without_discovery() {
        assert!(matches!(
            query_qualified_capabilities(Path::new("relative")),
            Err(RendererCatalogError::Qualification(
                FilterCatalogError::InvalidPrefix { .. }
            ))
        ));
    }
    #[test]
    fn every_property_duplicate_and_help_duplicate_is_refused() {
        for property in PROPERTIES {
            let name = native_name(property).to_str().unwrap();
            let row = LIST
                .lines()
                .find(|line| {
                    line.strip_prefix(" --")
                        .and_then(|line| line.split_ascii_whitespace().next())
                        == Some(name)
                })
                .unwrap();
            assert!(matches!(parse_catalog(&format!("{LIST}\n{row}\n"), HELPS),
                Err(RendererCatalogError::Malformed { property: Some(actual), .. }) if actual == property));
        }
        for index in 0..4 {
            let duplicate = format!("{}{}\n", HELPS[index], HELPS[index].lines().nth(1).unwrap());
            let mut helps = HELPS;
            helps[index] = &duplicate;
            assert!(parse_catalog(LIST, helps).is_err());
            let garbage = format!("{}unexpected\n", HELPS[index]);
            helps[index] = &garbage;
            assert!(parse_catalog(LIST, helps).is_err());
        }
    }
    #[test]
    fn full_bounds_are_compiled_even_with_deband_disabled() {
        for (antiring, iterations, grain) in [(0.0, 0, 0.0), (1.0, 16, 4096.0)] {
            let mut values = RendererSettings::default().values();
            values.scale_antiring = antiring;
            values.deband.enabled = false;
            values.deband.iterations = iterations;
            values.deband.grain = grain;
            let settings = RendererSettings::new(values).unwrap();
            let compiled = compile_renderer(&settings, &fixture_capabilities()).unwrap();
            assert_eq!(
                compiled.native()[5].1,
                NativeRendererValue::Int(i64::from(iterations))
            );
            assert_eq!(compiled.native()[6].1, NativeRendererValue::Float(grain));
            assert_eq!(compiled.native()[8].1, NativeRendererValue::Flag(false));
        }
    }
}
