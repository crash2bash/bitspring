// SPDX-License-Identifier: GPL-3.0-only
//
// bitspring-asio: a small, safe ASIO host for BitSpring.
//
// The ASIO interface used here is translated from the Steinberg ASIO SDK
// 2.3.4, which Steinberg licenses under its own terms or, alternatively, the
// GNU General Public License version 3. This crate uses the GPLv3 option, so
// it is GPL-3.0-only (see LICENSE and README.md); the rest of the BitSpring
// repository stays MIT.
//
// ASIO is a registered trademark of Steinberg Media Technologies GmbH.

//! A small, safe ASIO host for Windows.
//!
//! [`drivers`] lists the installed ASIO drivers. [`Host`] owns one thread on
//! which a driver is loaded and every driver call is made; through it a
//! driver is opened, described, probed for PCM and DSD rates, and streamed
//! to. The audio itself is produced by a render closure the owner hands to
//! [`Host::start`], called from the driver's realtime callback.
//!
//! On systems other than Windows only the platform-independent parts are
//! built: sample formats, the buffer size rules, the DSD rate list, driver id
//! parsing and the plain data types. They are what the unit tests cover.

#![warn(missing_docs)]

pub mod sys;

#[cfg(windows)]
mod host;
#[cfg(windows)]
pub use host::{Host, drivers};

use std::fmt;

/// The largest number of output channels one stream may use. The realtime
/// callback hands the render closure its channel buffers through a
/// fixed-size array on the stack, so it never allocates; this is its size.
pub const MAX_CHANNELS: usize = 64;

/// The PCM rates [`Host::probe`] asks the driver about, in hertz.
pub const PCM_RATES: [u32; 10] = [
    44_100, 48_000, 88_200, 96_000, 176_400, 192_000, 352_800, 384_000, 705_600, 768_000,
];

/// The DSD rates (bit rates per channel, in hertz), DSD64 to DSD512 in both
/// families: multiples of 44.1 kHz (2.8224 MHz times 2^k) and of 48 kHz
/// (3.072 MHz times 2^k), ascending. In DSD mode an ASIO driver's "sample
/// rate" is this bit rate: DSD64 is `setSampleRate(2822400.0)`.
pub const DSD_RATES: [u32; 8] = [
    2_822_400, 3_072_000, // DSD64
    5_644_800, 6_144_000, // DSD128
    11_289_600, 12_288_000, // DSD256
    22_579_200, 24_576_000, // DSD512
];

/// Whether `rate` (in hertz) is one of [`DSD_RATES`].
pub fn is_dsd_rate(rate: f64) -> bool {
    DSD_RATES.iter().any(|&r| rates_equal(r as f64, rate))
}

/// Whether two sample rates are the same for ASIO's purposes. Drivers report
/// rates as `f64` and some return values a hair away from the one set, so
/// rates within half a hertz count as equal.
pub fn rates_equal(a: f64, b: f64) -> bool {
    (a - b).abs() < 0.5
}

/// The format of the samples in an ASIO channel buffer (`ASIOSampleType`).
///
/// The `Msb` types are big-endian and the `Lsb` types little-endian. The
/// `Int32*16/18/20/24` types hold a smaller sample in a 32-bit word, aligned
/// to the low bits. The DSD types carry 1-bit samples.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SampleType {
    /// 16-bit integer, big-endian.
    Int16Msb,
    /// 24-bit integer packed in 3 bytes, big-endian.
    Int24Msb,
    /// 32-bit integer, big-endian.
    Int32Msb,
    /// 32-bit float, big-endian.
    Float32Msb,
    /// 64-bit float, big-endian.
    Float64Msb,
    /// 16-bit sample in a 32-bit big-endian word.
    Int32Msb16,
    /// 18-bit sample in a 32-bit big-endian word.
    Int32Msb18,
    /// 20-bit sample in a 32-bit big-endian word.
    Int32Msb20,
    /// 24-bit sample in a 32-bit big-endian word.
    Int32Msb24,
    /// 16-bit integer, little-endian.
    Int16Lsb,
    /// 24-bit integer packed in 3 bytes, little-endian.
    Int24Lsb,
    /// 32-bit integer, little-endian (the most common ASIO format).
    Int32Lsb,
    /// 32-bit float, little-endian.
    Float32Lsb,
    /// 64-bit float, little-endian.
    Float64Lsb,
    /// 16-bit sample in a 32-bit little-endian word.
    Int32Lsb16,
    /// 18-bit sample in a 32-bit little-endian word.
    Int32Lsb18,
    /// 20-bit sample in a 32-bit little-endian word.
    Int32Lsb20,
    /// 24-bit sample in a 32-bit little-endian word.
    Int32Lsb24,
    /// DSD, 8 one-bit samples per byte, the first sample in the least
    /// significant bit.
    DsdInt8Lsb1,
    /// DSD, 8 one-bit samples per byte, the first sample in the most
    /// significant bit (the order of DSF/DFF files and of DoP).
    DsdInt8Msb1,
    /// DSD, one sample per byte. Not supported for streaming by this crate.
    DsdInt8Ner8,
    /// A value this crate does not know.
    Unknown(i32),
}

