//! The audio stream a network player fetches from us: FLAC, WAV or raw L16, written as it
//! plays.
//!
//! FLAC is what streamers take most reliably (24-bit included), so it is the default. Every
//! frame is stored verbatim: the samples go out exactly as they are, and on a home network
//! the bandwidth doesn't matter (192 kHz / 24-bit stereo is 9.2 Mbit/s). The stream length
//! is left open (zero samples, no MD5), as the FLAC format allows for a stream.

/// How the samples are packed for the network player.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    Flac,
    Wav,
    /// Big-endian PCM with the format in the MIME type (DLNA's "LPCM").
    L16,
}

/// The format of one stream. FLAC and WAV carry 16 or 24 bits, L16 only 16.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Format {
    pub codec: Codec,
    pub rate: u32,
    pub channels: u16,
    pub bits: u16,
}

/// Frames per FLAC block (and per piece of the other codecs).
pub const BLOCK: usize = 4096;

impl Format {
    pub fn mime(&self) -> String {
        match self.codec {
            Codec::Flac => "audio/flac".into(),
            Codec::Wav => "audio/wav".into(),
            Codec::L16 => format!("audio/L16;rate={};channels={}", self.rate, self.channels),
        }
    }

    pub fn extension(&self) -> &'static str {
        match self.codec {
            Codec::Flac => "flac",
            Codec::Wav => "wav",
            Codec::L16 => "pcm",
        }
    }

    pub fn bytes_per_second(&self) -> f64 {
        self.rate as f64 * self.channels as f64 * (self.bits / 8) as f64
    }
}

/// Writes one stream: [`Encoder::header`] once, then [`Encoder::encode`] for every block.
pub struct Encoder {
    pub format: Format,
    frame_number: u64,
}

impl Encoder {
    pub fn new(format: Format) -> Self {
        assert!(matches!(format.bits, 16 | 24), "16 or 24 bits");
        assert!(format.codec != Codec::L16 || format.bits == 16, "L16 is 16-bit");
        assert!((1..=8).contains(&format.channels));
        Self { format, frame_number: 0 }
    }

    /// What goes before the first block.
    pub fn header(&self) -> Vec<u8> {
        let f = self.format;
        match f.codec {
            Codec::Flac => {
                let mut h = b"fLaC".to_vec();
                // STREAMINFO, the last (only) metadata block, 34 bytes.
                h.extend_from_slice(&[0x80, 0, 0, 34]);
                let mut w = Bits::default();
                w.put(BLOCK as u64, 16); // min block size
                w.put(BLOCK as u64, 16); // max block size
                w.put(0, 24); // min frame size: unknown
                w.put(0, 24); // max frame size: unknown
                w.put(f.rate as u64, 20);
                w.put(f.channels as u64 - 1, 3);
                w.put(f.bits as u64 - 1, 5);
                w.put(0, 36); // total samples: unknown (a stream)
                h.extend_from_slice(&w.bytes);
                h.extend_from_slice(&[0; 16]); // MD5: unknown
                h
            }
            Codec::Wav => {
                // Sizes as large as they go: the stream has no end we know of.
                let block_align = f.channels as u32 * (f.bits as u32 / 8);
                let mut h = Vec::with_capacity(44);
                h.extend_from_slice(b"RIFF");
                h.extend_from_slice(&(u32::MAX - 8).to_le_bytes());
                h.extend_from_slice(b"WAVEfmt ");
                h.extend_from_slice(&16u32.to_le_bytes());
                h.extend_from_slice(&1u16.to_le_bytes()); // PCM
                h.extend_from_slice(&f.channels.to_le_bytes());
                h.extend_from_slice(&f.rate.to_le_bytes());
                h.extend_from_slice(&(f.rate * block_align).to_le_bytes());
                h.extend_from_slice(&(block_align as u16).to_le_bytes());
                h.extend_from_slice(&f.bits.to_le_bytes());
                h.extend_from_slice(b"data");
                h.extend_from_slice(&(u32::MAX - 44).to_le_bytes());
                h
            }
            Codec::L16 => Vec::new(),
        }
    }

