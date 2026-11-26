//! PCM to DSD as the app handles it: the setting's names on the wire, the phone limit, what
//! can run for one playback (and why not, when it can't), and how to step down when the
//! processor can't keep up. The encoder itself is the engine's (`hifi_engine::dsp::dsd_encode`).
//!
//! `DspSettings.dsd_out` in the app's settings is what the user *wants*. What a playback runs
//! with is that wish resolved for the output and the playback ([`resolve`]): only the exclusive
//! playback of a DAC that takes DSD natively (its driver says so), or that the user said takes
//! DoP, at a rate it can carry, ever gets one.

use hifi_engine::dsd;
use hifi_engine::dsp::dsd_encode::{DsdOut, DsdRate, ModOrder};
use hifi_engine::dsp::upsample::FilterSpec;
use hifi_engine::output;
use hifi_engine::player::Output;
use serde::Serialize;

/// Phones get DSD64 only: the higher rates take a core's worth of battery or more.
const PHONE: bool = cfg!(mobile);

/// The setting the UI sent: the rate as a multiple of 44.1 kHz (64, 128 or 256; anything else:
/// off) and the modulator's order (5 or 7; anything else: 7).
pub fn parse(multiple: Option<u32>, order: Option<u32>) -> Option<DsdOut> {
    parse_on(multiple, order, PHONE)
}

/// [`parse`], on a phone or not.
fn parse_on(multiple: Option<u32>, order: Option<u32>, phone: bool) -> Option<DsdOut> {
    let rate = DsdRate::from_multiple(multiple?)?;
    let order = if order == Some(5) { ModOrder::Fifth } else { ModOrder::Seventh };
    Some(DsdOut { rate: if phone { DsdRate::Dsd64 } else { rate }, order })
}

/// The rate (64, 128, 256) and order (5, 7) as the UI names them.
pub fn names(out: Option<DsdOut>) -> (Option<u32>, Option<u32>) {
    match out {
        Some(o) => (Some(o.rate.multiple()), Some(o.order.get() as u32)),
        None => (None, None),
    }
}

/// Why DSD output was asked for and PCM plays. Sent as a code (shaped like `DsdNote`): the UI
/// words it in the user's language.
#[derive(Serialize, Clone, Debug, PartialEq)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum DsdOutNote {
    /// DSD output needs exclusive access to the device.
    Exclusive,
    /// The device's DoP switch is off: the user hasn't said this DAC decodes DoP, and one that
    /// doesn't would play it as noise. (Never for a DAC that takes DSD natively.)
    Device,
    /// Something system-wide blocks DoP (iOS: the volume is below maximum).
    Volume,
    /// A network player: it gets PCM.
    Network,
    /// The device doesn't run at the PCM rate DoP needs (`khz`) even for the lowest DSD rate
    /// (`dsd`).
    Rate { device: String, dsd: String, khz: f64 },
    /// The device was opened for DoP at `khz` (`dsd`) and failed to start; PCM plays.
    Refused { device: String, dsd: String, khz: f64 },
    /// The device takes DSD natively, but only at rates above the one asked for (`dsd`), and
    /// DoP doesn't carry any at or below it.
    NativeRate { device: String, dsd: String },
    /// The device was opened for native DSD at `dsd` and failed to start; PCM plays.
    RefusedNative { device: String, dsd: String },
}

impl DsdOutNote {
    /// `device` failed to start with DSD output `asked`, for whatever reason (the engine's
    /// error may be about DoP, the format, the rate or the device not opening): what was
    /// asked for, not what the error says.
    pub fn refused(device: &str, asked: DsdOut) -> Self {
        let (dsd, khz) = Self::wire(asked.rate);
        Self::Refused { device: device.trim().to_string(), dsd, khz }
    }

    /// [`refused`](Self::refused), for a device that takes the rate asked for natively (the
    /// engine tried that first).
    pub fn refused_as(device: &str, asked: DsdOut, native: bool) -> Self {
        if !native {
            return Self::refused(device, asked);
        }
        Self::RefusedNative { device: device.trim().to_string(), dsd: dsd::dsd_name(asked.rate.hz()) }
    }

    fn rate(device: &str, rate: DsdRate) -> Self {
        let (dsd, khz) = Self::wire(rate);
        Self::Rate { device: device.to_string(), dsd, khz }
    }

    /// The rate's name ("DSD64") and the PCM rate DoP carries it at, in kHz.
    fn wire(rate: DsdRate) -> (String, f64) {
        (dsd::dsd_name(rate.hz()), rate.wire_rate() as f64 / 1000.0)
    }
}