impl SampleType {
    /// Decodes the SDK's numeric sample type.
    pub fn from_raw(raw: i32) -> SampleType {
        use SampleType::*;
        match raw {
            0 => Int16Msb,
            1 => Int24Msb,
            2 => Int32Msb,
            3 => Float32Msb,
            4 => Float64Msb,
            8 => Int32Msb16,
            9 => Int32Msb18,
            10 => Int32Msb20,
            11 => Int32Msb24,
            16 => Int16Lsb,
            17 => Int24Lsb,
            18 => Int32Lsb,
            19 => Float32Lsb,
            20 => Float64Lsb,
            24 => Int32Lsb16,
            25 => Int32Lsb18,
            26 => Int32Lsb20,
            27 => Int32Lsb24,
            32 => DsdInt8Lsb1,
            33 => DsdInt8Msb1,
            40 => DsdInt8Ner8,
            other => Unknown(other),
        }
    }

    /// The SDK's numeric value for this type.
    pub fn raw(self) -> i32 {
        use SampleType::*;
        match self {
            Int16Msb => 0,
            Int24Msb => 1,
            Int32Msb => 2,
            Float32Msb => 3,
            Float64Msb => 4,
            Int32Msb16 => 8,
            Int32Msb18 => 9,
            Int32Msb20 => 10,
            Int32Msb24 => 11,
            Int16Lsb => 16,
            Int24Lsb => 17,
            Int32Lsb => 18,
            Float32Lsb => 19,
            Float64Lsb => 20,
            Int32Lsb16 => 24,
            Int32Lsb18 => 25,
            Int32Lsb20 => 26,
            Int32Lsb24 => 27,
            DsdInt8Lsb1 => 32,
            DsdInt8Msb1 => 33,
            DsdInt8Ner8 => 40,
            Unknown(raw) => raw,
        }
    }

    /// Whether this is one of the DSD types.
    pub fn is_dsd(self) -> bool {
        matches!(
            self,
            SampleType::DsdInt8Lsb1 | SampleType::DsdInt8Msb1 | SampleType::DsdInt8Ner8
        )
    }

    /// Whether the multi-byte samples of this type are big-endian.
    pub fn is_big_endian(self) -> bool {
        matches!(self.raw(), 0..=11)
    }

    /// Bytes per sample for the PCM types; `None` for DSD and unknown types.
    pub fn bytes_per_sample(self) -> Option<usize> {
        use SampleType::*;
        match self {
            Int16Msb | Int16Lsb => Some(2),
            Int24Msb | Int24Lsb => Some(3),
            Int32Msb | Int32Lsb | Float32Msb | Float32Lsb | Int32Msb16 | Int32Msb18
            | Int32Msb20 | Int32Msb24 | Int32Lsb16 | Int32Lsb18 | Int32Lsb20 | Int32Lsb24 => {
                Some(4)
            }
            Float64Msb | Float64Lsb => Some(8),
            DsdInt8Lsb1 | DsdInt8Msb1 | DsdInt8Ner8 | Unknown(_) => None,
        }
    }

    /// Bytes in one half of one channel's double buffer, for an ASIO buffer
    /// size of `buffer_size` samples. For PCM that is samples times bytes per
    /// sample. For the 1-bit DSD types the buffer size counts 1-bit samples,
    /// 8 to a byte, so it is `buffer_size / 8` (`None` unless it divides
    /// evenly); for `DsdInt8Ner8` it is one byte per sample. `None` for
    /// unknown types and sizes that are not positive.
    pub fn bytes_per_buffer(self, buffer_size: i32) -> Option<usize> {
        let size = usize::try_from(buffer_size).ok().filter(|&s| s > 0)?;
        match self {
            SampleType::DsdInt8Lsb1 | SampleType::DsdInt8Msb1 => {
                (size % 8 == 0).then_some(size / 8)
            }
            SampleType::DsdInt8Ner8 => Some(size),
            other => other.bytes_per_sample().map(|b| b * size),
        }
    }

    /// The byte that fills a buffer with silence. Zero for PCM. For 1-bit
    /// DSD it is the idle pattern 0x69 (0110 1001, as many ones as zeros)
    /// in most-significant-bit-first order, and the same bit sequence
    /// stored least significant bit first (0x96) for `DsdInt8Lsb1`.
    pub fn silence_byte(self) -> u8 {
        match self {
            SampleType::DsdInt8Msb1 | SampleType::DsdInt8Ner8 => 0x69,
            SampleType::DsdInt8Lsb1 => 0x96,
            _ => 0,
        }
    }
}

/// An ASIO error code, decoded (`ASIOError`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AsioError {
    /// `ASE_OK`: success.
    Ok,
    /// `ASE_SUCCESS`: the success value of `future()` calls.
    Success,
    /// `ASE_NotPresent`: no such input or output, or not available.
    NotPresent,
    /// `ASE_HWMalfunction`: the hardware is malfunctioning.
    HwMalfunction,
    /// `ASE_InvalidParameter`: a parameter is invalid.
    InvalidParameter,
    /// `ASE_InvalidMode`: the hardware is in, or used in, a bad mode.
    InvalidMode,
    /// `ASE_SPNotAdvancing`: the hardware is not running.
    SpNotAdvancing,
    /// `ASE_NoClock`: the sample clock or rate is not available.
    NoClock,
    /// `ASE_NoMemory`: not enough memory.
    NoMemory,
    /// A code the SDK does not define.
    Other(i32),
}