    /// Append one block of interleaved samples (integers in the format's bit depth) to `out`.
    /// Blocks other than the last must be [`BLOCK`] frames long.
    pub fn encode(&mut self, samples: &[i32], out: &mut Vec<u8>) {
        let f = self.format;
        let ch = f.channels as usize;
        debug_assert_eq!(samples.len() % ch, 0);
        let frames = samples.len() / ch;
        if frames == 0 {
            return;
        }
        let bytes = (f.bits / 8) as usize;
        match f.codec {
            Codec::Wav => {
                for &s in samples {
                    out.extend_from_slice(&s.to_le_bytes()[..bytes]);
                }
            }
            Codec::L16 => {
                for &s in samples {
                    out.extend_from_slice(&(s as i16).to_be_bytes());
                }
            }
            Codec::Flac => {
                debug_assert!(frames <= BLOCK);
                let start = out.len();
                let mut w = Bits::default();
                w.put(0b11_1111_1111_1110, 14); // sync
                w.put(0, 1); // reserved
                w.put(0, 1); // fixed block size: frames are numbered
                let (size_code, size_tail) = match frames {
                    BLOCK => (0b1100, None), // 4096
                    n if n <= 256 => (0b0110, Some(((n - 1) as u64, 8))),
                    n => (0b0111, Some(((n - 1) as u64, 16))),
                };
                w.put(size_code, 4);
                let (rate_code, rate_tail) = rate_code(f.rate);
                w.put(rate_code, 4);
                w.put(ch as u64 - 1, 4); // independent channels
                w.put(if f.bits == 16 { 0b100 } else { 0b110 }, 3);
                w.put(0, 1); // reserved
                w.bytes.extend_from_slice(&utf8_number(self.frame_number));
                if let Some((v, n)) = size_tail {
                    w.put(v, n);
                }
                if let Some((v, n)) = rate_tail {
                    w.put(v, n);
                }
                let crc = crc8(&w.bytes);
                out.extend_from_slice(&w.bytes);
                out.push(crc);
                for c in 0..ch {
                    out.push(0b0000_0010); // VERBATIM subframe, no wasted bits
                    for frame in samples.chunks_exact(ch) {
                        out.extend_from_slice(&frame[c].to_be_bytes()[4 - bytes..]);
                    }
                }
                let crc = crc16(&out[start..]);
                out.extend_from_slice(&crc.to_be_bytes());
                self.frame_number += 1;
            }
        }
    }
}

/// The frame header's sample rate code, and the extra bits some rates need after it.
fn rate_code(rate: u32) -> (u64, Option<(u64, u32)>) {
    match rate {
        88_200 => (0b0001, None),
        176_400 => (0b0010, None),
        192_000 => (0b0011, None),
        8_000 => (0b0100, None),
        16_000 => (0b0101, None),
        22_050 => (0b0110, None),
        24_000 => (0b0111, None),
        32_000 => (0b1000, None),
        44_100 => (0b1001, None),
        48_000 => (0b1010, None),
        96_000 => (0b1011, None),
        r if r % 10 == 0 && r / 10 <= 0xFFFF => (0b1110, Some(((r / 10) as u64, 16))),
        r if r <= 0xFFFF => (0b1101, Some((r as u64, 16))),
        // Above 655.35 kHz (705.6, 768): read from STREAMINFO.
        _ => (0b0000, None),
    }
}

/// FLAC's frame number coding (the same scheme as UTF-8, extended to 36 bits).
fn utf8_number(n: u64) -> Vec<u8> {
    if n < 0x80 {
        return vec![n as u8];
    }
    let mut tail = Vec::new();
    let mut v = n;
    let mut room = 6u32; // value bits left for the first byte
    while v >= 1 << room {
        tail.push(0x80 | (v & 0x3F) as u8);
        v >>= 6;
        room -= 1;
    }
    let len = tail.len() + 1;
    let lead = !(0xFFu8 >> len) | v as u8;
    let mut out = vec![lead];
    out.extend(tail.into_iter().rev());
    out
}

fn crc8(data: &[u8]) -> u8 {
    data.iter().fold(0u8, |mut crc, &b| {
        crc ^= b;
        for _ in 0..8 {
            crc = if crc & 0x80 != 0 { (crc << 1) ^ 0x07 } else { crc << 1 };
        }
        crc
    })
}

fn crc16(data: &[u8]) -> u16 {
    data.iter().fold(0u16, |mut crc, &b| {
        crc ^= (b as u16) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 { (crc << 1) ^ 0x8005 } else { crc << 1 };
        }
        crc
    })
}

