//! Equaliser presets in and out of other people's formats.
//!
//! Reads (the format is recognised from the content, not the file name):
//! * **Equalizer APO** configs, which also covers AutoEQ's ParametricEQ.txt / FixedBandEQ.txt,
//!   REW's filter export, Peace and anything else that writes `Filter 1: ON PK Fc …` lines —
//!   with `Preamp`, `Channel: L/R` sections, `BW Oct` widths and `GraphicEQ` lines;
//! * **GraphicEQ** (AutoEQ GraphicEQ.txt, Wavelet, JamesDSP): `GraphicEQ: 20 -1.2; 21 -1.1; …`;
//! * **EasyEffects** equaliser presets (JSON, split channels included);
//! * **foobar2000** `.feq` (18 graphic bands);
//! * **CSV / TSV / text** frequency–gain tables (AutoEQ's result CSVs: the `equalization`
//!   column);
//! * our own JSON (one preset or a whole backup).
//!
//! Writes APO (the lingua franca: Peace, Wavelet's APO import, AutoEQ, REW), GraphicEQ,
//! EasyEffects JSON, a frequency-response CSV and our JSON, which is the only lossless one.

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::eq::{Band, Channels, EqPreset, FilterKind, Phase};
use crate::UserError;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum EqFormat {
    /// Equalizer APO / AutoEQ ParametricEQ / Peace / REW — `.txt`.
    Apo,
    /// AutoEQ GraphicEQ / Wavelet — `.txt`.
    GraphicEq,
    EasyEffects,
    /// Frequency response, one row per 1/24 octave.
    Csv,
    /// Lossless.
    Hifi,
}

impl EqFormat {
    pub fn extension(self) -> &'static str {
        match self {
            Self::Apo | Self::GraphicEq => "txt",
            Self::EasyEffects | Self::Hifi => "json",
            Self::Csv => "csv",
        }
    }
}

/// Rate the exported curves are computed at (the response below 20 kHz barely depends on it).
const EXPORT_RATE: f64 = 48_000.0;

// ---------------------------------------------------------------------------------------------
// Import
// ---------------------------------------------------------------------------------------------

/// Read presets from `text`; `name` (usually the file name) names those that carry none.
pub fn import(name: &str, text: &str) -> Result<Vec<EqPreset>> {
    let text = text.trim_start_matches('\u{feff}').trim();
    let name = clean_name(name);
    let presets = if text.starts_with('{') || text.starts_with('[') {
        import_json(&name, text)?
    } else if let Some(p) = import_feq(&name, text) {
        vec![p]
    } else if is_apo(text) {
        vec![import_apo(&name, text)?]
    } else if let Some(p) = import_table(&name, text) {
        vec![p]
    } else {
        bail!(UserError::EqFormat);
    };
    let presets: Vec<EqPreset> = presets.into_iter().map(EqPreset::sanitized).collect();
    if presets.is_empty() || presets.iter().all(|p| p.bands.is_empty() && p.curve.is_empty() && p.preamp == 0.0) {
        bail!(UserError::EqEmpty);
    }
    Ok(presets)
}

/// "Audeze LCD-2 ParametricEQ.txt" → "Audeze LCD-2".
fn clean_name(name: &str) -> String {
    let stem = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let stem = match stem.rsplit_once('.') {
        Some((s, ext)) if ext.len() <= 5 && !s.is_empty() => s,
        _ => stem,
    };
    let mut s = stem.trim().to_string();
    for suffix in [" ParametricEQ", " GraphicEQ", " FixedBandEQ", " FixedBandEQ_10", " ParametricEq"] {
        if let Some(t) = s.strip_suffix(suffix) {
            s = t.trim().to_string();
        }
    }
    s
}

fn number(s: &str) -> Option<f64> {
    let s = s.trim().replace(',', ".");
    s.parse::<f64>().ok().filter(|v| v.is_finite())
}

fn is_apo(text: &str) -> bool {
    text.lines().map(str::trim).any(|l| {
        let low = l.to_ascii_lowercase();
        low.starts_with("filter") || low.starts_with("preamp:") || low.starts_with("graphiceq:")
    })
}

/// Q for a bandwidth in octaves.
fn q_from_octaves(n: f64) -> f64 {
    let p = 2f64.powf(n);
    p.sqrt() / (p - 1.0)
}

