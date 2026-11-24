//! The long upsampling filters as the app handles them: their names on the wire, the phone
//! limit, how much earlier a track is lined up behind one, and when to fall back to a cheaper
//! one because the processor can't keep up. The filters themselves are the engine's
//! (`hifi_engine::dsp::upsample`).

use std::sync::Mutex;

use hifi_engine::dsp::upsample::{self, FilterKind, FilterSpec};
use serde::Serialize;

/// The two lengths offered (taps at the doubled rate).
pub const SHORT_TAPS: usize = 1 << 16;
pub const LONG_TAPS: usize = 1 << 20;

/// Phones get the short length only: 2^20 taps cost tens of MB and a core's worth of battery.
const PHONE: bool = cfg!(mobile);

/// The filter the UI asked for: "linear", "minimum" or "apodizing" (anything else: the
/// standard resampler), 65,536 or 1,048,576 taps (default the shorter; ignored for apodizing).
pub fn parse(kind: Option<&str>, taps: Option<u32>) -> Option<FilterSpec> {
    let kind = match kind? {
        "linear" => FilterKind::Linear,
        "minimum" => FilterKind::Minimum,
        "apodizing" => FilterKind::Apodizing,
        _ => return None,
    };
    let taps = if taps.is_some_and(|t| t as usize >= LONG_TAPS) { LONG_TAPS } else { SHORT_TAPS };
    Some(limit(FilterSpec { kind, taps }, PHONE).effective())
}

/// On a phone, no longer than [`SHORT_TAPS`].
fn limit(spec: FilterSpec, phone: bool) -> FilterSpec {
    if phone { FilterSpec { taps: spec.taps.min(SHORT_TAPS), ..spec } } else { spec }
}

/// The filter as the UI names it, and its length (None for apodizing, which has only one).
pub fn names(spec: Option<FilterSpec>) -> (Option<String>, Option<u32>) {
    let Some(s) = spec.map(FilterSpec::effective) else { return (None, None) };
    match s.kind {
        FilterKind::Linear => (Some("linear".into()), Some(s.taps as u32)),
        FilterKind::Minimum => (Some("minimum".into()), Some(s.taps as u32)),
        FilterKind::Apodizing => (Some("apodizing".into()), None),
    }
}

/// An automatic step down to a cheaper filter, for the UI to show and adopt. `seq` grows with
/// every step; `filter: None` = now on the standard resampler. `device_key`: the output whose
/// settings it changed (see `device_key`), the one that played.
#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct FilterStep {
    pub seq: u64,
    pub filter: Option<String>,
    pub taps: Option<u32>,
    pub device_key: String,
}

/// Seconds the next track is lined up earlier than usual behind a long filter. The render
/// thread runs ahead of what is heard by the input the filter holds back before its first
/// output, so a track lined up at the usual time could arrive after the current one has been
/// fully rendered.
pub fn line_up_extra(startup_frames: Option<usize>, source_rate: u32) -> f64 {
    match startup_frames {
        Some(f) if source_rate > 0 => f as f64 / source_rate as f64,
        _ => 0.0,
    }
}

/// The input frames `spec` holds back (see `DoublerFilter::startup_frames`). Remembered for
/// the last spec asked: called every tick of the queue thread, and a design not in the
/// engine's cache any more would take a while.
pub fn startup_frames(spec: FilterSpec) -> usize {
    static LAST: Mutex<Option<(FilterSpec, usize)>> = Mutex::new(None);
    let spec = spec.effective();
    if let Some((s, f)) = *LAST.lock().unwrap() {
        if s == spec {
            return f;
        }
    }
    let f = upsample::filter(spec).startup_frames();
    *LAST.lock().unwrap() = Some((spec, f));
    f
}

/// The render thread's share of a core from which the filter is stepped down: well before
/// 1.0 (falling behind), with room for whatever else the machine does meanwhile.
pub const MAX_LOAD: f32 = 0.8;
/// [`MAX_LOAD`], or in a debug build what `BITSPRING_STEP_DOWN_AT` says instead (0.0001: any
/// measured load is too much), to watch the step down on a machine that keeps up.
fn max_load() -> f32 {
    static AT: std::sync::OnceLock<f32> = std::sync::OnceLock::new();
    *AT.get_or_init(|| {
        let asked = cfg!(debug_assertions).then(|| std::env::var("BITSPRING_STEP_DOWN_AT").ok()?.parse().ok()).flatten();
        asked.unwrap_or(MAX_LOAD)
    })
}

/// Dropouts on one playback from which the filter is stepped down.
pub const MAX_UNDERRUNS: u64 = 3;