impl AsioError {
    /// Decodes a raw `ASIOError` value.
    pub fn from_raw(code: i32) -> AsioError {
        match code {
            sys::ASE_OK => AsioError::Ok,
            sys::ASE_SUCCESS => AsioError::Success,
            sys::ASE_NOT_PRESENT => AsioError::NotPresent,
            sys::ASE_HW_MALFUNCTION => AsioError::HwMalfunction,
            sys::ASE_INVALID_PARAMETER => AsioError::InvalidParameter,
            sys::ASE_INVALID_MODE => AsioError::InvalidMode,
            sys::ASE_SP_NOT_ADVANCING => AsioError::SpNotAdvancing,
            sys::ASE_NO_CLOCK => AsioError::NoClock,
            sys::ASE_NO_MEMORY => AsioError::NoMemory,
            other => AsioError::Other(other),
        }
    }
}

impl fmt::Display for AsioError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AsioError::Ok => f.write_str("ok"),
            AsioError::Success => f.write_str("success"),
            AsioError::NotPresent => f.write_str("hardware not present or not available"),
            AsioError::HwMalfunction => f.write_str("hardware malfunction"),
            AsioError::InvalidParameter => f.write_str("invalid parameter"),
            AsioError::InvalidMode => f.write_str("invalid mode"),
            AsioError::SpNotAdvancing => f.write_str("hardware not running"),
            AsioError::NoClock => f.write_str("no sample clock"),
            AsioError::NoMemory => f.write_str("out of memory"),
            AsioError::Other(code) => write!(f, "error {code}"),
        }
    }
}

/// Whether a `future()` call succeeded. The SDK says success is
/// `ASE_SUCCESS` and that `ASE_OK` is not enough, but some drivers return
/// `ASE_OK`, so both count.
pub fn future_succeeded(code: i32) -> bool {
    code == sys::ASE_SUCCESS || code == sys::ASE_OK
}

/// What went wrong in a [`Host`] call.
#[derive(Clone, Debug, PartialEq)]
pub enum Error {
    /// A driver is already loaded in this process (by any `Host`). ASIO
    /// allows only one at a time; close it first.
    AlreadyLoaded,
    /// No driver is open.
    NotOpen,
    /// A stream is running; stop it first.
    Running,
    /// The driver id is not a CLSID like `{xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx}`.
    BadClsid(String),
    /// A Windows call failed, with its HRESULT or error code.
    Os {
        /// The Windows function that failed.
        call: &'static str,
        /// Its HRESULT or Win32 error code.
        code: i32,
    },
    /// The driver could not be created (it is not registered, or its DLL
    /// failed to load), with the HRESULT of `CoCreateInstance`.
    CreateFailed(i32),
    /// The driver's `init` failed; the text is the driver's own error
    /// message (often "no device connected" or similar).
    InitFailed(String),
    /// An ASIO call failed.
    Asio {
        /// The IASIO method that failed.
        call: &'static str,
        /// What it returned.
        error: AsioError,
    },
    /// The driver accepted a sample rate but kept running at another one.
    RateNotApplied {
        /// The rate asked for.
        wanted: f64,
        /// The rate the driver reports.
        got: f64,
    },
    /// The request cannot be met by this driver or this crate; the text
    /// says why.
    Unsupported(String),
    /// The stream configuration is invalid; the text says why.
    InvalidConfig(String),
    /// The host thread is gone (it failed to start or has panicked).
    HostGone,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::AlreadyLoaded => f.write_str("another ASIO driver is already loaded"),
            Error::NotOpen => f.write_str("no ASIO driver is open"),
            Error::Running => f.write_str("the ASIO stream is running"),
            Error::BadClsid(s) => write!(f, "not an ASIO driver id: {s:?}"),
            Error::Os { call, code } => write!(f, "{call} failed (0x{code:08x})"),
            Error::CreateFailed(hr) => {
                write!(f, "the ASIO driver could not be loaded (0x{hr:08x})")
            }
            Error::InitFailed(msg) if msg.is_empty() => {
                f.write_str("the ASIO driver failed to start")
            }
            Error::InitFailed(msg) => write!(f, "the ASIO driver failed to start: {msg}"),
            Error::Asio { call, error } => write!(f, "ASIO {call} failed: {error}"),
            Error::RateNotApplied { wanted, got } => {
                write!(
                    f,
                    "the ASIO driver stays at {got} Hz instead of {wanted} Hz"
                )
            }
            Error::Unsupported(why) => write!(f, "not supported by the ASIO driver: {why}"),
            Error::InvalidConfig(why) => write!(f, "invalid ASIO stream settings: {why}"),
            Error::HostGone => f.write_str("the ASIO host thread has stopped"),
        }
    }
}