fn import_apo(name: &str, text: &str) -> Result<EqPreset> {
    let mut p = EqPreset { name: name.to_string(), ..Default::default() };
    // Preamp for all channels, and on top for left / right only.
    let (mut pre_all, mut pre_l, mut pre_r) = (0.0, 0.0, 0.0);
    // None = a channel we don't play (centre, surround): skip its lines.
    let mut channel = Some(Channels::Both);
    for raw in text.lines() {
        let line = raw.split('#').next().unwrap_or("").trim();
        let Some((key, value)) = line.split_once(':') else { continue };
        let key = key.trim().to_ascii_lowercase();
        let value = value.trim();
        if key == "channel" {
            channel = parse_channel(value);
            continue;
        }
        let Some(ch) = channel else { continue };
        if key == "preamp" {
            let v = value.split_whitespace().next().and_then(number).unwrap_or(0.0);
            match ch {
                Channels::Both => pre_all += v,
                Channels::Left => pre_l += v,
                Channels::Right => pre_r += v,
            }
        } else if key == "graphiceq" {
            p.curve = parse_graphic(value);
        } else if key.starts_with("filter")
            && let Some(mut b) = parse_filter(value)
        {
            b.channels = ch;
            p.bands.push(b);
        }
    }
    let (left, right) = (pre_all + pre_l, pre_all + pre_r);
    p.preamp = left.max(right);
    p.balance = right - left;
    Ok(p)
}

fn parse_channel(value: &str) -> Option<Channels> {
    let words: Vec<String> = value.split_whitespace().map(|w| w.to_ascii_uppercase()).collect();
    let has = |w: &[&str]| words.iter().any(|x| w.contains(&x.as_str()));
    if words.is_empty() || has(&["ALL"]) {
        return Some(Channels::Both);
    }
    match (has(&["L", "1"]), has(&["R", "2"])) {
        (true, true) => Some(Channels::Both),
        (true, false) => Some(Channels::Left),
        (false, true) => Some(Channels::Right),
        (false, false) => None,
    }
}

fn parse_graphic(value: &str) -> Vec<[f64; 2]> {
    value
        .split(';')
        .filter_map(|pair| {
            let mut it = pair.split_whitespace();
            Some([number(it.next()?)?, number(it.next()?)?])
        })
        .collect()
}

/// `ON PK Fc 105 Hz Gain 6.0 dB Q 0.70` and all its relatives.
fn parse_filter(value: &str) -> Option<Band> {
    let words: Vec<&str> = value.split_whitespace().collect();
    let enabled = !words.first().is_some_and(|w| w.eq_ignore_ascii_case("OFF"));
    let mut i = usize::from(words.first().is_some_and(|w| w.eq_ignore_ascii_case("ON") || w.eq_ignore_ascii_case("OFF")));
    let kind_word = words.get(i)?.to_ascii_uppercase();
    i += 1;
    // "LS 6dB" / "HS 12dB": a shelf slope in place of Q.
    let shelf_slope = words.get(i).and_then(|w| w.to_ascii_lowercase().strip_suffix("db").and_then(number));
    if shelf_slope.is_some() {
        i += 1;
    }
    let (kind, explicit_q) = match kind_word.as_str() {
        "PK" | "PEQ" | "MODAL" => (FilterKind::Peak, true),
        "LS" | "LSC" | "LSQ" | "LOWSHELF" => (FilterKind::LowShelf, kind_word != "LS"),
        "HS" | "HSC" | "HSQ" | "HIGHSHELF" => (FilterKind::HighShelf, kind_word != "HS"),
        "LP" | "LPQ" | "LOWPASS" => (FilterKind::LowPass, kind_word == "LPQ"),
        "HP" | "HPQ" | "HIGHPASS" => (FilterKind::HighPass, kind_word == "HPQ"),
        "BP" | "BANDPASS" => (FilterKind::BandPass, true),
        "NO" | "NOTCH" => (FilterKind::Notch, true),
        "AP" | "ALLPASS" => (FilterKind::AllPass, true),
        _ => return None, // "None", "IIR", or a type we don't know
    };
    let rest = &words[i..];
    let after = |key: &str, skip: usize| -> Option<f64> {
        let k = rest.iter().position(|w| w.eq_ignore_ascii_case(key))?;
        number(rest.get(k + 1 + skip)?)
    };
    let freq = after("Fc", 0)?;
    let gain = after("Gain", 0).unwrap_or(0.0);
    let q = after("Q", 0)
        .or_else(|| after("BW", 1).filter(|_| rest.iter().any(|w| w.eq_ignore_ascii_case("Oct"))).map(q_from_octaves))
        .filter(|_| explicit_q || matches!(kind, FilterKind::LowShelf | FilterKind::HighShelf))
        .unwrap_or(match (kind, shelf_slope) {
            // A 6 dB/oct shelf is about as gentle as Q 0.5 gets.
            (FilterKind::LowShelf | FilterKind::HighShelf, Some(s)) if s <= 6.0 => 0.5,
            _ => std::f64::consts::FRAC_1_SQRT_2,
        });
    Some(Band { enabled, kind, freq, gain, q, slope: 12, channels: Channels::Both })
}

