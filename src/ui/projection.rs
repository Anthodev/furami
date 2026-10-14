//! Read-only product values. Action tokens are opaque decimal strings, never JS numbers.
use crate::domain::{
    capture::{FrameRate, FrameSize},
    filters::{self, Filter, FilterChain, FilterKind},
    render::{DitherMode, RendererSettings, RendererValues, ScalerChoice, ScalerKernel},
};
use crate::media::{
    filter_catalog::FilterCapabilities,
    render_catalog::{self, QualifiedTreatmentCapabilities},
};
use serde::Serialize;

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Snapshot {
    pub schema: u32,
    pub state_token: String,
    pub generation: String,
    pub product_phase: String,
    pub gate_phase: String,
    pub paused: bool,
    pub opening: bool,
    pub closing: bool,
    pub can_edit_selection: bool,
    pub can_restart: bool,
    pub can_reconnect: bool,
    pub can_toggle_pause: bool,
    pub can_set_gain: bool,
    pub selection_revision: String,
    pub selection: Option<Selection>,
    pub applied: Option<Selection>,
    pub last_valid: Option<Selection>,
    pub live: Live,
    pub stream_stats: Option<StreamStats>,
    pub catalog: Catalog,
    pub filters: Filters,
    pub renderer: Renderer,
    pub output: Output,
    pub recovery: Recovery,
    pub settings: Settings,
    pub intent_error: Option<IntentError>,
}
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Selection {
    pub source_key: String,
    pub source_label: String,
    pub source_available: bool,
    pub size: Option<Size>,
    pub rate: Option<Rate>,
    pub format_policy: &'static str,
    pub manual_format_key: String,
    pub requested_format_label: String,
    pub resolved_format_label: String,
    pub audio_enabled: bool,
    pub audio_input_key: String,
    pub audio_input_label: String,
    pub filters: FilterChain,
    pub renderer: RendererSettings,
}
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct StreamStats {
    pub identity_text: String,
    pub input_policy: String,
    pub size: Size,
    pub rate: Rate,
    pub resolved_input: String,
    pub audio_enabled: bool,
    pub audio_input_label: String,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub(super) struct Size {
    pub width: u32,
    pub height: u32,
}
impl From<FrameSize> for Size {
    fn from(value: FrameSize) -> Self {
        Self {
            width: value.width(),
            height: value.height(),
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(super) struct Rate {
    pub numerator: u32,
    pub denominator: u32,
    pub text: String,
}
impl From<FrameRate> for Rate {
    fn from(value: FrameRate) -> Self {
        Self {
            numerator: value.numerator(),
            denominator: value.denominator(),
            text: format!("{}/{}", value.numerator(), value.denominator()),
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub(super) struct Rational {
    pub numerator: u32,
    pub denominator: u32,
}
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Catalog {
    pub revision: String,
    pub busy: bool,
    pub error: String,
    pub source_rows: Vec<SourceRow>,
    pub selected_source_key: String,
    pub format_rows: Vec<FormatRow>,
    pub size_rows: Vec<SizeRow>,
    pub rate_rows: Vec<RateRow>,
    pub size_ranges: Vec<SizeRange>,
    pub interval_ranges: Vec<IntervalRange>,
    pub audio_input_rows: Vec<AudioInputRow>,
    pub selected_audio_input_key: String,
    pub capability_status: String,
    pub capability_reason: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct SourceRow {
    pub key: String,
    pub label: String,
    pub detail: String,
    pub eligible: bool,
    pub reason: String,
    pub vendor_id: String,
    pub product_id: String,
    pub topology: String,
    pub serial: Option<String>,
    pub node_label: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(super) struct FormatRow {
    pub key: String,
    pub label: String,
    pub fourcc: String,
    pub eligible: bool,
    pub reason: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(super) struct SizeRow {
    pub key: String,
    pub label: String,
    pub width: u32,
    pub height: u32,
    pub eligible: bool,
    pub reason: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(super) struct RateRow {
    pub key: String,
    pub label: String,
    pub numerator: u32,
    pub denominator: u32,
    pub exact: String,
    pub eligible: bool,
    pub reason: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct SizeRange {
    pub format_key: String,
    pub kind: &'static str,
    pub min: Size,
    pub max: Size,
    pub step_width: Option<u32>,
    pub step_height: Option<u32>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct IntervalRange {
    pub format_key: String,
    pub kind: &'static str,
    pub min: Rational,
    pub max: Rational,
    pub step: Option<Rational>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(super) struct AudioInputRow {
    pub key: String,
    pub label: String,
    pub eligible: bool,
    pub reason: String,
}
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Live {
    pub source_label: String,
    pub size_label: String,
    pub rate_label: String,
    pub format_label: String,
    pub audio_status: String,
    pub audio_diagnostic: String,
    pub playback_status: String,
    pub volume_percent: i32,
    pub muted: bool,
}
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(untagged)]
pub(super) enum Parameters {
    Format(filters::FormatParams),
    Eq(filters::EqParams),
    Unsharp(filters::UnsharpParams),
    Hqdn3d(filters::Hqdn3dParams),
    Bwdif(filters::BwdifParams),
}
impl From<&Filter> for Parameters {
    fn from(filter: &Filter) -> Self {
        match filter {
            Filter::Format(v) => Self::Format(*v),
            Filter::Eq(v) => Self::Eq(*v),
            Filter::Unsharp(v) => Self::Unsharp(*v),
            Filter::Hqdn3d(v) => Self::Hqdn3d(*v),
            Filter::Bwdif(v) => Self::Bwdif(*v),
        }
    }
}
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub(super) struct Filters {
    pub available: bool,
    pub error: String,
    pub kinds: Vec<FilterKindRow>,
    pub entries: Vec<FilterEntryRow>,
}
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct FilterKindRow {
    pub kind: &'static str,
    pub label: String,
    pub available: bool,
    pub reason: String,
    pub default_parameters: Parameters,
    pub fields: Vec<FilterFieldRow>,
}
#[derive(Clone, Debug, PartialEq, Serialize)]
pub(super) struct FilterEntryRow {
    pub key: String,
    pub label: String,
    pub kind: &'static str,
    pub enabled: bool,
    pub parameters: Parameters,
    pub available: bool,
    pub reason: String,
}
#[derive(Clone, Debug, PartialEq, Serialize)]
pub(super) struct FilterFieldRow {
    pub key: &'static str,
    pub label: String,
    #[serde(rename = "type")]
    pub field_type: &'static str,
    pub choices: Vec<Choice>,
    pub min: Option<f64>,
    pub max: Option<f64>,
    pub step: Option<f64>,
    pub notice: String,
}
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub(super) struct Renderer {
    pub available: bool,
    pub error: String,
    pub fields: Vec<RenderFieldRow>,
    pub notice: String,
}
#[derive(Clone, Debug, PartialEq, Serialize)]
pub(super) struct RenderFieldRow {
    pub key: &'static str,
    pub label: String,
    #[serde(rename = "type")]
    pub field_type: &'static str,
    pub choices: Vec<Choice>,
    pub min: Option<f64>,
    pub max: Option<f64>,
    pub step: Option<f64>,
    pub notice: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(super) struct Choice {
    pub key: String,
    pub label: String,
    pub eligible: bool,
    pub reason: String,
}
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Output {
    pub revision: String,
    pub rows: Vec<OutputRow>,
    pub selected_key: String,
    pub selected_label: String,
    pub effective_label: String,
    pub status: String,
    pub needs_action: bool,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(super) struct OutputRow {
    pub key: String,
    pub label: String,
    pub name: String,
    pub eligible: bool,
    pub reason: String,
}
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub(super) struct Recovery {
    pub evidence: String,
    pub stage: String,
    pub candidates: Vec<RecoveryRow>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(super) struct RecoveryRow {
    pub key: String,
    pub label: String,
    pub detail: String,
    pub eligible: bool,
}
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Settings {
    pub status: String,
    pub path: String,
    pub refused: bool,
    pub startup_reason: String,
    pub close_dialog: String,
    pub reset_token: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(super) struct IntentError {
    pub code: &'static str,
    pub action: &'static str,
    pub field: &'static str,
    pub message: String,
}

pub(super) fn default_filter(kind: FilterKind) -> Filter {
    use filters::*;
    match kind {
        FilterKind::Format => Filter::Format(FormatParams::new(
            SdrMatrix::Auto,
            ColorLevels::Auto,
            SdrGamma::Auto,
        )),
        FilterKind::Eq => Filter::Eq(
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
            .expect("documented eq defaults"),
        ),
        FilterKind::Unsharp => Filter::Unsharp(
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
            .expect("documented unsharp defaults"),
        ),
        FilterKind::Hqdn3d => Filter::Hqdn3d(
            Hqdn3dParams::new(Hqdn3dValues {
                luma_spatial: 4.0,
                chroma_spatial: 3.0,
                luma_tmp: 6.0,
                chroma_tmp: 4.5,
            })
            .expect("documented hqdn3d defaults"),
        ),
        FilterKind::Bwdif => Filter::Bwdif(BwdifParams::new(
            BwdifMode::SendField,
            FieldParity::Auto,
            DeinterlaceSelection::All,
        )),
    }
}

fn filter_fields(kind: FilterKind) -> Vec<FilterFieldRow> {
    use filters::*;
    let number = |key, label: &str, bounds: (f64, f64), notice: &str| FilterFieldRow {
        key,
        label: label.into(),
        field_type: "number",
        choices: Vec::new(),
        min: Some(bounds.0),
        max: Some(bounds.1),
        step: None,
        notice: notice.into(),
    };
    let matrix = |key, label: &str, notice: &str| FilterFieldRow {
        key,
        label: label.into(),
        field_type: "integer",
        choices: Vec::new(),
        min: Some(f64::from(UnsharpParams::MATRIX_MIN)),
        max: Some(f64::from(UnsharpParams::MATRIX_MAX)),
        step: Some(f64::from(UnsharpParams::MATRIX_STEP)),
        notice: notice.into(),
    };
    fn choice(
        key: &'static str,
        label: &str,
        tokens: impl Iterator<Item = &'static str>,
    ) -> FilterFieldRow {
        FilterFieldRow {
            key,
            label: label.into(),
            field_type: "choice",
            choices: tokens
                .map(|token| Choice {
                    key: token.into(),
                    label: token.into(),
                    eligible: true,
                    reason: String::new(),
                })
                .collect(),
            min: None,
            max: None,
            step: None,
            notice: String::new(),
        }
    }
    const STRENGTH: &str = "Force sans dimension.";
    const TAPS: &str = "Nombre impair de taps, de 3 à 23.";
    const ALPHA: &str = "Sans effet garanti si le format ne contient pas de canal alpha.";
    const DENOISE: &str = "Force sans dimension. Zéro demande le défaut relationnel FFmpeg ; il ne désactive pas le filtre.";
    match kind {
        FilterKind::Format => vec![
            choice(
                "matrix",
                "Matrice SDR",
                SdrMatrix::ALL.iter().map(|v| v.as_str()),
            ),
            choice(
                "levels",
                "Plage des niveaux",
                ColorLevels::ALL.iter().map(|v| v.as_str()),
            ),
            choice(
                "gamma",
                "Transfert gamma SDR",
                SdrGamma::ALL.iter().map(|v| v.as_str()),
            ),
        ],
        FilterKind::Eq => vec![
            number("contrast", "Contraste", EqParams::CONTRAST_BOUNDS, STRENGTH),
            number(
                "brightness",
                "Luminosité",
                EqParams::BRIGHTNESS_BOUNDS,
                STRENGTH,
            ),
            number(
                "saturation",
                "Saturation",
                EqParams::SATURATION_BOUNDS,
                STRENGTH,
            ),
            number("gamma", "Gamma", EqParams::GAMMA_BOUNDS, STRENGTH),
            number("gamma_r", "Gamma rouge", EqParams::GAMMA_BOUNDS, STRENGTH),
            number("gamma_g", "Gamma vert", EqParams::GAMMA_BOUNDS, STRENGTH),
            number("gamma_b", "Gamma bleu", EqParams::GAMMA_BOUNDS, STRENGTH),
            number(
                "gamma_weight",
                "Poids du gamma",
                EqParams::GAMMA_WEIGHT_BOUNDS,
                STRENGTH,
            ),
        ],
        FilterKind::Unsharp => vec![
            matrix("luma_msize_x", "Luma · largeur (taps)", TAPS),
            matrix("luma_msize_y", "Luma · hauteur (taps)", TAPS),
            number(
                "luma_amount",
                "Luma · force",
                UnsharpParams::AMOUNT_BOUNDS,
                STRENGTH,
            ),
            matrix("chroma_msize_x", "Chroma · largeur (taps)", TAPS),
            matrix("chroma_msize_y", "Chroma · hauteur (taps)", TAPS),
            number(
                "chroma_amount",
                "Chroma · force",
                UnsharpParams::AMOUNT_BOUNDS,
                STRENGTH,
            ),
            matrix(
                "alpha_msize_x",
                "Alpha · largeur (taps)",
                "Nombre impair de taps, de 3 à 23. Sans effet garanti sans canal alpha.",
            ),
            matrix(
                "alpha_msize_y",
                "Alpha · hauteur (taps)",
                "Nombre impair de taps, de 3 à 23. Sans effet garanti sans canal alpha.",
            ),
            number(
                "alpha_amount",
                "Alpha · force",
                UnsharpParams::AMOUNT_BOUNDS,
                ALPHA,
            ),
        ],
        FilterKind::Hqdn3d => vec![
            number(
                "luma_spatial",
                "Luma · spatial",
                Hqdn3dParams::STRENGTH_BOUNDS,
                DENOISE,
            ),
            number(
                "chroma_spatial",
                "Chroma · spatial",
                Hqdn3dParams::STRENGTH_BOUNDS,
                DENOISE,
            ),
            number(
                "luma_tmp",
                "Luma · temporel",
                Hqdn3dParams::STRENGTH_BOUNDS,
                DENOISE,
            ),
            number(
                "chroma_tmp",
                "Chroma · temporel",
                Hqdn3dParams::STRENGTH_BOUNDS,
                DENOISE,
            ),
        ],
        FilterKind::Bwdif => vec![
            choice("mode", "Mode", BwdifMode::ALL.iter().map(|v| v.as_str())),
            choice(
                "parity",
                "Parité",
                FieldParity::ALL.iter().map(|v| v.as_str()),
            ),
            choice(
                "deint",
                "Images à désentrelacer",
                DeinterlaceSelection::ALL.iter().map(|v| v.as_str()),
            ),
        ],
    }
}

pub(super) fn filter_kinds(capabilities: Result<&FilterCapabilities, String>) -> Filters {
    let error = capabilities.as_ref().err().cloned().unwrap_or_default();
    Filters {
        available: capabilities.is_ok(),
        error: error.clone(),
        entries: Vec::new(),
        kinds: FilterKind::ALL
            .into_iter()
            .map(|kind| {
                let available = capabilities.as_ref().is_ok_and(|caps| caps.supports(kind));
                FilterKindRow {
                    kind: kind.as_str(),
                    label: match kind {
                        FilterKind::Format => "Couleurs SDR",
                        FilterKind::Eq => "Égalisation vidéo · réglages neutres",
                        FilterKind::Unsharp => "Netteté",
                        FilterKind::Hqdn3d => "Réduction du bruit",
                        FilterKind::Bwdif => "Désentrelacement",
                    }
                    .into(),
                    available,
                    reason: if available {
                        String::new()
                    } else if error.is_empty() {
                        "Absent du registre qualifié".into()
                    } else {
                        error.clone()
                    },
                    default_parameters: Parameters::from(&default_filter(kind)),
                    fields: filter_fields(kind),
                }
            })
            .collect(),
    }
}
pub(super) fn treatments(capabilities: &QualifiedTreatmentCapabilities) -> (Filters, Renderer) {
    let filters = filter_kinds(capabilities.filters.as_ref().map_err(ToString::to_string));
    let available = capabilities.renderer.is_ok();
    let error = capabilities
        .renderer
        .as_ref()
        .err()
        .map(ToString::to_string)
        .unwrap_or_default();
    let mut fields = Vec::new();
    for (key, label) in [
        ("scale", "Agrandissement"),
        ("cscale", "Chroma"),
        ("dscale", "Réduction"),
    ] {
        let mut choices = Vec::new();
        if key != "scale" {
            choices.push(Choice {
                key: "auto".into(),
                label: "Auto · hériter".into(),
                eligible: available,
                reason: error.clone(),
            });
        }
        for kernel in ScalerKernel::ALL {
            let mut values = RendererSettings::default().values();
            match key {
                "scale" => values.scale = *kernel,
                "cscale" => values.cscale = ScalerChoice::Kernel(*kernel),
                _ => values.dscale = ScalerChoice::Kernel(*kernel),
            }
            let settings = RendererSettings::new(values).expect("validated kernel");
            let result = capabilities
                .renderer
                .as_ref()
                .map_err(ToString::to_string)
                .and_then(|caps| {
                    render_catalog::compile_renderer(&settings, caps)
                        .map(|_| ())
                        .map_err(|e| e.to_string())
                });
            choices.push(Choice {
                key: kernel.as_str().into(),
                label: kernel.as_str().into(),
                eligible: result.is_ok(),
                reason: result.err().unwrap_or_default(),
            });
        }
        fields.push(RenderFieldRow {
            key,
            label: label.into(),
            field_type: "choice",
            choices,
            min: None,
            max: None,
            step: None,
            notice: String::new(),
        });
    }
    fields.push(RenderFieldRow {
        key: "dither",
        label: "Dithering".into(),
        field_type: "choice",
        choices: DitherMode::ALL
            .iter()
            .map(|mode| Choice {
                key: mode.as_str().into(),
                label: mode.as_str().into(),
                eligible: available,
                reason: error.clone(),
            })
            .collect(),
        min: None,
        max: None,
        step: None,
        notice: String::new(),
    });
    for (key, label, field_type, min, max, step) in [
        (
            "scale_antiring",
            "Anti-ringing",
            "number",
            Some(0.0),
            Some(1.0),
            Some(0.05),
        ),
        ("deband", "Debanding", "boolean", None, None, None),
        (
            "deband_iterations",
            "Itérations",
            "integer",
            Some(0.0),
            Some(16.0),
            Some(1.0),
        ),
        (
            "deband_grain",
            "Grain",
            "number",
            Some(0.0),
            Some(4096.0),
            Some(1.0),
        ),
        (
            "interpolation",
            "Interpolation",
            "boolean",
            None,
            None,
            None,
        ),
    ] {
        fields.push(RenderFieldRow { key, label: label.into(), field_type, choices: Vec::new(), min, max, step, notice: if key == "interpolation" { "Réglage conservé et appliqué, mais inactif avec la synchronisation actuelle à faible latence.".into() } else { String::new() } });
    }
    (filters, Renderer { available, error, fields, notice: "Interpolation conservée mais inactive avec la synchronisation actuelle à faible latence.".into() })
}
pub(super) fn mutate_renderer(values: RendererValues) -> Result<RendererSettings, String> {
    RendererSettings::new(values).map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offered_filter_fields_cover_exact_payloads_and_valid_domain_boundaries() {
        let catalog = filter_kinds(Err("unavailable fixture".into()));
        let mut counts = [0, 0, 0];
        for kind in catalog.kinds {
            let parameters = serde_json::to_value(kind.default_parameters).unwrap();
            let mut control_keys = kind
                .fields
                .iter()
                .map(|field| field.key)
                .collect::<Vec<_>>();
            control_keys.sort_unstable();
            let mut payload_keys = parameters
                .as_object()
                .unwrap()
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>();
            payload_keys.sort_unstable();
            assert_eq!(
                control_keys, payload_keys,
                "{} has exactly one control per payload field",
                kind.kind
            );
            for field in kind.fields {
                assert!(parameters.get(field.key).is_some(), "{}", field.key);
                let decode = |value: serde_json::Value| {
                    let mut edited = parameters.clone();
                    edited[field.key] = value;
                    serde_json::from_value::<Filter>(serde_json::json!({
                        "kind": kind.kind, "parameters": edited,
                    }))
                };
                match field.field_type {
                    "choice" => {
                        counts[2] += 1;
                        assert!(!field.choices.is_empty());
                        assert_eq!((field.min, field.max, field.step), (None, None, None));
                        for choice in field.choices {
                            assert!(choice.eligible && choice.reason.is_empty());
                            assert!(decode(choice.key.into()).is_ok());
                        }
                        assert!(decode("not-a-domain-token".into()).is_err());
                    }
                    "number" | "integer" => {
                        let integer = field.field_type == "integer";
                        counts[usize::from(integer)] += 1;
                        assert!(field.choices.is_empty());
                        let min = field.min.unwrap();
                        let max = field.max.unwrap();
                        for value in [min, max] {
                            let value = if integer {
                                serde_json::json!(value as u32)
                            } else {
                                serde_json::json!(value)
                            };
                            assert!(decode(value).is_ok(), "{}: {}", kind.kind, field.key);
                        }
                        if integer {
                            assert_eq!(field.step, Some(2.0));
                            assert!(decode(serde_json::json!(min as u32 + 1)).is_err());
                            assert!(decode(serde_json::json!(min + 0.5)).is_err());
                        } else {
                            assert_eq!(field.step, None, "continuous values are not quantized");
                            assert!(decode(serde_json::json!(min - 0.0001)).is_err());
                            assert!(decode(serde_json::json!(max + 0.0001)).is_err());
                        }
                    }
                    other => panic!("unexpected filter field type {other}"),
                }
            }
        }
        assert_eq!(counts, [15, 6, 6]);
    }
    #[test]
    fn renderer_invalid_values_never_replace_the_typed_value() {
        for value in [f64::NAN, f64::INFINITY, -0.1, 1.1] {
            let mut values = RendererSettings::default().values();
            values.scale_antiring = value;
            assert!(mutate_renderer(values).is_err());
        }
    }
}