/// Whether the processor is losing the race and the long filter should give way to a
/// cheaper one. `load`: the render thread's share of a core (None or 0: not measured yet);
/// `underruns`: dropouts since this playback started.
pub fn should_step_down(load: Option<f32>, underruns: u64, long_filter: bool) -> bool {
    long_filter && (load.is_some_and(|l| l >= max_load()) || underruns >= MAX_UNDERRUNS)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(kind: FilterKind, taps: usize) -> FilterSpec {
        FilterSpec { kind, taps }
    }

    #[test]
    fn the_ui_names_parse_into_specs() {
        assert_eq!(parse(Some("linear"), Some(65_536)), Some(spec(FilterKind::Linear, SHORT_TAPS)));
        assert_eq!(parse(Some("minimum"), None), Some(spec(FilterKind::Minimum, SHORT_TAPS)));
        assert_eq!(parse(Some("apodizing"), Some(1_048_576)), Some(spec(FilterKind::Apodizing, upsample::APODIZING_TAPS)));
        assert_eq!(parse(Some("standard"), Some(65_536)), None);
        assert_eq!(parse(Some(""), None), None);
        assert_eq!(parse(None, Some(1_048_576)), None);
        // Odd lengths land on one of the two offered.
        assert_eq!(parse(Some("linear"), Some(1_000)).map(|s| s.taps), Some(SHORT_TAPS));
        assert_eq!(parse(Some("linear"), Some(u32::MAX)).map(|s| s.taps), Some(if PHONE { SHORT_TAPS } else { LONG_TAPS }));
        if !PHONE {
            assert_eq!(parse(Some("linear"), Some(1_048_576)), Some(spec(FilterKind::Linear, LONG_TAPS)));
            assert_eq!(parse(Some("minimum"), Some(1_048_576)), Some(spec(FilterKind::Minimum, LONG_TAPS)));
        }
    }

    #[test]
    fn phones_get_the_short_length_only() {
        assert_eq!(limit(spec(FilterKind::Linear, LONG_TAPS), true), spec(FilterKind::Linear, SHORT_TAPS));
        assert_eq!(limit(spec(FilterKind::Minimum, LONG_TAPS), true), spec(FilterKind::Minimum, SHORT_TAPS));
        assert_eq!(limit(spec(FilterKind::Linear, SHORT_TAPS), true), spec(FilterKind::Linear, SHORT_TAPS));
        assert_eq!(limit(spec(FilterKind::Linear, LONG_TAPS), false), spec(FilterKind::Linear, LONG_TAPS));
    }

    #[test]
    fn specs_get_their_ui_names_back() {
        assert_eq!(names(None), (None, None));
        assert_eq!(names(Some(spec(FilterKind::Linear, LONG_TAPS))), (Some("linear".into()), Some(1_048_576)));
        assert_eq!(names(Some(spec(FilterKind::Minimum, SHORT_TAPS))), (Some("minimum".into()), Some(65_536)));
        assert_eq!(names(Some(spec(FilterKind::Apodizing, SHORT_TAPS))), (Some("apodizing".into()), None));
        for (kind, taps) in [("linear", 65_536), ("minimum", 65_536)] {
            assert_eq!(names(parse(Some(kind), Some(taps))), (Some(kind.into()), Some(taps)));
        }
    }

    /// What the UI reads (`FilterStep` in api.ts).
    #[test]
    fn a_filter_step_on_the_wire() {
        let step = FilterStep { seq: 3, filter: Some("linear".into()), taps: Some(65_536), device_key: "android:Usb:FiiO Q7".into() };
        assert_eq!(serde_json::to_string(&step).unwrap(), r#"{"seq":3,"filter":"linear","taps":65536,"deviceKey":"android:Usb:FiiO Q7"}"#);
        let standard = FilterStep { seq: 4, filter: None, taps: None, device_key: "dac".into() };
        assert_eq!(serde_json::to_string(&standard).unwrap(), r#"{"seq":4,"filter":null,"taps":null,"deviceKey":"dac"}"#);
    }

    #[test]
    fn a_long_filter_lines_the_next_track_up_earlier() {
        assert_eq!(line_up_extra(None, 44_100), 0.0);
        assert_eq!(line_up_extra(Some(278_528), 0), 0.0);
        // Linear 2^20 at 44.1 kHz: about 6.3 s more; 2^16 well under half a second.
        assert!((line_up_extra(Some(278_528), 44_100) - 6.316).abs() < 0.001);
        assert!((line_up_extra(Some(17_408), 44_100) - 0.395).abs() < 0.001);
        assert!((line_up_extra(Some(278_528), 96_000) - 2.901).abs() < 0.001);
    }

    #[test]
    fn the_filter_held_back_is_the_engines() {
        let linear = spec(FilterKind::Linear, SHORT_TAPS);
        assert_eq!(startup_frames(linear), upsample::filter(linear).startup_frames());
        // Asked again: the same answer, from memory.
        assert_eq!(startup_frames(linear), 17_408);
    }

    #[test]
    fn the_filter_steps_down_when_the_processor_cannot_keep_up() {
        // Comfortable: stays.
        assert!(!should_step_down(Some(0.02), 0, true));
        assert!(!should_step_down(Some(0.79), 2, true));
        // Not measured yet.
        assert!(!should_step_down(None, 0, true));
        assert!(!should_step_down(Some(0.0), 0, true));
        // Close to a full core, or dropouts already heard.
        assert!(should_step_down(Some(0.8), 0, true));
        assert!(should_step_down(Some(1.7), 0, true));
        assert!(should_step_down(Some(0.1), 3, true));
        assert!(should_step_down(None, 12, true));
        // Nothing to step down from: the standard filter, or none at all.
        assert!(!should_step_down(Some(1.2), 9, false));
    }
}