/// foobar2000's .feq: 18 integer gains for fixed bands.
fn import_feq(name: &str, text: &str) -> Option<EqPreset> {
    const BANDS: [f64; 18] =
        [55.0, 77.0, 110.0, 156.0, 220.0, 311.0, 440.0, 622.0, 880.0, 1200.0, 1800.0, 2500.0, 3500.0, 5000.0, 7000.0, 10_000.0, 14_000.0, 20_000.0];
    let values: Vec<&str> = text.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    if values.len() != 18 || !values.iter().all(|v| v.parse::<i32>().is_ok()) {
        return None;
    }
    let curve = BANDS.iter().zip(&values).map(|(&f, v)| [f, v.parse::<f64>().unwrap()]).collect();
    Some(EqPreset { name: name.to_string(), curve, ..Default::default() })
}

/// A table of frequency and gain: CSV, TSV, or whitespace. With a header, the gain column is
/// the one named like a correction (AutoEQ's `equalization`), else the second.
fn import_table(name: &str, text: &str) -> Option<EqPreset> {
    let split = |l: &str| -> Vec<String> {
        let sep: &[char] = if l.contains(',') && !l.contains(';') { &[','] } else if l.contains(';') { &[';'] } else { &['\t', ' '] };
        l.split(sep).map(|s| s.trim().trim_matches('"').to_string()).filter(|s| !s.is_empty()).collect()
    };
    let mut column = 1;
    let mut curve = Vec::new();
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with(['#', '*'])) {
        let cells = split(line);
        if cells.first().and_then(|c| number(c)).is_none() {
            // A header row.
            let low: Vec<String> = cells.iter().map(|c| c.to_ascii_lowercase()).collect();
            if let Some(i) = ["equalization", "eq", "gain", "db", "correction", "value"]
                .iter()
                .find_map(|k| low.iter().position(|c| c == k))
            {
                column = i;
            }
            continue;
        }
        let (Some(f), Some(g)) = (cells.first().and_then(|c| number(c)), cells.get(column).and_then(|c| number(c))) else {
            continue;
        };
        if f > 0.0 && f < 100_000.0 {
            curve.push([f, g]);
        }
    }
    (curve.len() >= 2 && curve.windows(2).filter(|w| w[1][0] > w[0][0]).count() * 10 >= (curve.len() - 1) * 9)
        .then(|| EqPreset { name: name.to_string(), curve: thin(curve), ..Default::default() })
}

/// Measurement exports can have thousands of rows; 1/48 octave is plenty for a curve.
fn thin(mut curve: Vec<[f64; 2]>) -> Vec<[f64; 2]> {
    curve.sort_by(|a, b| a[0].total_cmp(&b[0]));
    let mut out: Vec<[f64; 2]> = Vec::new();
    for p in curve {
        match out.last() {
            Some(last) if p[0] / last[0] < 2f64.powf(1.0 / 48.0) => {}
            _ => out.push(p),
        }
    }
    out
}

#[derive(Deserialize)]
struct HifiFile {
    format: String,
    #[serde(default)]
    presets: Vec<EqPreset>,
    #[serde(default)]
    preset: Option<EqPreset>,
}

fn import_json(name: &str, text: &str) -> Result<Vec<EqPreset>> {
    let v: Value = serde_json::from_str(text).map_err(|_| UserError::EqFormat)?;
    if v.get("format").and_then(Value::as_str) == Some("hifi-eq") {
        let f: HifiFile = serde_json::from_value(v).map_err(|_| UserError::EqFormat)?;
        let _ = f.format;
        let mut all = f.presets;
        all.extend(f.preset);
        return Ok(all);
    }
    if v.get("bands").is_some() && v.get("output").is_none() {
        let mut p: EqPreset = serde_json::from_value(v).map_err(|_| UserError::EqFormat)?;
        if p.name.is_empty() {
            p.name = name.to_string();
        }
        return Ok(vec![p]);
    }
    if let Some(p) = import_easyeffects(name, &v) {
        return Ok(vec![p]);
    }
    bail!(UserError::EqFormat)
}

