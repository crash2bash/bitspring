//! Choosing the hardware sample rate for a given source.
//!
//! Platform-neutral on purpose: CoreAudio, WASAPI and ALSA all report "supported rates" as
//! either discrete values or ranges, so the same policy serves every backend.

/// A rate (or continuous range of rates) a device accepts, in Hz.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RateRange {
    pub min: f64,
    pub max: f64,
}

impl RateRange {
    pub fn exact(hz: f64) -> Self {
        Self { min: hz, max: hz }
    }

    pub fn contains(&self, hz: f64) -> bool {
        hz >= self.min - 0.5 && hz <= self.max + 0.5
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RateDecision {
    /// The device runs at the source rate: samples pass through untouched.
    BitPerfect(f64),
    /// The device can't run at the source rate; play at `device_hz` through a resampler.
    Resample { device_hz: f64 },
    /// Nothing sensible to play at.
    Unsupported,
}

/// Standard rates we'll consider inside a continuous device range.
const STANDARD_RATES: [f64; 16] = [
    8_000.0, 11_025.0, 16_000.0, 22_050.0, 32_000.0, 44_100.0, 48_000.0, 88_200.0, 96_000.0,
    176_400.0, 192_000.0, 352_800.0, 384_000.0, 705_600.0, 768_000.0, 1_536_000.0,
];

/// Rate family: 44.1 kHz multiples and 48 kHz multiples come from different master clocks;
/// converting within a family is a clean integer ratio (2×, 4×, ½…).
fn family(hz: f64) -> u8 {
    let hz = hz.round() as u64;
    if hz % 11_025 == 0 { 1 } else if hz % 8_000 == 0 { 2 } else { 0 }
}

/// Concrete rates a device offers (ranges expanded to the standard rates they contain).
pub fn candidate_rates(available: &[RateRange]) -> Vec<f64> {
    let mut out: Vec<f64> = available
        .iter()
        .flat_map(|r| {
            if (r.max - r.min).abs() < 0.5 {
                vec![r.min]
            } else {
                STANDARD_RATES.iter().copied().filter(|&hz| r.contains(hz)).collect()
            }
        })
        .collect();
    out.sort_by(f64::total_cmp);
    out.dedup_by(|a, b| (*a - *b).abs() < 0.5);
    out
}

/// Decide which rate to switch the device to for a source at `source_hz`.
///
/// Policy, in order:
/// 1. the source rate itself → bit-perfect;
/// 2. the lowest same-family rate above the source (44.1 → 88.2): integer upsampling, the
///    cleanest conversion, and no more data than needed;
/// 3. the highest same-family rate below it (352.8 → 176.4 on a 192 kHz DAC): integer
///    downsampling keeps the most bandwidth the DAC can take;
/// 4. the lowest rate above the source from the other family (44.1 → 48 on a 48-only dongle);
/// 5. whatever the device's highest rate is.
pub fn choose_device_rate(source_hz: u32, available: &[RateRange]) -> RateDecision {
    let src = source_hz as f64;
    if available.iter().any(|r| r.contains(src)) {
        return RateDecision::BitPerfect(src);
    }
    let rates = candidate_rates(available);
    let fam = family(src);
    let same = |hz: &&f64| fam != 0 && family(**hz) == fam;

    let pick = rates.iter().filter(same).find(|&&hz| hz > src)
        .or_else(|| rates.iter().filter(same).rev().find(|&&hz| hz < src))
        .or_else(|| rates.iter().find(|&&hz| hz > src))
        .or_else(|| rates.last());
    match pick {
        Some(&device_hz) => RateDecision::Resample { device_hz },
        None => RateDecision::Unsupported,
    }
}

/// Rate to run at when the user asks for upsampling: the highest same-family rate the
/// device offers, capped at `max_hz` (very high rates cost CPU for no audible gain).
pub fn upsample_target(source_hz: u32, available: &[RateRange], max_hz: f64) -> f64 {
    let src = source_hz as f64;
    let rates = candidate_rates(available);
    let fam = family(src);
    rates
        .iter()
        .copied()
        .filter(|&hz| hz >= src && hz <= max_hz && (fam == 0 || family(hz) == fam))
        .fold(None, |best: Option<f64>, hz| Some(best.map_or(hz, |b| b.max(hz))))
        // No same-family rate (e.g. 44.1k on a 48/96-only DAC): the user asked to upsample,
        // so take the highest rate under the cap from the other family.
        .or_else(|| rates.iter().copied().filter(|&hz| hz > src && hz <= max_hz).reduce(f64::max))
        .unwrap_or_else(|| match choose_device_rate(source_hz, available) {
            RateDecision::BitPerfect(hz) | RateDecision::Resample { device_hz: hz } => hz,
            RateDecision::Unsupported => src,
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rates(hz: &[f64]) -> Vec<RateRange> {
        hz.iter().copied().map(RateRange::exact).collect()
    }

    #[test]
    fn exact_rate_is_bit_perfect() {
        let dac = rates(&[44_100.0, 48_000.0, 96_000.0]);
        assert_eq!(choose_device_rate(44_100, &dac), RateDecision::BitPerfect(44_100.0));
    }

    #[test]
    fn prefers_same_family_upsampling() {
        let dac = rates(&[48_000.0, 88_200.0, 96_000.0, 176_400.0]);
        assert_eq!(choose_device_rate(44_100, &dac), RateDecision::Resample { device_hz: 88_200.0 });
    }

    #[test]
    fn dxd_on_a_192k_dac_halves_within_family() {
        let dac = rates(&[44_100.0, 48_000.0, 88_200.0, 96_000.0, 176_400.0, 192_000.0]);
        assert_eq!(choose_device_rate(352_800, &dac), RateDecision::Resample { device_hz: 176_400.0 });
    }

    #[test]
    fn cd_on_a_48k_only_dongle_goes_up_across_families() {
        let dongle = rates(&[48_000.0]);
        assert_eq!(choose_device_rate(44_100, &dongle), RateDecision::Resample { device_hz: 48_000.0 });
    }

    #[test]
    fn upsample_target_stays_in_family_and_under_cap() {
        let q7 = rates(&[44_100.0, 48_000.0, 88_200.0, 96_000.0, 176_400.0, 192_000.0, 352_800.0, 384_000.0, 705_600.0, 768_000.0]);
        assert_eq!(upsample_target(44_100, &q7, 384_000.0), 352_800.0);
        assert_eq!(upsample_target(48_000, &q7, 384_000.0), 384_000.0);
        assert_eq!(upsample_target(44_100, &q7, 192_000.0), 176_400.0);
    }

    #[test]
    fn upsample_crosses_family_when_it_must() {
        let elgato = rates(&[48_000.0, 96_000.0]);
        assert_eq!(upsample_target(44_100, &elgato, 384_000.0), 96_000.0);
    }

    #[test]
    fn empty_device_is_unsupported() {
        assert_eq!(choose_device_rate(44_100, &[]), RateDecision::Unsupported);
    }

    #[test]
    fn rate_inside_a_range_is_bit_perfect() {
        let dac = [RateRange { min: 8_000.0, max: 192_000.0 }];
        assert_eq!(choose_device_rate(88_200, &dac), RateDecision::BitPerfect(88_200.0));
    }
}