/// What decides whether DSD output can run for one playback.
pub struct Facts<'a> {
    /// A network player (UPnP, Chromecast).
    pub network: bool,
    /// The playback has the device to itself.
    pub exclusive: bool,
    /// The user's per-device switch: this DAC decodes DoP.
    pub dop: bool,
    /// Something system-wide blocks DoP now (`output::dop_blocker`).
    pub blocked: bool,
    /// The device's name, for the note.
    pub device: &'a str,
    /// The device can carry DoP at this DSD rate (its clock and formats).
    pub carries: &'a dyn Fn(DsdRate) -> bool,
    /// The device takes this DSD rate natively (its driver declares a DSD format for it):
    /// no DoP switch needed, and nothing that blocks DoP matters.
    pub native: &'a dyn Fn(DsdRate) -> bool,
}

/// What runs of `want` under `facts`, and why nothing does when something was wanted. The
/// rate is the highest the device carries at or below the wish, with the same order — a rate
/// carries natively, or as DoP where the switch is on and nothing blocks it; the signal path
/// then shows it, and the UI compares it with the wish (no note).
pub fn decide(want: Option<DsdOut>, facts: &Facts) -> (Option<DsdOut>, Option<DsdOutNote>) {
    let Some(want) = want else { return (None, None) };
    if facts.network {
        return (None, Some(DsdOutNote::Network));
    }
    if !facts.exclusive {
        return (None, Some(DsdOutNote::Exclusive));
    }
    let dop = facts.dop && !facts.blocked;
    let carries = |r: DsdRate| (facts.native)(r) || (dop && (facts.carries)(r));
    if let Some(rate) = DsdRate::ALL.into_iter().rev().filter(|r| r.multiple() <= want.rate.multiple()).find(|r| carries(*r)) {
        return (Some(DsdOut { rate, order: want.order }), None);
    }
    let note = if DsdRate::ALL.into_iter().any(|r| (facts.native)(r)) {
        DsdOutNote::NativeRate { device: facts.device.to_string(), dsd: dsd::dsd_name(want.rate.hz()) }
    } else if !facts.dop {
        DsdOutNote::Device
    } else if facts.blocked {
        DsdOutNote::Volume
    } else {
        DsdOutNote::rate(facts.device, DsdRate::Dsd64)
    };
    (None, Some(note))
}

/// [`decide`] for a playback on `device`: `exclusive` as it is asked for, `dop` the device's
/// DoP switch.
pub fn resolve(want: Option<DsdOut>, device: &Output, exclusive: bool, dop: bool) -> (Option<DsdOut>, Option<DsdOutNote>) {
    if want.is_none() {
        return (None, None);
    }
    let carries = |r: DsdRate| device.supports_dop(r.hz());
    let native = |r: DsdRate| device.supports_native_dsd(r.hz());
    let facts = Facts {
        network: device.is_network(),
        exclusive,
        dop,
        blocked: output::dop_blocker().is_some(),
        device: device.name().trim(),
        carries: &carries,
        native: &native,
    };
    decide(want, &facts)
}

/// An automatic step down of the DSD output, for the UI to show and adopt. `seq` grows with
/// every step; `dsd_out: None` = DSD output is now off (PCM plays). `device_key`: the output
/// whose settings it changed (see `device_key`), the one that played.
#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct DsdStep {
    pub seq: u64,
    pub dsd_out: Option<u32>,
    pub order: Option<u32>,
    pub device_key: String,
}

/// One step down, when the processor can't keep up.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Step {
    /// The DSD output becomes this (None: off).
    Dsd(Option<DsdOut>),
    /// The upsampling filter becomes this (None: the standard resampler).
    Filter(Option<FilterSpec>),
}

/// The next step down from what plays (`dsd_out` and the long filter, as the signal path has
/// them): the DSD output gets lighter while it can (256 → 128 → 64, then the fifth order),
/// then the long filter gets shorter down to the standard one, then the DSD output goes off.
/// At the higher rates the encoder is the heavier part, so it gives way first; DSD output as
/// such is the bigger choice of the two (a DAC fed DSD rather than PCM), so it goes last.
pub fn next_step(dsd_out: Option<DsdOut>, filter: Option<FilterSpec>) -> Option<Step> {
    if let Some(lighter) = dsd_out.and_then(DsdOut::lighter) {
        return Some(Step::Dsd(Some(lighter)));
    }
    if let Some(f) = filter {
        return Some(Step::Filter(f.shorter()));
    }
    dsd_out.map(|_| Step::Dsd(None))
}