fn import_easyeffects(name: &str, v: &Value) -> Option<EqPreset> {
    let root = v.get("output").or_else(|| v.get("input")).unwrap_or(v);
    let obj = root.as_object()?;
    let eq = obj.iter().find(|(k, _)| k.starts_with("equalizer")).map(|(_, v)| v)?;
    let gain = |k: &str| eq.get(k).and_then(Value::as_f64).unwrap_or(0.0);
    let split = eq.get("split-channels").and_then(Value::as_bool).unwrap_or(false);
    let count = eq.get("num-bands").and_then(Value::as_u64).unwrap_or(32) as usize;
    let mut p = EqPreset { name: name.to_string(), preamp: gain("input-gain") + gain("output-gain"), ..Default::default() };
    let sides: &[(&str, Channels)] =
        if split { &[("left", Channels::Left), ("right", Channels::Right)] } else { &[("left", Channels::Both)] };
    for &(side, channels) in sides {
        let Some(bands) = eq.get(side) else { continue };
        for i in 0..count {
            let Some(b) = bands.get(format!("band{i}")) else { continue };
            let kind = match b.get("type").and_then(Value::as_str).unwrap_or("Bell") {
                "Bell" | "Resonance" => FilterKind::Peak,
                "Lo-shelf" => FilterKind::LowShelf,
                "Hi-shelf" => FilterKind::HighShelf,
                "Lo-pass" => FilterKind::LowPass,
                "Hi-pass" => FilterKind::HighPass,
                "Notch" => FilterKind::Notch,
                "Bandpass" => FilterKind::BandPass,
                "Allpass" => FilterKind::AllPass,
                _ => continue, // "Off", ladder filters
            };
            let num = |k: &str, d: f64| b.get(k).and_then(Value::as_f64).unwrap_or(d);
            let slope = b.get("slope").and_then(Value::as_str).and_then(|s| s.strip_prefix('x')).and_then(|s| s.parse::<u32>().ok()).unwrap_or(1);
            p.bands.push(Band {
                enabled: !b.get("mute").and_then(Value::as_bool).unwrap_or(false),
                kind,
                freq: num("frequency", 1000.0),
                gain: num("gain", 0.0),
                q: num("q", std::f64::consts::FRAC_1_SQRT_2),
                slope: 12 * slope.clamp(1, 4),
                channels,
            });
        }
    }
    Some(p)
}

// ---------------------------------------------------------------------------------------------
// Export
// ---------------------------------------------------------------------------------------------

/// Write `presets` (one, except for the lossless JSON backup) as `format`.
pub fn export(presets: &[EqPreset], format: EqFormat) -> String {
    let Some(first) = presets.first() else { return String::new() };
    match format {
        EqFormat::Apo => export_apo(first),
        EqFormat::GraphicEq => export_graphic(first),
        EqFormat::EasyEffects => export_easyeffects(first),
        EqFormat::Csv => export_csv(first),
        EqFormat::Hifi => {
            let mut s = serde_json::to_string_pretty(&json!({ "format": "hifi-eq", "version": 1, "presets": presets }))
                .unwrap_or_default();
            s.push('\n');
            s
        }
    }
}

/// A number without trailing zeros, at most `dp` decimals.
fn fmt(v: f64, dp: usize) -> String {
    let s = format!("{v:.dp$}");
    let s = if s.contains('.') { s.trim_end_matches('0').trim_end_matches('.').to_string() } else { s };
    if s == "-0" { "0".into() } else { s }
}

fn export_apo(p: &EqPreset) -> String {
    let mut out = format!("# {}\n# Exported from BitSpring\n", p.name);
    if p.phase == Phase::Linear {
        out.push_str("# BitSpring plays this preset in linear phase; Equalizer APO runs it in minimum phase.\n");
    }
    out.push_str(&format!("Preamp: {} dB\n", fmt(p.effective_preamp(EXPORT_RATE), 1)));
    let mut n = 0;
    let groups = [(Channels::Both, "all"), (Channels::Left, "L"), (Channels::Right, "R")];
    let per_channel = p.balance != 0.0 || p.bands.iter().any(|b| b.channels != Channels::Both);
    for (ch, label) in groups {
        let bands: Vec<&Band> = p.bands.iter().filter(|b| b.channels == ch).collect();
        let balance = match ch {
            Channels::Left => p.balance.max(0.0),
            Channels::Right => (-p.balance).max(0.0),
            Channels::Both => 0.0,
        };
        if bands.is_empty() && balance == 0.0 {
            continue;
        }
        if per_channel {
            out.push_str(&format!("Channel: {label}\n"));
        }
        if balance > 0.0 {
            out.push_str(&format!("Preamp: -{} dB\n", fmt(balance, 1)));
        }
        for b in bands {
            for line in apo_lines(b) {
                n += 1;
                out.push_str(&format!("Filter {n}: {line}\n"));
            }
        }
    }
    if per_channel {
        out.push_str("Channel: all\n");
    }
    if !p.curve.is_empty() {
        let pts: Vec<String> = p.curve.iter().map(|[f, g]| format!("{} {}", fmt(*f, 1), fmt(*g, 1))).collect();
        out.push_str(&format!("GraphicEQ: {}\n", pts.join("; ")));
    }
    out
}