impl std::error::Error for Error {}

/// One installed ASIO driver, as listed by [`drivers`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DriverEntry {
    /// The display name (the registry `Description`, else the key name).
    pub name: String,
    /// The driver's CLSID as written in the registry, `{...}`; pass it to
    /// [`Host::open`].
    pub clsid: String,
}

/// The buffer sizes a driver allows, in samples per channel
/// (`getBufferSize`). In DSD mode the samples are 1-bit samples.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BufferSizes {
    /// The smallest size.
    pub min: i32,
    /// The largest size.
    pub max: i32,
    /// The size the driver (or its control panel setting) prefers.
    pub preferred: i32,
    /// The step between sizes: -1 means powers of two, 0 means only the
    /// fixed size, a positive value means `min + k * granularity`.
    pub granularity: i32,
}

/// One output channel of a driver.
#[derive(Clone, Debug, PartialEq)]
pub struct ChannelInfo {
    /// The channel number to put in [`StreamConfig::channels`].
    pub channel: i32,
    /// The driver's name for it, such as "Analog Out 1".
    pub name: String,
    /// The channel group (analog, S/PDIF and so on, driver-defined).
    pub group: i32,
    /// Its sample type in the driver's current mode.
    pub sample_type: SampleType,
}

/// What [`Host::info`] reports about the open driver, in its current mode.
#[derive(Clone, Debug, PartialEq)]
pub struct DriverInfo {
    /// The driver's own name (`getDriverName`).
    pub name: String,
    /// The driver's version number.
    pub version: i32,
    /// The number of input channels.
    pub input_channels: i32,
    /// The number of output channels.
    pub output_channels: i32,
    /// Every output channel.
    pub outputs: Vec<ChannelInfo>,
    /// The allowed buffer sizes.
    pub buffer_sizes: BufferSizes,
    /// The input latency in samples, if the driver reports one. The SDK
    /// says latencies are only exact once buffers exist.
    pub input_latency: Option<i32>,
    /// The output latency in samples, if the driver reports one.
    pub output_latency: Option<i32>,
    /// The current sample rate, if the driver reports one.
    pub sample_rate: Option<f64>,
}

/// What [`Host::probe`] found out about the open driver.
#[derive(Clone, Debug, PartialEq)]
pub struct Caps {
    /// The rates of [`PCM_RATES`] the driver accepts.
    pub pcm_rates: Vec<u32>,
    /// The rates of [`DSD_RATES`] the driver accepts in DSD mode; empty if
    /// it has no native DSD mode.
    pub dsd_rates: Vec<u32>,
    /// The number of output channels.
    pub output_channels: i32,
    /// The sample type of each output channel in PCM mode.
    pub pcm_types: Vec<SampleType>,
    /// The sample type of each output channel in DSD mode (empty without
    /// DSD).
    pub dsd_types: Vec<SampleType>,
    /// The driver answers the IoFormat "can do" call for DSD: it says it has
    /// a native DSD mode (which [`Host::probe_dsd`] then checks).
    pub can_dsd: bool,
}

/// What [`Host::probe_dsd`] found out about the open driver's DSD mode.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DsdCaps {
    /// The rates of [`DSD_RATES`] the driver accepts in DSD mode; empty if
    /// it has no usable DSD mode.
    pub rates: Vec<u32>,
    /// The sample type of each output channel in DSD mode (empty without
    /// DSD).
    pub types: Vec<SampleType>,
}

/// The render closure of a stream. It is called from the driver's realtime
/// callback with one byte slice per stream channel (in the order of
/// [`StreamConfig::channels`]), each holding one buffer half in the
/// driver's sample format ([`StreamInfo::sample_type`]), and must fill them
/// all. It must not block, allocate if avoidable, or call back into the
/// [`Host`]: the host may be waiting for this very callback to finish.
pub type RenderFn = Box<dyn FnMut(&mut [&mut [u8]]) + Send>;

/// How to start a stream with [`Host::start`].
pub struct StreamConfig {
    /// `true` for native DSD (the driver is switched to its DSD mode and
    /// `rate` is the DSD bit rate), `false` for PCM.
    pub dsd: bool,
    /// The sample rate in hertz, or the DSD bit rate (DSD64 = 2822400).
    pub rate: f64,
    /// The output channel numbers to play to, at most [`MAX_CHANNELS`].
    pub channels: Vec<i32>,
    /// The buffer size to aim for, in samples per channel (1-bit samples in
    /// DSD mode); `None` takes the driver's preferred size. The closest size
    /// the driver allows is used.
    pub buffer_size: Option<i32>,
    /// Fills each buffer half; see [`RenderFn`].
    pub render: RenderFn,
}

impl fmt::Debug for StreamConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StreamConfig")
            .field("dsd", &self.dsd)
            .field("rate", &self.rate)
            .field("channels", &self.channels)
            .field("buffer_size", &self.buffer_size)
            .finish_non_exhaustive()
    }
}