/// The settings still ask for what plays (`playing`): a wish of its order at its rate or
/// higher (a device that can't go as high plays lower). Otherwise they changed since it was
/// measured, and the change restarts playback itself.
pub fn asks_for(wish: Option<DsdOut>, playing: DsdOut) -> bool {
    wish.is_some_and(|w| w.order == playing.order && w.rate.multiple() >= playing.rate.multiple())
}

#[cfg(test)]
mod tests {
    use super::*;
    use hifi_engine::dsp::upsample::FilterKind;

    fn out(m: u32, order: u32) -> DsdOut {
        DsdOut { rate: DsdRate::from_multiple(m).unwrap(), order: ModOrder::from_number(order).unwrap() }
    }

    #[test]
    fn the_ui_names_parse() {
        assert_eq!(parse_on(Some(64), Some(5), false), Some(out(64, 5)));
        assert_eq!(parse_on(Some(128), Some(7), false), Some(out(128, 7)));
        assert_eq!(parse_on(Some(256), Some(7), false), Some(out(256, 7)));
        // The order defaults to the seventh, also for one not offered.
        assert_eq!(parse_on(Some(128), None, false), Some(out(128, 7)));
        assert_eq!(parse_on(Some(64), Some(6), false), Some(out(64, 7)));
        // Off: nothing asked, or a rate not offered.
        assert_eq!(parse_on(None, Some(5), false), None);
        for m in [0, 32, 100, 512, u32::MAX] {
            assert_eq!(parse_on(Some(m), Some(7), false), None, "{m}");
        }
        assert_eq!(parse(Some(64), Some(5)), Some(out(64, 5)));
        assert_eq!(parse(Some(256), None).map(|o| o.rate), Some(if PHONE { DsdRate::Dsd64 } else { DsdRate::Dsd256 }));
    }

    #[test]
    fn phones_get_dsd64_only() {
        assert_eq!(parse_on(Some(256), Some(7), true), Some(out(64, 7)));
        assert_eq!(parse_on(Some(128), Some(5), true), Some(out(64, 5)));
        assert_eq!(parse_on(Some(64), None, true), Some(out(64, 7)));
        assert_eq!(parse_on(Some(512), None, true), None);
        assert_eq!(parse_on(None, None, true), None);
    }

    #[test]
    fn settings_get_their_ui_names_back() {
        assert_eq!(names(None), (None, None));
        assert_eq!(names(Some(out(256, 5))), (Some(256), Some(5)));
        for m in [64, 128, 256] {
            for o in [5, 7] {
                assert_eq!(parse_on(names(Some(out(m, o))).0, names(Some(out(m, o))).1, false), Some(out(m, o)));
            }
        }
    }

    /// Facts under which everything can run, up to DSD `max` (0: none).
    fn facts(max: u32) -> (bool, bool, bool, bool, u32) {
        (false, true, true, false, max)
    }

    fn run(want: Option<DsdOut>, (network, exclusive, dop, blocked, max): (bool, bool, bool, bool, u32)) -> (Option<DsdOut>, Option<DsdOutNote>) {
        let carries = move |r: DsdRate| r.multiple() <= max;
        decide(want, &Facts { network, exclusive, dop, blocked, device: "DAC", carries: &carries, native: &|_| false })
    }

    /// [`run`] on a device that takes DSD natively at the multiples `from..=to` (none when
    /// `from` > `to`), DoP up to `dop_max` (0: none).
    fn run_native(want: Option<DsdOut>, exclusive: bool, dop: bool, blocked: bool, dop_max: u32, (from, to): (u32, u32)) -> (Option<DsdOut>, Option<DsdOutNote>) {
        let carries = move |r: DsdRate| r.multiple() <= dop_max;
        let native = move |r: DsdRate| (from..=to).contains(&r.multiple());
        decide(want, &Facts { network: false, exclusive, dop, blocked, device: "DAC", carries: &carries, native: &native })
    }