/// APO has no slopes beyond 12 dB/oct: a Butterworth of higher order is written as its
/// sections, which is exactly the same filter.
fn apo_lines(b: &Band) -> Vec<String> {
    let on = if b.enabled { "ON" } else { "OFF" };
    let (f, g, q) = (fmt(b.freq, 1), fmt(b.gain, 1), fmt(b.q, 3));
    let one = |s: String| vec![format!("{on} {s}")];
    match b.kind {
        FilterKind::Peak => one(format!("PK Fc {f} Hz Gain {g} dB Q {q}")),
        FilterKind::LowShelf => one(format!("LSC Fc {f} Hz Gain {g} dB Q {q}")),
        FilterKind::HighShelf => one(format!("HSC Fc {f} Hz Gain {g} dB Q {q}")),
        FilterKind::BandPass => one(format!("BP Fc {f} Hz Q {q}")),
        FilterKind::Notch => one(format!("NO Fc {f} Hz Q {q}")),
        FilterKind::AllPass => one(format!("AP Fc {f} Hz Q {q}")),
        FilterKind::LowPass | FilterKind::HighPass => {
            let t = if b.kind == FilterKind::LowPass { "LPQ" } else { "HPQ" };
            let order = (b.slope / 6) as usize;
            if order == 2 {
                return one(format!("{t} Fc {f} Hz Q {q}"));
            }
            let mut lines = Vec::new();
            if !order.is_multiple_of(2) {
                // No first-order type in APO: the closest is a gentle second order.
                lines.push(format!("{on} {t} Fc {f} Hz Q 0.5"));
            }
            for k in 1..=order / 2 {
                let theta = if order.is_multiple_of(2) {
                    (2 * k - 1) as f64 * std::f64::consts::PI / (2 * order) as f64
                } else {
                    k as f64 * std::f64::consts::PI / order as f64
                };
                lines.push(format!("{on} {t} Fc {f} Hz Q {}", fmt(1.0 / (2.0 * theta.cos()), 4)));
            }
            lines
        }
    }
}

/// 127 points from 20 Hz to ~20 kHz, like AutoEQ's GraphicEQ (Wavelet's limit).
fn graphic_freqs() -> Vec<f64> {
    (0..127).map(|i| (20.0 * 1000f64.powf(i as f64 / 126.0)).round()).collect()
}

/// Both channels' response, averaged where they differ.
fn mean_response(p: &EqPreset, f: f64) -> f64 {
    (p.response_db(f, EXPORT_RATE, 0) + p.response_db(f, EXPORT_RATE, 1)) / 2.0
}

fn export_graphic(p: &EqPreset) -> String {
    let pts: Vec<String> = graphic_freqs().iter().map(|&f| format!("{} {}", fmt(f, 0), fmt(mean_response(p, f), 1))).collect();
    format!("GraphicEQ: {}\n", pts.join("; "))
}

fn export_csv(p: &EqPreset) -> String {
    let split = p.balance != 0.0 || p.bands.iter().any(|b| b.channels != Channels::Both);
    let mut out = String::from(if split { "frequency,left,right\n" } else { "frequency,gain\n" });
    for i in 0..=240 {
        let f = 20.0 * 1000f64.powf(i as f64 / 240.0);
        let (l, r) = (p.response_db(f, EXPORT_RATE, 0), p.response_db(f, EXPORT_RATE, 1));
        if split {
            out.push_str(&format!("{},{},{}\n", fmt(f, 2), fmt(l, 2), fmt(r, 2)));
        } else {
            out.push_str(&format!("{},{}\n", fmt(f, 2), fmt(l, 2)));
        }
    }
    out
}