/// MSB-first bit writer.
#[derive(Default)]
struct Bits {
    bytes: Vec<u8>,
    used: u32, // bits used in the last byte (0 = none / full)
}

impl Bits {
    fn put(&mut self, value: u64, n: u32) {
        for i in (0..n).rev() {
            if self.used == 0 {
                self.bytes.push(0);
            }
            let bit = ((value >> i) & 1) as u8;
            let last = self.bytes.last_mut().expect("pushed");
            *last |= bit << (7 - self.used);
            self.used = (self.used + 1) % 8;
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use symphonia::core::codecs::audio::AudioDecoderOptions;
    use symphonia::core::formats::probe::Hint;
    use symphonia::core::formats::{FormatOptions, TrackType};
    use symphonia::core::io::{MediaSourceStream, MediaSourceStreamOptions};
    use symphonia::core::meta::MetadataOptions;

    /// Decode a stream with symphonia; returns the samples as integers of `bits`.
    pub(crate) fn decode(bytes: Vec<u8>, ext: &str, bits: u32) -> (u32, Vec<i32>) {
        let mss = MediaSourceStream::new(Box::new(std::io::Cursor::new(bytes)), MediaSourceStreamOptions::default());
        let mut hint = Hint::new();
        hint.with_extension(ext);
        let mut format = symphonia::default::get_probe()
            .probe(&hint, mss, FormatOptions::default(), MetadataOptions::default())
            .expect("probe");
        let track = format.default_track(TrackType::Audio).expect("track").clone();
        let params = track.codec_params.as_ref().and_then(|p| p.audio()).expect("audio").clone();
        let rate = params.sample_rate.expect("rate");
        let mut dec = symphonia::default::get_codecs()
            .make_audio_decoder(&params, &AudioDecoderOptions::default())
            .expect("decoder");
        let mut out = Vec::new();
        let full = (1i64 << (bits - 1)) as f64;
        while let Ok(Some(packet)) = format.next_packet() {
            let buf = dec.decode(&packet).expect("decode");
            let mut f = vec![0f32; buf.samples_interleaved()];
            buf.copy_to_slice_interleaved(&mut f);
            out.extend(f.iter().map(|&s| (s as f64 * full).round() as i32));
        }
        (rate, out)
    }

    fn noise(frames: usize, ch: usize, bits: u32) -> Vec<i32> {
        let mut x = 0x1234_5678u32;
        let max = 1i32 << (bits - 1);
        (0..frames * ch)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                (x as i32 >> (32 - bits)).clamp(-max, max - 1)
            })
            .collect()
    }

    fn roundtrip(codec: Codec, rate: u32, bits: u16, frames: usize) {
        let format = Format { codec, rate, channels: 2, bits };
        let samples = noise(frames, 2, bits as u32);
        let mut enc = Encoder::new(format);
        let mut bytes = enc.header();
        for block in samples.chunks(BLOCK * 2) {
            enc.encode(block, &mut bytes);
        }
        let (got_rate, got) = decode(bytes, format.extension(), bits as u32);
        assert_eq!(got_rate, rate);
        assert_eq!(got.len(), samples.len(), "{codec:?} {rate} {bits}");
        assert!(got == samples, "{codec:?} {rate}/{bits}: samples differ");
    }

    #[test]
    fn flac_frames_decode_to_the_exact_samples() {
        roundtrip(Codec::Flac, 44_100, 16, BLOCK * 3 + 100);
        roundtrip(Codec::Flac, 96_000, 24, BLOCK * 2 + 300);
        // Rates written after the header, in tens of Hz and in Hz.
        roundtrip(Codec::Flac, 352_800, 24, BLOCK + 1);
        roundtrip(Codec::Flac, 11_025, 16, BLOCK);
    }

    #[test]
    fn wav_decodes_to_the_exact_samples() {
        roundtrip(Codec::Wav, 48_000, 24, BLOCK * 2 + 7);
        roundtrip(Codec::Wav, 44_100, 16, 1000);
    }

    #[test]
    fn frame_numbers_use_the_utf8_scheme() {
        assert_eq!(utf8_number(0x7F), [0x7F]);
        assert_eq!(utf8_number(0x80), [0xC2, 0x80]);
        assert_eq!(utf8_number(0x800), [0xE0, 0xA0, 0x80]);
        assert_eq!(utf8_number(0x10000), [0xF0, 0x90, 0x80, 0x80]);
    }
}
