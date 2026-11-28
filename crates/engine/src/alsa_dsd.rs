//! Native DSD on Linux, the parts that need no ALSA (so their tests run on every OS): the
//! kernel's five DSD sample formats, the rates a card runs at for each, and what a USB card's
//! `/proc/asound/cardN/streamM` says it plays — PCM and native DSD kept apart, altset by altset.
//!
//! In a native DSD format each device frame carries 1, 2 or 4 DSD bytes per channel, so the
//! card's rate is the DSD bit rate / (8 · bytes): DSD64 (2.8224 MHz) runs at 88.2 kHz in
//! DSD_U32, 176.4 kHz in DSD_U16 and 352.8 kHz in DSD_U8. A USB DAC offers a DSD format only
//! where the kernel's quirk table knows it takes raw DSD; otherwise DSD goes as DoP, or PCM.

// Only the Linux backend uses this outside the tests.
#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

use crate::output::{self, OutFormat};

/// One of ALSA's native DSD formats.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DsdFormat {
    U32Be,
    U32Le,
    U16Be,
    U16Le,
    U8,
}

impl DsdFormat {
    /// In order of preference: the widest container first (the fewest device frames for the
    /// same bits, so the lowest card rate), big-endian before little (the earliest byte first
    /// in memory, what USB DACs take).
    pub(crate) const PREFERENCE: [DsdFormat; 5] = [Self::U32Be, Self::U32Le, Self::U16Be, Self::U16Le, Self::U8];

    /// DSD bytes per channel in one device frame.
    pub(crate) fn bytes(self) -> u8 {
        match self {
            Self::U32Be | Self::U32Le => 4,
            Self::U16Be | Self::U16Le => 2,
            Self::U8 => 1,
        }
    }

    /// How the callback packs the bits for it (see [`OutFormat::Dsd`]).
    pub(crate) fn out_format(self) -> OutFormat {
        let big_endian = matches!(self, Self::U32Be | Self::U16Be | Self::U8);
        OutFormat::Dsd { bytes: self.bytes(), big_endian, lsb_first: false }
    }

    /// The name the kernel prints (`/proc/asound/…/stream0`, `aplay --dump-hw-params`).
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::U32Be => "DSD_U32_BE",
            Self::U32Le => "DSD_U32_LE",
            Self::U16Be => "DSD_U16_BE",
            Self::U16Le => "DSD_U16_LE",
            Self::U8 => "DSD_U8",
        }
    }

    pub(crate) fn from_name(name: &str) -> Option<Self> {
        Self::PREFERENCE.into_iter().find(|f| f.name() == name)
    }

    /// The card's rate for `dsd_rate` (bits per second per channel) in this format.
    pub(crate) fn device_rate(self, dsd_rate: u32) -> u32 {
        dsd_rate / (8 * self.bytes() as u32)
    }

    /// The DSD bit rate behind a card rate in this format.
    pub(crate) fn dsd_rate(self, device_rate: u32) -> u32 {
        device_rate * 8 * self.bytes() as u32
    }

    /// The card rates worth asking for in this format: one for each of [`DSD_RATES`].
    pub(crate) fn device_rates(self) -> impl Iterator<Item = u32> {
        DSD_RATES.into_iter().map(move |r| self.device_rate(r))
    }
}

/// The DSD rates played natively: DSD64 · 2^k of both families (44.1 kHz · 64 and 48 kHz ·
/// 64), DSD64 up to DSD1024. A format's rate that maps to anything else is left out.
pub(crate) const DSD_RATES: [u32; 10] = [
    2_822_400, 3_072_000, // DSD64
    5_644_800, 6_144_000, // DSD128
    11_289_600, 12_288_000, // DSD256
    22_579_200, 24_576_000, // DSD512
    45_158_400, 49_152_000, // DSD1024
];

pub(crate) fn is_dsd_rate(dsd_rate: u32) -> bool {
    DSD_RATES.contains(&dsd_rate)
}

/// What a card takes as native DSD: for each format it offers, the DSD rates (bits per second
/// per channel, see [`DSD_RATES`]) it runs at in it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct NativeDsd {
    /// In [`DsdFormat::PREFERENCE`] order, each with its rates ascending; no format without
    /// rates.
    formats: Vec<(DsdFormat, Vec<u32>)>,
}