fn export_easyeffects(p: &EqPreset) -> String {
    let split = p.bands.iter().any(|b| b.channels != Channels::Both);
    let side = |ch: usize| -> (Value, usize) {
        let mut m = serde_json::Map::new();
        let mut i = 0;
        for b in p.bands.iter().filter(|b| b.channels == Channels::Both || (split && b.channels == if ch == 0 { Channels::Left } else { Channels::Right })) {
            let t = match b.kind {
                FilterKind::Peak => "Bell",
                FilterKind::LowShelf => "Lo-shelf",
                FilterKind::HighShelf => "Hi-shelf",
                FilterKind::LowPass => "Lo-pass",
                FilterKind::HighPass => "Hi-pass",
                FilterKind::Notch => "Notch",
                FilterKind::BandPass => "Bandpass",
                FilterKind::AllPass => "Allpass",
            };
            m.insert(
                format!("band{i}"),
                json!({
                    "frequency": b.freq, "gain": b.gain, "q": b.q, "type": t, "mode": "RLC (BT)",
                    "slope": format!("x{}", (b.slope / 12).clamp(1, 4)), "mute": !b.enabled, "solo": false,
                }),
            );
            i += 1;
        }
        (Value::Object(m), i)
    };
    let (left, nl) = side(0);
    let (right, nr) = side(1);
    let v = json!({
        "output": {
            "blocklist": [],
            "equalizer#0": {
                "balance": 0.0,
                "bypass": false,
                "input-gain": (p.effective_preamp(EXPORT_RATE) * 10.0).round() / 10.0,
                "output-gain": 0.0,
                "mode": "IIR",
                "num-bands": nl.max(nr).max(1),
                "split-channels": split,
                "left": left,
                "right": right,
            },
            "plugins_order": ["equalizer#0"],
        }
    });
    let mut s = serde_json::to_string_pretty(&v).unwrap_or_default();
    s.push('\n');
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    const AUTOEQ: &str = "Preamp: -6.4 dB\n\
        Filter 1: ON LSC Fc 105 Hz Gain 6.0 dB Q 0.70\n\
        Filter 2: ON PK Fc 2700 Hz Gain -3.1 dB Q 1.41\n\
        Filter 3: OFF PK Fc 5000 Hz Gain 9.0 dB Q 2.00\n\
        Filter 4: ON HSC Fc 10000 Hz Gain 2.5 dB Q 0.70\n";

    #[test]
    fn reads_autoeq_parametric() {
        let p = &import("Audeze LCD-2 ParametricEQ.txt", AUTOEQ).unwrap()[0];
        assert_eq!(p.name, "Audeze LCD-2");
        assert_eq!(p.preamp, -6.4);
        assert_eq!(p.bands.len(), 4);
        assert_eq!(p.bands[0], Band { kind: FilterKind::LowShelf, freq: 105.0, gain: 6.0, q: 0.7, ..Default::default() });
        assert!(!p.bands[2].enabled);
        assert_eq!(p.bands[3].kind, FilterKind::HighShelf);
    }

    #[test]
    fn reads_apo_channels_bandwidth_and_rew_spacing() {
        let text = "# comment\nDevice: Speakers\nPreamp: -3 dB\n\
            Channel: L\nPreamp: -1.5 dB\nFilter: ON PK Fc 1000 Hz Gain -4 dB BW Oct 1.0\n\
            Channel: R\nFilter  2: ON  HPQ      Fc   25,0 Hz  Q  0.71\n\
            Channel: C\nFilter: ON PK Fc 50 Hz Gain 10 dB Q 1\n\
            Channel: all\nFilter  3: ON  None\nFilter 4: ON LP Fc 18000 Hz\n";
        let p = &import("room.txt", text).unwrap()[0];
        assert_eq!(p.bands.len(), 3, "{:?}", p.bands);
        assert_eq!(p.bands[0].channels, Channels::Left);
        assert!((p.bands[0].q - std::f64::consts::SQRT_2).abs() < 0.001);
        assert_eq!((p.bands[1].kind, p.bands[1].freq, p.bands[1].channels), (FilterKind::HighPass, 25.0, Channels::Right));
        assert_eq!(p.bands[2].kind, FilterKind::LowPass);
        // −3 for both, −1.5 more on the left: the right is louder by 1.5 dB.
        assert_eq!(p.preamp, -3.0);
        assert_eq!(p.balance, 1.5);
        assert_eq!(p.channel_gain_db(0, 48_000.0), -4.5);
    }

    #[test]
    fn reads_graphiceq_and_feq_and_csv() {
        let g = &import("x GraphicEQ.txt", "GraphicEQ: 20 -1.5; 21 -1.4; 1000 0; 20000 2,5")
            .unwrap()[0];
        assert_eq!(g.curve, vec![[20.0, -1.5], [21.0, -1.4], [1000.0, 0.0], [20_000.0, 2.5]]);
        let feq: String = (0..18).map(|i| format!("{}\n", i - 9)).collect();
        let f = &import("old.feq", &feq).unwrap()[0];
        assert_eq!(f.curve.len(), 18);
        assert_eq!(f.curve[0], [55.0, -9.0]);
        let csv = "frequency,raw,error,smoothed,error_smoothed,equalization\n20,1,2,3,4,-5.5\n40,1,2,3,4,-2\n80,0,0,0,0,1\n";
        let c = &import("result.csv", csv).unwrap()[0];
        assert_eq!(c.curve, vec![[20.0, -5.5], [40.0, -2.0], [80.0, 1.0]]);
        let tsv = "* REW\n20\t-3\n100\t0\n";
        assert_eq!(import("t.txt", tsv).unwrap()[0].curve, vec![[20.0, -3.0], [100.0, 0.0]]);
    }

    #[test]
    fn unknown_or_empty_text_is_refused_by_code() {
        let e = import("x.txt", "hello world").unwrap_err();
        assert_eq!(UserError::of(&e), UserError::EqFormat);
        let e = import("x.txt", "Preamp: 0 dB\n").unwrap_err();
        assert_eq!(UserError::of(&e), UserError::EqEmpty);
    }

    fn rich() -> EqPreset {
        EqPreset {
            name: "Rich".into(),
            preamp: -4.0,
            balance: -0.5,
            bands: vec![
                Band { kind: FilterKind::LowShelf, freq: 105.0, gain: 5.5, q: 0.71, ..Default::default() },
                Band { kind: FilterKind::Peak, freq: 2700.0, gain: -3.0, q: 1.4, channels: Channels::Left, ..Default::default() },
                Band { kind: FilterKind::HighPass, freq: 20.0, q: std::f64::consts::FRAC_1_SQRT_2, slope: 24, ..Default::default() },
                Band { kind: FilterKind::Notch, freq: 6000.0, q: 8.0, enabled: false, ..Default::default() },
            ],
            ..Default::default()
        }
    }

    fn same_response(a: &EqPreset, b: &EqPreset, tol: f64) {
        for i in 0..=60 {
            let f = 20.0 * 1000f64.powf(i as f64 / 60.0);
            for ch in 0..2 {
                let (x, y) = (a.response_db(f, 48_000.0, ch), b.response_db(f, 48_000.0, ch));
                assert!((x - y).abs() < tol, "{f} Hz ch{ch}: {x} vs {y}");
            }
        }
    }

    #[test]
    fn apo_round_trip_keeps_the_sound() {
        let p = rich();
        let text = export(std::slice::from_ref(&p), EqFormat::Apo);
        let back = &import("Rich.txt", &text).unwrap()[0];
        same_response(&p, back, 0.01);
        assert!(back.bands.iter().any(|b| !b.enabled));
    }

    #[test]
    fn easyeffects_round_trip() {
        let p = rich();
        let text = export(std::slice::from_ref(&p), EqFormat::EasyEffects);
        let back = &import("Rich.json", &text).unwrap()[0];
        // EasyEffects has no balance; everything else survives.
        let mut want = p.clone();
        want.preamp = p.effective_preamp(48_000.0);
        want.balance = 0.0;
        same_response(&want, back, 0.01);
    }

    #[test]
    fn graphic_and_csv_export_follow_the_response() {
        let p = rich();
        let g = &import("g.txt", &export(std::slice::from_ref(&p), EqFormat::GraphicEq)).unwrap()[0];
        assert_eq!(g.curve.len(), 127);
        for &[f, db] in &g.curve {
            assert!((db - mean_response(&p, f)).abs() <= 0.051);
        }
        let csv = export(std::slice::from_ref(&p), EqFormat::Csv);
        assert!(csv.starts_with("frequency,left,right\n"));
    }

    #[test]
    fn hifi_json_is_lossless_and_holds_many() {
        let mut a = rich();
        a.phase = Phase::Linear;
        a.curve = vec![[100.0, 1.0], [1000.0, -1.0]];
        a.auto_preamp = true;
        let b = EqPreset { name: "Second".into(), ..Default::default() };
        let text = export(&[a.clone(), b.clone()], EqFormat::Hifi);
        let back = import("backup.json", &text).unwrap();
        assert_eq!(back, vec![a.sanitized(), b.sanitized()]);
    }
}