/// What a started stream looks like.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct StreamInfo {
    /// The buffer size in samples per channel (1-bit samples in DSD mode).
    pub buffer_size: i32,
    /// The length of each slice the render closure gets, in bytes.
    pub bytes_per_channel: usize,
    /// The sample format of those bytes, the same for every channel.
    pub sample_type: SampleType,
    /// The rate the driver reports running at.
    pub rate: f64,
    /// The output latency in samples, if the driver reports one.
    pub output_latency: Option<i32>,
}

/// Things the driver reported since the last [`Host::take_events`].
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Events {
    /// The driver asked to be reset (closed and opened again), or said its
    /// buffer size changed. Typically after a change in its control panel.
    /// The owner should stop, close and reopen the driver at a calm moment;
    /// the host never does it inside the driver's callback.
    pub reset_requested: bool,
    /// The driver said its rate changed to this value, different from the
    /// one the running stream was set to (0 means the clock was lost).
    pub sample_rate_changed: Option<f64>,
    /// The number of dropouts the driver reported (overloads and resync
    /// requests).
    pub dropouts: u64,
    /// The driver said its latencies changed.
    pub latencies_changed: bool,
    /// The render closure panicked; the stream plays silence from then on.
    pub render_panicked: bool,
}

/// Chooses the buffer size to use, in samples per channel, from what
/// `getBufferSize` reported.
///
/// `preferred` is used when the driver allows it; otherwise the allowed size
/// closest to it (the larger one on a tie, for safety). The allowed sizes
/// follow the granularity: -1 means the powers of two from `min` to `max`;
/// 0 (meant for `min == max`) means the fixed size, `max` and `preferred`
/// also accepted when in range; a positive step means `min + k * granularity`
/// up to `max`. Other negative values are treated like 0. For DSD
/// (`dsd == true`) the size must also be a multiple of 16, so each buffer
/// holds whole 16-bit words (`buffer_size / 8` bytes, an even number).
///
/// When the driver reports a nonsensical range (`min <= 0` or `max < min`),
/// `preferred` is trusted alone. `None` when no size fits.
pub fn choose_buffer_size(
    min: i32,
    max: i32,
    preferred: i32,
    granularity: i32,
    dsd: bool,
) -> Option<i32> {
    let fits = |s: i32| s > 0 && (!dsd || s % 16 == 0);
    if min <= 0 || max < min {
        return fits(preferred).then_some(preferred);
    }
    let target = if preferred > 0 {
        preferred.clamp(min, max)
    } else {
        min
    };
    let mut candidates: Vec<i32> = match granularity {
        -1 => (0..31)
            .map(|k| 1i32 << k)
            .filter(|&s| s >= min && s <= max)
            .collect(),
        g if g > 0 => {
            // Only the steps around the target matter: for DSD the multiple
            // of 16 repeats at least every 16 steps, so 16 steps either side
            // always contain the nearest one if there is any.
            let k0 = (target - min) / g;
            (k0.saturating_sub(16).max(0)..=k0.saturating_add(17))
                .filter_map(|k| k.checked_mul(g).and_then(|d| d.checked_add(min)))
                .filter(|&s| s <= max)
                .collect()
        }
        _ => vec![min, max, preferred],
    };
    candidates.retain(|&s| s >= min && s <= max && fits(s));
    if candidates.contains(&preferred) {
        return Some(preferred);
    }
    candidates
        .into_iter()
        .min_by_key(|&s| ((s as i64 - target as i64).abs(), std::cmp::Reverse(s)))
}

/// Parses a CLSID as written in the registry,
/// `{xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx}` (hex digits, either case; the
/// braces may be left out). `None` if it is not one.
pub fn parse_clsid(text: &str) -> Option<sys::Guid> {
    let s = text.trim();
    let s = s
        .strip_prefix('{')
        .and_then(|s| s.strip_suffix('}'))
        .unwrap_or(s);
    let parts: Vec<&str> = s.split('-').collect();
    let lens = [8, 4, 4, 4, 12];
    if parts.len() != 5
        || parts
            .iter()
            .zip(lens)
            .any(|(p, n)| p.len() != n || !p.bytes().all(|b| b.is_ascii_hexdigit()))
    {
        return None;
    }
    let data1 = u32::from_str_radix(parts[0], 16).ok()?;
    let data2 = u16::from_str_radix(parts[1], 16).ok()?;
    let data3 = u16::from_str_radix(parts[2], 16).ok()?;
    let tail = format!("{}{}", parts[3], parts[4]);
    let mut data4 = [0u8; 8];
    for (i, byte) in data4.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&tail[2 * i..2 * i + 2], 16).ok()?;
    }
    Some(sys::Guid {
        data1,
        data2,
        data3,
        data4,
    })
}

impl fmt::Display for sys::Guid {
    /// Writes the registry form, `{XXXXXXXX-XXXX-XXXX-XXXX-XXXXXXXXXXXX}`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let d = &self.data4;
        write!(
            f,
            "{{{:08X}-{:04X}-{:04X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}}}",
            self.data1, self.data2, self.data3, d[0], d[1], d[2], d[3], d[4], d[5], d[6], d[7]
        )
    }
}