    #[test]
    fn what_runs_for_a_playback() {
        // Nothing wanted: nothing to say, whatever the device.
        assert_eq!(run(None, facts(256)), (None, None));
        assert_eq!(run(None, (true, false, false, true, 0)), (None, None));
        let want = Some(out(256, 7));
        // Everything in place: as asked.
        assert_eq!(run(want, facts(256)), (want, None));
        // Each reason, in the order they are checked.
        assert_eq!(run(want, (true, false, false, true, 0)), (None, Some(DsdOutNote::Network)));
        assert_eq!(run(want, (false, false, false, true, 0)), (None, Some(DsdOutNote::Exclusive)));
        assert_eq!(run(want, (false, true, false, true, 0)), (None, Some(DsdOutNote::Device)));
        assert_eq!(run(want, (false, true, true, true, 256)), (None, Some(DsdOutNote::Volume)));
        // Not even DSD64: the rate DoP needs for it.
        let rate = DsdOutNote::Rate { device: "DAC".into(), dsd: "DSD64".into(), khz: 176.4 };
        assert_eq!(run(want, facts(0)), (None, Some(rate.clone())));
        assert_eq!(run(Some(out(64, 5)), facts(0)), (None, Some(rate)));
    }

    #[test]
    fn the_highest_rate_the_device_carries_at_or_below_the_wish() {
        assert_eq!(run(Some(out(256, 7)), facts(128)), (Some(out(128, 7)), None));
        assert_eq!(run(Some(out(256, 5)), facts(64)), (Some(out(64, 5)), None));
        assert_eq!(run(Some(out(128, 5)), facts(64)), (Some(out(64, 5)), None));
        // Never above the wish.
        assert_eq!(run(Some(out(64, 7)), facts(256)), (Some(out(64, 7)), None));
        assert_eq!(run(Some(out(128, 7)), facts(256)), (Some(out(128, 7)), None));
        // A device that carries DSD128 but not DSD64 (a clock without 176.4 kHz): DSD128.
        let carries = |r: DsdRate| r == DsdRate::Dsd128;
        let f = Facts { network: false, exclusive: true, dop: true, blocked: false, device: "DAC", carries: &carries, native: &|_| false };
        assert_eq!(decide(Some(out(256, 7)), &f), (Some(out(128, 7)), None));
        assert_eq!(decide(Some(out(64, 7)), &f).0, None);
    }

    #[test]
    fn a_native_dac_needs_no_dop_switch() {
        let want = Some(out(256, 7));
        // Native up to DSD256, the DoP switch off (and DoP carrying nothing): as asked.
        assert_eq!(run_native(want, true, false, false, 0, (64, 256)), (want, None));
        // What blocks DoP doesn't touch native DSD.
        assert_eq!(run_native(want, true, false, true, 0, (64, 256)), (want, None));
        assert_eq!(run_native(want, true, true, true, 256, (64, 256)), (want, None));
        // It still needs exclusive access, and a network player never gets it.
        assert_eq!(run_native(want, false, false, false, 0, (64, 256)), (None, Some(DsdOutNote::Exclusive)));
        // Native only up to DSD128: DSD128, unless DoP (switched on) goes higher.
        assert_eq!(run_native(want, true, false, false, 0, (64, 128)), (Some(out(128, 7)), None));
        assert_eq!(run_native(want, true, true, false, 256, (64, 128)), (want, None));
        // ...but not with DoP off, or blocked.
        assert_eq!(run_native(want, true, false, false, 256, (64, 128)), (Some(out(128, 7)), None));
        assert_eq!(run_native(want, true, true, true, 256, (64, 128)), (Some(out(128, 7)), None));
        // Never above the wish.
        assert_eq!(run_native(Some(out(64, 5)), true, false, false, 0, (64, 256)), (Some(out(64, 5)), None));
    }

    #[test]
    fn a_native_dac_only_above_the_wish() {
        // Native from DSD128 up, DSD64 asked for, no DoP: says so (not "switch DoP on").
        let note = DsdOutNote::NativeRate { device: "DAC".into(), dsd: "DSD64".into() };
        assert_eq!(run_native(Some(out(64, 7)), true, false, false, 0, (128, 256)), (None, Some(note.clone())));
        assert_eq!(run_native(Some(out(64, 7)), true, true, false, 0, (128, 256)), (None, Some(note)));
        // DoP switched on and carrying DSD64: DoP.
        assert_eq!(run_native(Some(out(64, 7)), true, true, false, 64, (128, 256)), (Some(out(64, 7)), None));
        // No native rate at all: as before.
        assert_eq!(run_native(Some(out(64, 7)), true, false, false, 64, (1, 0)), (None, Some(DsdOutNote::Device)));
    }