// ---------------------------------------------------------------------------------------------
// Built-in headphone profiles
// ---------------------------------------------------------------------------------------------

/// One built-in headphone profile: AutoEQ's correction for that model, and whose measurement
/// it comes from.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct HeadphoneProfile {
    pub name: String,
    pub source: String,
    /// AutoEQ's ParametricEQ.txt.
    pub eq: String,
}

#[derive(serde::Deserialize)]
struct HeadphoneData {
    profiles: Vec<HeadphoneProfile>,
}

/// The built-in profiles (see data/README.md), by name.
pub fn headphone_profiles() -> &'static [HeadphoneProfile] {
    static DATA: std::sync::OnceLock<Vec<HeadphoneProfile>> = std::sync::OnceLock::new();
    DATA.get_or_init(|| {
        serde_json::from_str::<HeadphoneData>(include_str!("../../data/headphones.json")).map(|d| d.profiles).unwrap_or_default()
    })
}

/// The preset for a built-in profile, named after the headphones.
pub fn headphone_preset(name: &str) -> Option<EqPreset> {
    let p = headphone_profiles().iter().find(|p| p.name == name)?;
    import(&p.name, &p.eq).ok()?.into_iter().next().map(|mut preset| {
        preset.name = p.name.clone();
        preset
    })
}

/// AutoEQ's whole catalog (every headphone it has results for, often measured by several
/// people), packed by data/autoeq_catalog.py and served with the website, so the app fetches
/// one file instead of thousands, and from us rather than GitHub.
pub const AUTOEQ_CATALOG: &str = "https://hifi-player-nu.vercel.app/autoeq/catalog.json";