/// Decodes a fixed-size C string from a driver (a name or an error
/// message): everything up to the first NUL, or the whole buffer if there
/// is none, with surrounding whitespace trimmed. Drivers write these in the
/// system's ANSI code page, not UTF-8, so text that is not valid UTF-8 is
/// read as Latin-1, which keeps accented Western letters readable instead
/// of turning them into replacement characters.
pub fn c_string(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    let raw = &bytes[..end];
    let text = match std::str::from_utf8(raw) {
        Ok(s) => s.to_owned(),
        Err(_) => raw.iter().map(|&b| char::from(b)).collect(),
    };
    text.trim().to_owned()
}

/// What the host does with one `asioMessage` from the driver, apart from
/// the value it returns.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) enum MessageEffect {
    /// Nothing to record.
    None,
    /// Record that the driver wants a reset.
    Reset,
    /// Count a dropout.
    Dropout,
    /// Record that the latencies changed.
    LatenciesChanged,
}

/// The host's answer to an `asioMessage(selector, value)` call, and what to
/// record. Selectors the host handles are answered 1, unknown ones 0. A
/// reset request is only recorded here: the SDK says the host must not stop
/// or unload the driver from inside the callback, so the owner does it later.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn answer_message(selector: i32, value: i32) -> (i32, MessageEffect) {
    use sys::*;
    match selector {
        K_ASIO_SELECTOR_SUPPORTED => {
            let handled = matches!(
                value,
                K_ASIO_ENGINE_VERSION
                    | K_ASIO_RESET_REQUEST
                    | K_ASIO_BUFFER_SIZE_CHANGE
                    | K_ASIO_RESYNC_REQUEST
                    | K_ASIO_LATENCIES_CHANGED
                    | K_ASIO_SUPPORTS_TIME_INFO
                    | K_ASIO_OVERLOAD
            );
            (handled as i32, MessageEffect::None)
        }
        K_ASIO_ENGINE_VERSION => (2, MessageEffect::None),
        K_ASIO_RESET_REQUEST | K_ASIO_BUFFER_SIZE_CHANGE => (1, MessageEffect::Reset),
        K_ASIO_RESYNC_REQUEST | K_ASIO_OVERLOAD => (1, MessageEffect::Dropout),
        K_ASIO_LATENCIES_CHANGED => (1, MessageEffect::LatenciesChanged),
        K_ASIO_SUPPORTS_TIME_INFO => (1, MessageEffect::None),
        _ => (0, MessageEffect::None),
    }
}