    #[test]
    fn refused_names_what_was_asked_for() {
        let note = |m, o| DsdOutNote::refused(" DAC ", out(m, o));
        assert_eq!(note(64, 7), DsdOutNote::Refused { device: "DAC".into(), dsd: "DSD64".into(), khz: 176.4 });
        assert_eq!(note(128, 5), DsdOutNote::Refused { device: "DAC".into(), dsd: "DSD128".into(), khz: 352.8 });
        assert_eq!(note(256, 7), DsdOutNote::Refused { device: "DAC".into(), dsd: "DSD256".into(), khz: 705.6 });
        assert_eq!(DsdOutNote::refused_as(" DAC ", out(128, 7), false), note(128, 7));
        assert_eq!(DsdOutNote::refused_as(" DAC ", out(128, 7), true), DsdOutNote::RefusedNative { device: "DAC".into(), dsd: "DSD128".into() });
    }

    #[test]
    fn notes_on_the_wire() {
        let notes = [
            DsdOutNote::Exclusive,
            DsdOutNote::Device,
            DsdOutNote::Volume,
            DsdOutNote::Network,
            DsdOutNote::rate("DAC", DsdRate::Dsd64),
            DsdOutNote::refused("DAC", out(256, 7)),
            DsdOutNote::NativeRate { device: "DAC".into(), dsd: "DSD64".into() },
            DsdOutNote::refused_as("DAC", out(64, 7), true),
        ];
        let json: Vec<String> = notes.iter().map(|n| serde_json::to_string(n).unwrap()).collect();
        for j in &json {
            println!("{j}");
        }
        assert_eq!(json[0], r#"{"kind":"exclusive"}"#);
        assert_eq!(json[1], r#"{"kind":"device"}"#);
        assert_eq!(json[2], r#"{"kind":"volume"}"#);
        assert_eq!(json[3], r#"{"kind":"network"}"#);
        assert_eq!(json[4], r#"{"kind":"rate","device":"DAC","dsd":"DSD64","khz":176.4}"#);
        assert_eq!(json[5], r#"{"kind":"refused","device":"DAC","dsd":"DSD256","khz":705.6}"#);
        assert_eq!(json[6], r#"{"kind":"nativeRate","device":"DAC","dsd":"DSD64"}"#);
        assert_eq!(json[7], r#"{"kind":"refusedNative","device":"DAC","dsd":"DSD64"}"#);
        let step = serde_json::to_string(&DsdStep { seq: 2, dsd_out: Some(64), order: Some(5), device_key: "dac".into() }).unwrap();
        println!("{step}");
        assert_eq!(step, r#"{"seq":2,"dsdOut":64,"order":5,"deviceKey":"dac"}"#);
    }

    #[test]
    fn the_steps_down_without_a_long_filter() {
        // 256/7 → 128/7 → 64/7 → 64/5 → off, each from what plays.
        let mut playing = Some(out(256, 7));
        let mut seen = Vec::new();
        while let Some(step) = next_step(playing, None) {
            let Step::Dsd(next) = step else { panic!("a filter step without a filter") };
            seen.push(names(next));
            playing = next;
        }
        assert_eq!(seen, [(Some(128), Some(7)), (Some(64), Some(7)), (Some(64), Some(5)), (None, None)]);
        // Nothing to step down from.
        assert_eq!(next_step(None, None), None);
    }

    #[test]
    fn the_steps_down_with_a_long_filter() {
        let long = FilterSpec { kind: FilterKind::Linear, taps: 1 << 20 };
        let short = FilterSpec { kind: FilterKind::Linear, taps: 1 << 16 };
        // The DSD output gets lighter first, then the filter shorter, then the output goes off.
        let (mut dsd, mut filter) = (Some(out(128, 7)), Some(long));
        let mut seen = Vec::new();
        while let Some(step) = next_step(dsd, filter) {
            seen.push(step);
            match step {
                Step::Dsd(next) => dsd = next,
                Step::Filter(next) => filter = next,
            }
        }
        assert_eq!(
            seen,
            [
                Step::Dsd(Some(out(64, 7))),
                Step::Dsd(Some(out(64, 5))),
                Step::Filter(Some(short)),
                Step::Filter(None),
                Step::Dsd(None),
            ]
        );
        // Without DSD output: the filter alone, as before.
        assert_eq!(next_step(None, Some(long)), Some(Step::Filter(Some(short))));
        assert_eq!(next_step(None, Some(short)), Some(Step::Filter(None)));
    }

    #[test]
    fn the_settings_still_ask_for_what_plays() {
        assert!(asks_for(Some(out(128, 7)), out(128, 7)));
        // Wished higher, the device plays lower.
        assert!(asks_for(Some(out(256, 7)), out(64, 7)));
        assert!(!asks_for(Some(out(64, 7)), out(128, 7)));
        assert!(!asks_for(Some(out(128, 5)), out(128, 7)));
        assert!(!asks_for(None, out(64, 5)));
    }
}