impl NativeDsd {
    /// The card runs at `dsd_rate` in `format` (ignored unless it is one of [`DSD_RATES`]).
    pub(crate) fn add(&mut self, format: DsdFormat, dsd_rate: u32) {
        if !is_dsd_rate(dsd_rate) {
            return;
        }
        let rank = |f: DsdFormat| DsdFormat::PREFERENCE.iter().position(|&p| p == f);
        let at = match self.formats.iter().position(|(f, _)| rank(*f) >= rank(format)) {
            Some(i) if self.formats[i].0 == format => i,
            Some(i) => {
                self.formats.insert(i, (format, Vec::new()));
                i
            }
            None => {
                self.formats.push((format, Vec::new()));
                self.formats.len() - 1
            }
        };
        let rates = &mut self.formats[at].1;
        if let Err(i) = rates.binary_search(&dsd_rate) {
            rates.insert(i, dsd_rate);
        }
    }

    /// The DSD rates the card runs at in `format`.
    #[cfg(test)]
    pub(crate) fn rates(&self, format: DsdFormat) -> &[u32] {
        self.formats.iter().find(|(f, _)| *f == format).map_or(&[], |(_, r)| r)
    }

    /// The format to send `dsd_rate` in: the first of [`DsdFormat::PREFERENCE`] that has it.
    pub(crate) fn format_for(&self, dsd_rate: u32) -> Option<DsdFormat> {
        self.formats.iter().find(|(_, r)| r.contains(&dsd_rate)).map(|(f, _)| *f)
    }

    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.formats.is_empty()
    }
}

/// How to open a card for a native DSD playback whose word rate (`Plan::target`, our 16-bit
/// words per second, `dsd_rate / 16`) is `word_rate`: the format, and the card rate exactly.
/// None when the card has no native format for that DSD rate.
pub(crate) fn native_open(native: &NativeDsd, word_rate: f64) -> Option<(DsdFormat, u32)> {
    let format = native.format_for(output::dsd_rate_of_words(word_rate))?;
    let rate = output::native_device_rate(word_rate, format.out_format())?;
    Some((format, rate.round() as u32))
}

/// What a USB card's `/proc/asound/cardN/streamM` lists for playback.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct StreamCaps {
    /// PCM rates, ascending: those of `pcm_candidates` that an altset with a PCM format lists.
    pub pcm_rates: Vec<u32>,
    /// The most channels a PCM altset has (2 at least).
    pub max_channels: u32,
    /// A PCM altset takes an integer format of 24 bits or more (needed for DoP).
    pub wide_int: bool,
    /// The DSD altsets: each one's DSD format, at the DSD rates its own rates stand for.
    pub native: NativeDsd,
}

/// Read the "Playback:" part of a `/proc/asound/cardN/streamM` text, altset by altset (each
/// begins at its "Format:" line; "Capture:" and what follows is left out). A PCM altset's
/// rates are PCM rates; a DSD altset's are card rates in that DSD format, so they go into
/// `native` as DSD rates and never into the PCM list (DSD64 at DSD_U32 lists 88200, which is
/// no PCM rate the card has). An altset listing both a PCM and a DSD format counts for both.
/// None when no PCM rate is found.
pub(crate) fn parse_stream(text: &str, pcm_candidates: &[u32]) -> Option<StreamCaps> {
    struct Altset<'a> {
        formats: Vec<&'a str>,
        channels: u32,
        rates: Vec<u32>,
    }
    let playback = text.split("Capture:").next()?;
    let mut altsets: Vec<Altset> = Vec::new();
    for line in playback.lines().map(str::trim) {
        if let Some(f) = line.strip_prefix("Format:") {
            altsets.push(Altset { formats: f.split_whitespace().collect(), channels: 2, rates: Vec::new() });
            continue;
        }
        let Some(alt) = altsets.last_mut() else { continue };
        if let Some(c) = line.strip_prefix("Channels:") {
            alt.channels = c.trim().parse().unwrap_or(2);
        } else if let Some(list) = line.strip_prefix("Rates:") {
            alt.rates = parse_rates(list, pcm_candidates);
        }
    }
    let mut caps = StreamCaps { max_channels: 2, ..Default::default() };
    for alt in &altsets {
        let dsd: Vec<DsdFormat> = alt.formats.iter().filter_map(|f| DsdFormat::from_name(f)).collect();
        // ("SPECIAL" is raw data the kernel didn't name: neither PCM we know nor DSD.)
        let pcm = alt.formats.iter().any(|f| !f.starts_with("DSD_") && *f != "SPECIAL");
        if pcm {
            for &r in &alt.rates {
                if pcm_candidates.contains(&r) && !caps.pcm_rates.contains(&r) {
                    caps.pcm_rates.push(r);
                }
            }
            caps.max_channels = caps.max_channels.max(alt.channels);
            caps.wide_int |= alt.formats.iter().any(|f| matches!(*f, "S32_LE" | "S24_3LE" | "S24_LE"));
        }
        for f in dsd {
            for &r in &alt.rates {
                caps.native.add(f, f.dsd_rate(r));
            }
        }
    }
    caps.pcm_rates.sort_unstable();
    (!caps.pcm_rates.is_empty()).then_some(caps)
}