/// One measurement in the catalog.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct CatalogEntry {
    pub name: String,
    /// Who measured them (crinacle, oratory1990, Rtings…).
    pub source: String,
    /// On what rig, when known ("GRAS 43AG-7").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rig: Option<String>,
    /// `<preamp>;<filter>;…`, a filter as `<kind><Fc>,<gain>,<Q>` (L low shelf, P peak, H high shelf).
    pub eq: String,
}

#[derive(serde::Deserialize)]
struct CatalogFile {
    version: u32,
    profiles: Vec<CatalogEntry>,
}

/// The catalog's entries; an error when the file isn't one (or is a newer format than this
/// build reads).
pub fn parse_catalog(json: &str) -> anyhow::Result<Vec<CatalogEntry>> {
    let file: CatalogFile = serde_json::from_str(json)?;
    anyhow::ensure!(file.version == 1, "catalog format {} is newer than this app", file.version);
    Ok(file.profiles)
}

/// A catalog entry's filters as AutoEQ's ParametricEQ.txt, which [`import`] reads.
fn catalog_text(eq: &str) -> Option<String> {
    let mut parts = eq.split(';');
    let preamp: f64 = parts.next()?.parse().ok()?;
    let mut text = format!("Preamp: {preamp} dB\n");
    for (i, f) in parts.enumerate() {
        let kind = match f.get(..1)? {
            "L" => "LSC",
            "P" => "PK",
            "H" => "HSC",
            _ => return None,
        };
        let mut v = f[1..].split(',').map(|x| x.parse::<f64>().ok().filter(|x| x.is_finite()));
        let (fc, gain, q) = (v.next()??, v.next()??, v.next()??);
        if v.next().is_some() {
            return None;
        }
        text += &format!("Filter {}: ON {kind} Fc {fc} Hz Gain {gain} dB Q {q}\n", i + 1);
    }
    Some(text)
}

/// The preset for a catalog entry: named after the headphones and who measured them, since
/// the catalog often has the same model several times.
pub fn catalog_preset(entry: &CatalogEntry) -> anyhow::Result<EqPreset> {
    let name = format!("{} ({})", entry.name, entry.source);
    let text = catalog_text(&entry.eq).ok_or(crate::UserError::ProfileEmpty)?;
    let mut preset = import(&name, &text)?.into_iter().next().ok_or(crate::UserError::ProfileEmpty)?;
    preset.name = name;
    Ok(preset)
}

#[cfg(test)]
mod headphone_tests {
    use super::*;

    #[test]
    fn every_built_in_profile_imports() {
        let all = headphone_profiles();
        assert!(all.len() > 100, "{}", all.len());
        for p in all {
            let preset = headphone_preset(&p.name).unwrap_or_else(|| panic!("{} doesn't import", p.name));
            assert!(!preset.bands.is_empty(), "{}", p.name);
        }
        let lcd2 = headphone_preset("Audeze LCD-2").unwrap();
        assert!(lcd2.preamp < -3.0, "{}", lcd2.preamp);
    }

    #[test]
    fn a_catalog_entry_becomes_a_preset() {
        let json = r#"{"version":1,"about":"","commit":"x","profiles":[
            {"name":"Sennheiser HD 600","source":"crinacle","rig":"GRAS 43AG-7","eq":"-6.3;L105,6.3,0.7;P10000,4.4,0.75;H10000,-2,0.7"},
            {"name":"Bad","source":"x","eq":"-1;X100,1,1"}]}"#;
        let all = parse_catalog(json).unwrap();
        assert_eq!(all[0].rig.as_deref(), Some("GRAS 43AG-7"));
        let p = catalog_preset(&all[0]).unwrap();
        assert_eq!(p.name, "Sennheiser HD 600 (crinacle)");
        assert_eq!(p.bands.len(), 3);
        assert!((p.preamp + 6.3).abs() < 1e-9, "{}", p.preamp);
        assert!(catalog_preset(&all[1]).is_err());
        assert!(parse_catalog(r#"{"version":2,"profiles":[]}"#).is_err());
    }

    /// Every entry of the real catalog (`HIFI_AUTOEQ_CATALOG=apps/website/public/autoeq/catalog.json`).
    #[test]
    #[ignore]
    fn the_whole_catalog_imports() {
        let all = parse_catalog(&std::fs::read_to_string(std::env::var("HIFI_AUTOEQ_CATALOG").unwrap()).unwrap()).unwrap();
        assert!(all.len() > 8000, "{}", all.len());
        for e in &all {
            let p = catalog_preset(e).unwrap_or_else(|err| panic!("{} ({}): {err}", e.name, e.source));
            assert!(p.bands.len() >= 3, "{}", e.name);
        }
    }
}