/// Whether the half-open byte ranges `(start, end)` are pairwise disjoint.
/// Used to check the buffers a driver hands back before making slices of
/// them, since overlapping `&mut` slices would be undefined behaviour.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn ranges_disjoint(ranges: &[(usize, usize)]) -> bool {
    ranges
        .iter()
        .enumerate()
        .all(|(i, a)| ranges[i + 1..].iter().all(|b| a.1 <= b.0 || b.1 <= a.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buffer_size_preferred_when_valid() {
        assert_eq!(choose_buffer_size(64, 4096, 512, -1, false), Some(512));
        assert_eq!(choose_buffer_size(64, 4096, 480, 32, false), Some(480));
        assert_eq!(choose_buffer_size(256, 256, 256, 0, false), Some(256));
    }

    #[test]
    fn buffer_size_powers_of_two() {
        // 600 is not a power of two: 512 is closer than 1024.
        assert_eq!(choose_buffer_size(64, 4096, 600, -1, false), Some(512));
        // 800 is closer to 1024.
        assert_eq!(choose_buffer_size(64, 4096, 800, -1, false), Some(1024));
        // 768 is exactly between: the larger wins.
        assert_eq!(choose_buffer_size(64, 4096, 768, -1, false), Some(1024));
        // Out of range on either side clamps to the ends.
        assert_eq!(choose_buffer_size(64, 4096, 16, -1, false), Some(64));
        assert_eq!(choose_buffer_size(64, 4096, 100_000, -1, false), Some(4096));
        // A min that is not a power of two starts at the next one.
        assert_eq!(choose_buffer_size(48, 4096, 50, -1, false), Some(64));
    }

    #[test]
    fn buffer_size_fixed() {
        assert_eq!(choose_buffer_size(441, 441, 441, 0, false), Some(441));
        // Preferred outside the fixed size: the fixed size.
        assert_eq!(choose_buffer_size(441, 441, 512, 0, false), Some(441));
        // A fixed size that is not a multiple of 16 cannot carry DSD.
        assert_eq!(choose_buffer_size(441, 441, 441, 0, true), None);
        assert_eq!(choose_buffer_size(2048, 2048, 2048, 0, true), Some(2048));
    }

    #[test]
    fn buffer_size_linear_granularity() {
        // Steps of 48 from 48: 432 and 480 around 450, 432 is closer.
        assert_eq!(choose_buffer_size(48, 4800, 450, 48, false), Some(432));
        // Steps of 1: anything in range.
        assert_eq!(choose_buffer_size(32, 8192, 1000, 1, false), Some(1000));
        // Preferred off the grid and beyond max.
        assert_eq!(choose_buffer_size(100, 1000, 5000, 300, false), Some(1000));
        assert_eq!(choose_buffer_size(100, 1050, 5000, 300, false), Some(1000));
    }

    #[test]
    fn buffer_size_dsd_multiple_of_16() {
        // 1000 is not a multiple of 16; 992 and 1008 are equally close, so
        // the larger one wins.
        assert_eq!(choose_buffer_size(32, 8192, 1000, 1, true), Some(1008));
        assert_eq!(choose_buffer_size(32, 8192, 1000, 8, true), Some(1008));
        assert_eq!(choose_buffer_size(32, 8192, 997, 1, true), Some(992));
        assert_eq!(choose_buffer_size(32, 8192, 1002, 8, true), Some(1008));
        // Steps of 24 from 24: multiples of 16 are every other step (48, 96, ...).
        assert_eq!(choose_buffer_size(24, 2400, 100, 24, true), Some(96));
        // Powers of two of 16 or more always qualify.
        assert_eq!(choose_buffer_size(8, 4096, 8, -1, true), Some(16));
        // Steps that can never land on a multiple of 16.
        assert_eq!(choose_buffer_size(1, 99, 50, 2, true), None);
        // The preferred size itself is fine.
        assert_eq!(choose_buffer_size(64, 8192, 4096, -1, true), Some(4096));
    }

    #[test]
    fn buffer_size_bad_driver_ranges() {
        assert_eq!(choose_buffer_size(0, 0, 512, 0, false), Some(512));
        assert_eq!(choose_buffer_size(1024, 64, 512, -1, false), Some(512));
        assert_eq!(choose_buffer_size(0, 0, 0, 0, false), None);
        assert_eq!(choose_buffer_size(0, 0, 500, 0, true), None);
        // Preferred zero: the closest to min.
        assert_eq!(choose_buffer_size(64, 4096, 0, -1, false), Some(64));
    }

    #[test]
    fn clsid_parsing() {
        let g = parse_clsid("{232685C6-6548-49D8-846D-4141A3EF7560}").unwrap();
        assert_eq!(g.data1, 0x232685C6);
        assert_eq!(g.data2, 0x6548);
        assert_eq!(g.data3, 0x49D8);
        assert_eq!(g.data4, [0x84, 0x6D, 0x41, 0x41, 0xA3, 0xEF, 0x75, 0x60]);
        assert_eq!(g.to_string(), "{232685C6-6548-49D8-846D-4141A3EF7560}");
        // Lower case, no braces, surrounding spaces.
        assert_eq!(
            parse_clsid(" 232685c6-6548-49d8-846d-4141a3ef7560 "),
            Some(g)
        );
        assert_eq!(parse_clsid("{232685C6-6548-49D8-846D-4141A3EF756}"), None);
        assert_eq!(parse_clsid("{232685C6-6548-49D8-846D4141A3EF7560}"), None);
        assert_eq!(parse_clsid("{232685C6-6548-49D8-846D-4141A3EF756G}"), None);
        assert_eq!(parse_clsid("{+32685C6-6548-49D8-846D-4141A3EF7560}"), None);
        assert_eq!(parse_clsid(""), None);
    }

    #[test]
    fn c_strings() {
        assert_eq!(c_string(b"ASIO4ALL v2\0garbage"), "ASIO4ALL v2");
        assert_eq!(c_string(b"no terminator"), "no terminator");
        assert_eq!(c_string(b"\0"), "");
        assert_eq!(c_string(b"  Out 1  \0"), "Out 1");
        assert_eq!(c_string("Gerät\0".as_bytes()), "Gerät");
        // Windows-1252 / Latin-1 bytes that are not UTF-8.
        assert_eq!(c_string(b"Ger\xe4t\0"), "Gerät");
    }

    #[test]
    fn sample_types() {
        for raw in [
            0, 1, 2, 3, 4, 8, 9, 10, 11, 16, 17, 18, 19, 20, 24, 25, 26, 27, 32, 33, 40,
        ] {
            let t = SampleType::from_raw(raw);
            assert!(!matches!(t, SampleType::Unknown(_)), "{raw}");
            assert_eq!(t.raw(), raw);
        }
        assert_eq!(SampleType::from_raw(5), SampleType::Unknown(5));
        assert_eq!(SampleType::from_raw(18), SampleType::Int32Lsb);
        assert_eq!(SampleType::from_raw(33), SampleType::DsdInt8Msb1);
        assert_eq!(SampleType::Int32Lsb.bytes_per_sample(), Some(4));
        assert_eq!(SampleType::Int24Lsb.bytes_per_sample(), Some(3));
        assert_eq!(SampleType::Int16Msb.bytes_per_sample(), Some(2));
        assert_eq!(SampleType::Float64Lsb.bytes_per_sample(), Some(8));
        assert_eq!(SampleType::Int32Lsb24.bytes_per_sample(), Some(4));
        assert_eq!(SampleType::DsdInt8Msb1.bytes_per_sample(), None);
        assert_eq!(SampleType::Int32Lsb.bytes_per_buffer(512), Some(2048));
        assert_eq!(SampleType::Int24Lsb.bytes_per_buffer(512), Some(1536));
        assert_eq!(SampleType::DsdInt8Msb1.bytes_per_buffer(4096), Some(512));
        assert_eq!(SampleType::DsdInt8Lsb1.bytes_per_buffer(4100), None);
        assert_eq!(SampleType::DsdInt8Ner8.bytes_per_buffer(64), Some(64));
        assert_eq!(SampleType::Unknown(99).bytes_per_buffer(64), None);
        assert_eq!(SampleType::Int32Lsb.bytes_per_buffer(0), None);
        assert!(SampleType::DsdInt8Lsb1.is_dsd() && !SampleType::Float32Lsb.is_dsd());
        assert!(SampleType::Int32Msb24.is_big_endian() && !SampleType::Int32Lsb.is_big_endian());
        assert_eq!(SampleType::DsdInt8Msb1.silence_byte(), 0x69);
        assert_eq!(
            SampleType::DsdInt8Lsb1.silence_byte(),
            0x69u8.reverse_bits()
        );
        assert_eq!(SampleType::Int32Lsb.silence_byte(), 0);
    }

    #[test]
    fn dsd_rate_list() {
        assert_eq!(DSD_RATES.len(), 8);
        for k in 0..4 {
            assert_eq!(DSD_RATES[2 * k], 2_822_400 << k);
            assert_eq!(DSD_RATES[2 * k + 1], 3_072_000 << k);
            assert_eq!(DSD_RATES[2 * k], 44_100 * (64 << k));
            assert_eq!(DSD_RATES[2 * k + 1], 48_000 * (64 << k));
        }
        assert!(DSD_RATES.windows(2).all(|w| w[0] < w[1]));
        assert!(is_dsd_rate(2_822_400.0) && is_dsd_rate(24_576_000.2));
        assert!(!is_dsd_rate(192_000.0));
    }

    #[test]
    fn asio_errors() {
        assert_eq!(AsioError::from_raw(0), AsioError::Ok);
        assert_eq!(AsioError::from_raw(0x3f4847a0), AsioError::Success);
        assert_eq!(AsioError::from_raw(-1000), AsioError::NotPresent);
        assert_eq!(AsioError::from_raw(-994), AsioError::NoMemory);
        assert_eq!(AsioError::from_raw(-995), AsioError::NoClock);
        assert_eq!(AsioError::from_raw(7), AsioError::Other(7));
        assert!(future_succeeded(sys::ASE_SUCCESS) && future_succeeded(sys::ASE_OK));
        assert!(
            !future_succeeded(sys::ASE_INVALID_PARAMETER)
                && !future_succeeded(sys::ASE_NOT_PRESENT)
        );
    }

    #[test]
    fn host_messages() {
        use sys::*;
        assert_eq!(
            answer_message(K_ASIO_SELECTOR_SUPPORTED, K_ASIO_RESET_REQUEST).0,
            1
        );
        assert_eq!(
            answer_message(K_ASIO_SELECTOR_SUPPORTED, K_ASIO_SUPPORTS_TIME_INFO).0,
            1
        );
        assert_eq!(
            answer_message(K_ASIO_SELECTOR_SUPPORTED, K_ASIO_SUPPORTS_TIME_CODE).0,
            0
        );
        assert_eq!(answer_message(K_ASIO_SELECTOR_SUPPORTED, 9).0, 0);
        assert_eq!(
            answer_message(K_ASIO_ENGINE_VERSION, 0),
            (2, MessageEffect::None)
        );
        assert_eq!(
            answer_message(K_ASIO_RESET_REQUEST, 0),
            (1, MessageEffect::Reset)
        );
        assert_eq!(
            answer_message(K_ASIO_BUFFER_SIZE_CHANGE, 1024),
            (1, MessageEffect::Reset)
        );
        assert_eq!(
            answer_message(K_ASIO_RESYNC_REQUEST, 0),
            (1, MessageEffect::Dropout)
        );
        assert_eq!(
            answer_message(K_ASIO_OVERLOAD, 0),
            (1, MessageEffect::Dropout)
        );
        assert_eq!(
            answer_message(K_ASIO_LATENCIES_CHANGED, 0),
            (1, MessageEffect::LatenciesChanged)
        );
        assert_eq!(
            answer_message(K_ASIO_SUPPORTS_TIME_INFO, 0),
            (1, MessageEffect::None)
        );
        assert_eq!(
            answer_message(K_ASIO_SUPPORTS_TIME_CODE, 0),
            (0, MessageEffect::None)
        );
        assert_eq!(answer_message(99, 0), (0, MessageEffect::None));
    }

    #[test]
    fn buffer_ranges() {
        assert!(ranges_disjoint(&[(0, 10), (10, 20), (30, 40)]));
        assert!(ranges_disjoint(&[(30, 40), (0, 10)]));
        assert!(!ranges_disjoint(&[(0, 10), (5, 15)]));
        assert!(!ranges_disjoint(&[(0, 10), (20, 30), (25, 26)]));
        assert!(ranges_disjoint(&[]));
    }

    #[test]
    fn rate_comparison() {
        assert!(rates_equal(44_100.0, 44_100.3));
        assert!(!rates_equal(44_100.0, 48_000.0));
    }
}