/// A "Rates:" list: "44100, 48000, 96000", or "8000 - 192000 (continuous)" (then every rate
/// worth playing in that range: `candidates` and the DSD card rates of every format).
fn parse_rates(list: &str, candidates: &[u32]) -> Vec<u32> {
    if let Some(range) = list.trim().strip_suffix("(continuous)") {
        let mut ends = range.split('-').map(|v| v.trim().parse::<u32>());
        let (Some(Ok(lo)), Some(Ok(hi))) = (ends.next(), ends.next()) else { return Vec::new() };
        let dsd = DsdFormat::PREFERENCE.into_iter().flat_map(DsdFormat::device_rates);
        let mut rates: Vec<u32> = candidates.iter().copied().chain(dsd).filter(|r| (lo..=hi).contains(r)).collect();
        rates.sort_unstable();
        rates.dedup();
        return rates;
    }
    list.split(',').filter_map(|r| r.trim().parse().ok()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The PCM rates the Linux backend asks for.
    const PCM: [u32; 10] = [44_100, 48_000, 88_200, 96_000, 176_400, 192_000, 352_800, 384_000, 705_600, 768_000];

    /// A plain USB DAC: PCM only, 16 and 24 bits, playing (its "Status: Running" block lists
    /// "Interface = 1"-style lines, which must not start an altset), with a capture side.
    const PCM_ONLY: &str = "\
Generic USB Audio at usb-0000:00:14.0-2, high speed : USB Audio

Playback:
  Status: Running
    Interface = 1
    Altset = 2
    Packet Size = 288
    Momentary freq = 96000 Hz (0xc.0000)
    Feedback Format = 16.16
  Interface 1
    Altset 1
    Format: S16_LE
    Channels: 2
    Endpoint: 0x01 (1 OUT) (ASYNC)
    Rates: 44100, 48000, 88200, 96000
    Data packet interval: 125 us
    Bits: 16
    Channel map: FL FR
  Interface 1
    Altset 2
    Format: S24_3LE
    Channels: 2
    Endpoint: 0x01 (1 OUT) (ASYNC)
    Rates: 44100, 48000, 88200, 96000, 176400, 192000
    Data packet interval: 125 us
    Bits: 24
    Channel map: FL FR

Capture:
  Status: Stop
  Interface 2
    Altset 1
    Format: S32_LE
    Channels: 8
    Endpoint: 0x82 (2 IN) (ASYNC)
    Rates: 44100, 48000, 352800, 384000
    Data packet interval: 125 us
    Bits: 32
";

    /// A DAC the kernel's quirk table gives raw DSD (FiiO: QUIRK_FLAG_DSD_RAW): PCM altsets up
    /// to 768 kHz, and one DSD_U32_BE altset whose rates are DSD64–DSD512 card rates.
    const WITH_DSD: &str = "\
FiiO FiiO Q7 at usb-0000:00:14.0-1, high speed : USB Audio

Playback:
  Status: Stop
  Interface 1
    Altset 1
    Format: S16_LE
    Channels: 2
    Endpoint: 0x01 (1 OUT) (ASYNC)
    Rates: 44100, 48000, 88200, 96000, 176400, 192000, 352800, 384000
    Data packet interval: 125 us
    Bits: 16
    Channel map: FL FR
    Sync Endpoint: 0x81 (1 IN)
    Sync EP Interface: 1
    Sync EP Altset: 1
    Implicit Feedback Mode: No
  Interface 1
    Altset 2
    Format: S24_3LE
    Channels: 2
    Endpoint: 0x01 (1 OUT) (ASYNC)
    Rates: 44100, 48000, 88200, 96000, 176400, 192000, 352800, 384000
    Data packet interval: 125 us
    Bits: 24
    Channel map: FL FR
  Interface 1
    Altset 3
    Format: S32_LE
    Channels: 2
    Endpoint: 0x01 (1 OUT) (ASYNC)
    Rates: 44100, 48000, 88200, 96000, 176400, 192000, 352800, 384000, 705600, 768000
    Data packet interval: 125 us
    Bits: 32
    Channel map: FL FR
  Interface 1
    Altset 4
    Format: DSD_U32_BE
    Channels: 2
    Endpoint: 0x01 (1 OUT) (ASYNC)
    Rates: 88200, 96000, 176400, 192000, 352800, 384000, 705600, 768000
    Data packet interval: 125 us
    Bits: 32
    DSD raw: DOP=0, bitrev=0
    Channel map: FL FR

Capture:
  Status: Stop
  Interface 2
    Altset 1
    Format: DSD_U8
    Channels: 2
    Rates: 352800
";

    #[test]
    fn formats_rates_and_order() {
        let u32_be = DsdFormat::U32Be;
        assert_eq!(u32_be.out_format(), OutFormat::Dsd { bytes: 4, big_endian: true, lsb_first: false });
        assert_eq!(DsdFormat::U32Le.out_format(), OutFormat::Dsd { bytes: 4, big_endian: false, lsb_first: false });
        assert_eq!(DsdFormat::U16Be.out_format(), OutFormat::Dsd { bytes: 2, big_endian: true, lsb_first: false });
        assert_eq!(DsdFormat::U16Le.out_format(), OutFormat::Dsd { bytes: 2, big_endian: false, lsb_first: false });
        assert_eq!(DsdFormat::U8.out_format(), OutFormat::Dsd { bytes: 1, big_endian: true, lsb_first: false });
        for f in DsdFormat::PREFERENCE {
            assert_eq!(DsdFormat::from_name(f.name()), Some(f));
            for r in DSD_RATES {
                assert_eq!(f.dsd_rate(f.device_rate(r)), r, "{f:?} {r}");
            }
        }
        assert_eq!(DsdFormat::from_name("S32_LE"), None);
        // DSD64 at 88.2, 176.4 and 352.8 kHz; DSD512 at 705.6 kHz in DSD_U32; the 48 kHz family.
        assert_eq!(u32_be.device_rate(2_822_400), 88_200);
        assert_eq!(DsdFormat::U16Le.device_rate(2_822_400), 176_400);
        assert_eq!(DsdFormat::U8.device_rate(2_822_400), 352_800);
        assert_eq!(u32_be.device_rate(22_579_200), 705_600);
        assert_eq!(u32_be.device_rate(3_072_000), 96_000);
        assert_eq!(DsdFormat::U8.device_rate(49_152_000), 6_144_000);
        assert_eq!(u32_be.device_rates().collect::<Vec<_>>(), [88_200, 96_000, 176_400, 192_000, 352_800, 384_000, 705_600, 768_000, 1_411_200, 1_536_000]);
        assert!(is_dsd_rate(45_158_400) && !is_dsd_rate(90_316_800) && !is_dsd_rate(2_822_401) && !is_dsd_rate(1_411_200));
    }

    #[test]
    fn the_preferred_format_wins_and_odd_rates_are_ignored() {
        let mut n = NativeDsd::default();
        assert!(n.is_empty());
        n.add(DsdFormat::U8, 2_822_400);
        n.add(DsdFormat::U8, 5_644_800);
        n.add(DsdFormat::U32Le, 5_644_800);
        n.add(DsdFormat::U32Be, 22_579_200);
        n.add(DsdFormat::U32Be, 2_822_400);
        n.add(DsdFormat::U32Be, 2_822_400);
        n.add(DsdFormat::U16Be, 1_234_567); // no DSD rate: left out, and no empty entry
        assert_eq!(n.formats.iter().map(|(f, _)| *f).collect::<Vec<_>>(), [DsdFormat::U32Be, DsdFormat::U32Le, DsdFormat::U8]);
        assert_eq!(n.rates(DsdFormat::U32Be), [2_822_400, 22_579_200]);
        assert_eq!(n.rates(DsdFormat::U16Be), [] as [u32; 0]);
        assert_eq!(n.format_for(2_822_400), Some(DsdFormat::U32Be));
        assert_eq!(n.format_for(5_644_800), Some(DsdFormat::U32Le));
        assert_eq!(n.format_for(11_289_600), None);
        // The plan's word rate (dsd_rate / 16) → the format and its card rate exactly.
        assert_eq!(native_open(&n, 176_400.0), Some((DsdFormat::U32Be, 88_200)));
        assert_eq!(native_open(&n, 352_800.0), Some((DsdFormat::U32Le, 176_400)));
        assert_eq!(native_open(&n, 1_411_200.0), Some((DsdFormat::U32Be, 705_600)));
        assert_eq!(native_open(&n, 705_600.0), None);
        let mut only_u8 = NativeDsd::default();
        only_u8.add(DsdFormat::U8, 2_822_400);
        assert_eq!(native_open(&only_u8, 176_400.0), Some((DsdFormat::U8, 352_800)));
    }

    #[test]
    fn a_pcm_only_card_has_no_native_dsd() {
        let caps = parse_stream(PCM_ONLY, &PCM).unwrap();
        assert_eq!(caps.pcm_rates, [44_100, 48_000, 88_200, 96_000, 176_400, 192_000]);
        assert!(caps.wide_int);
        assert_eq!(caps.max_channels, 2, "the capture side's 8 channels don't count");
        assert!(caps.native.is_empty());
    }

    #[test]
    fn a_dsd_altset_gives_dsd_rates_and_no_pcm_rates() {
        let caps = parse_stream(WITH_DSD, &PCM).unwrap();
        // The PCM altsets' rates only: S32_LE goes to 768 kHz; the DSD altset adds nothing.
        assert_eq!(caps.pcm_rates, PCM);
        assert!(caps.wide_int);
        assert_eq!(caps.max_channels, 2);
        assert_eq!(caps.native.rates(DsdFormat::U32Be), [2_822_400, 3_072_000, 5_644_800, 6_144_000, 11_289_600, 12_288_000, 22_579_200, 24_576_000]);
        assert_eq!(caps.native.format_for(22_579_200), Some(DsdFormat::U32Be));
        assert_eq!(caps.native.format_for(45_158_400), None, "DSD1024 isn't listed");
        assert_eq!(caps.native.rates(DsdFormat::U8), [] as [u32; 0], "capture's DSD_U8 is no playback format");

        // A card whose DSD altset reaches a rate no PCM altset has: it stays out of the PCM
        // list. And one altset with both a PCM and a DSD format counts for both.
        let text = "\
Playback:
  Interface 1
    Altset 1
    Format: S24_3LE
    Channels: 2
    Rates: 44100, 48000, 88200, 96000
  Interface 1
    Altset 2
    Format: DSD_U32_BE
    Channels: 2
    Rates: 176400, 352800, 705600
  Interface 1
    Altset 3
    Format: S32_LE DSD_U16_LE
    Channels: 6
    Rates: 176400, 192000
";
        let caps = parse_stream(text, &PCM).unwrap();
        assert_eq!(caps.pcm_rates, [44_100, 48_000, 88_200, 96_000, 176_400, 192_000]);
        assert!(!caps.pcm_rates.contains(&352_800) && !caps.pcm_rates.contains(&705_600));
        assert_eq!(caps.max_channels, 6);
        assert_eq!(caps.native.rates(DsdFormat::U32Be), [5_644_800, 11_289_600, 22_579_200]);
        assert_eq!(caps.native.rates(DsdFormat::U16Le), [2_822_400, 3_072_000]);
        assert_eq!(caps.native.format_for(2_822_400), Some(DsdFormat::U16Le));
        assert_eq!(caps.native.format_for(5_644_800), Some(DsdFormat::U32Be));
    }

    #[test]
    fn continuous_rates_and_nothing_to_play() {
        let text = "\
Playback:
  Interface 1
    Altset 1
    Format: S32_LE
    Channels: 2
    Rates: 8000 - 192000 (continuous)
  Interface 1
    Altset 2
    Format: DSD_U32_BE
    Channels: 2
    Rates: 88200 - 200000 (continuous)
";
        let caps = parse_stream(text, &PCM).unwrap();
        assert_eq!(caps.pcm_rates, [44_100, 48_000, 88_200, 96_000, 176_400, 192_000]);
        assert_eq!(caps.native.rates(DsdFormat::U32Be), [2_822_400, 3_072_000, 5_644_800, 6_144_000]);
        // Only DSD, or nothing at all: no device to list (as before).
        assert_eq!(parse_stream("Playback:\n  Format: DSD_U32_BE\n  Rates: 88200\n", &PCM), None);
        assert_eq!(parse_stream("", &PCM), None);
    }
}
