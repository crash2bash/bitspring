// SPDX-License-Identifier: GPL-3.0-only
//
// Raw ASIO interface, translated from the Steinberg ASIO SDK 2.3.4 headers
// (common/asio.h and common/iasiodrv.h), used under the SDK's GPLv3 licence
// option. ASIO is a registered trademark of Steinberg Media Technologies GmbH.

//! The raw ASIO driver interface: the `IASIO` COM object, its structures and
//! its constants, exactly as a driver sees them in memory.
//!
//! Nothing here is safe to use on its own. The rest of the crate wraps it;
//! it is public so the layout can be checked and so a reader can follow the
//! wrapper against the SDK.
//!
//! The SDK compiles all of these structures with `#pragma pack(4)`, so they
//! are `#[repr(C, packed(4))]` here: no field is aligned to more than 4 bytes.
//! The C type `long` is 32 bits on Windows (on 64-bit Windows too), so it is
//! `i32` throughout. Fields holding `f64` must be read by value, never by
//! reference, because they may sit on a 4-byte boundary.

#![allow(non_snake_case)]

use core::ffi::c_void;

/// The SDK's `ASIOError`: zero or a positive success value, or a negative
/// error code. See [`crate::AsioError`] for the decoded form.
pub type AsioErrorCode = i32;

/// The call succeeded.
pub const ASE_OK: AsioErrorCode = 0;
/// The unique success value of `future()` calls (plain `ASE_OK` is not
/// enough there, according to the SDK, although some drivers return it).
pub const ASE_SUCCESS: AsioErrorCode = 0x3f48_47a0;
/// The hardware input or output is not present or not available.
pub const ASE_NOT_PRESENT: AsioErrorCode = -1000;
/// The hardware is malfunctioning.
pub const ASE_HW_MALFUNCTION: AsioErrorCode = -999;
/// An input parameter is invalid.
pub const ASE_INVALID_PARAMETER: AsioErrorCode = -998;
/// The hardware is in a bad mode, or is being used in a bad mode.
pub const ASE_INVALID_MODE: AsioErrorCode = -997;
/// The hardware is not running when the sample position is asked for.
pub const ASE_SP_NOT_ADVANCING: AsioErrorCode = -996;
/// The sample clock or rate cannot be determined or is not present.
pub const ASE_NO_CLOCK: AsioErrorCode = -995;
/// Not enough memory to complete the request.
pub const ASE_NO_MEMORY: AsioErrorCode = -994;

/// `asioMessage` selector: does the host support the selector in `value`?
pub const K_ASIO_SELECTOR_SUPPORTED: i32 = 1;
/// `asioMessage` selector: which ASIO version does the host implement?
pub const K_ASIO_ENGINE_VERSION: i32 = 2;
/// `asioMessage` selector: the driver wants to be closed and opened again.
pub const K_ASIO_RESET_REQUEST: i32 = 3;
/// `asioMessage` selector: the driver's buffer size changed.
pub const K_ASIO_BUFFER_SIZE_CHANGE: i32 = 4;
/// `asioMessage` selector: the driver lost sync and asks for a restart.
pub const K_ASIO_RESYNC_REQUEST: i32 = 5;
/// `asioMessage` selector: the driver's latencies changed.
pub const K_ASIO_LATENCIES_CHANGED: i32 = 6;
/// `asioMessage` selector: does the host handle `bufferSwitchTimeInfo`?
pub const K_ASIO_SUPPORTS_TIME_INFO: i32 = 7;
/// `asioMessage` selector: does the host read time code?
pub const K_ASIO_SUPPORTS_TIME_CODE: i32 = 8;
/// `asioMessage` selector: the driver detected an overload (a dropout).
pub const K_ASIO_OVERLOAD: i32 = 15;

/// `future` selector: switch the driver between PCM and DSD.
pub const K_ASIO_SET_IO_FORMAT: i32 = 0x2311_1961;
/// `future` selector: read the driver's current PCM or DSD mode.
pub const K_ASIO_GET_IO_FORMAT: i32 = 0x2311_1983;
/// `future` selector: ask whether the driver can switch to a mode.
pub const K_ASIO_CAN_DO_IO_FORMAT: i32 = 0x2311_2004;

/// `ASIOIoFormat` type: the driver rejected the requested format.
pub const K_ASIO_FORMAT_INVALID: i32 = -1;
/// `ASIOIoFormat` type: PCM.
pub const K_ASIO_PCM_FORMAT: i32 = 0;
/// `ASIOIoFormat` type: DSD.
pub const K_ASIO_DSD_FORMAT: i32 = 1;

/// A 64-bit sample count split into two 32-bit halves (`ASIOSamples`).
#[repr(C, packed(4))]
#[derive(Clone, Copy, Debug, Default)]
pub struct AsioSamples {
    /// The upper 32 bits.
    pub hi: u32,
    /// The lower 32 bits.
    pub lo: u32,
}

/// A 64-bit time stamp in nanoseconds, split into two halves
/// (`ASIOTimeStamp`).
pub type AsioTimeStamp = AsioSamples;

/// What `init` is told about the host and what it reports back
/// (`ASIODriverInfo`). Only used by the C-level `ASIOInit` helper of the
/// SDK; kept here for completeness and for the layout check.
#[repr(C, packed(4))]
#[derive(Clone, Copy)]
pub struct AsioDriverInfo {
    /// The ASIO version (2).
    pub asioVersion: i32,
    /// The driver's own version number.
    pub driverVersion: i32,
    /// The driver name, a NUL-terminated C string.
    pub name: [u8; 32],
    /// The last error message, a NUL-terminated C string.
    pub errorMessage: [u8; 124],
    /// On Windows, the application's main window handle.
    pub sysRef: *mut c_void,
}

/// One input or output channel (`ASIOChannelInfo`).
#[repr(C, packed(4))]
#[derive(Clone, Copy)]
pub struct AsioChannelInfo {
    /// In: the channel index.
    pub channel: i32,
    /// In: 1 for an input channel, 0 for an output.
    pub isInput: i32,
    /// Out: 1 if the channel has buffers from `createBuffers`.
    pub isActive: i32,
    /// Out: the channel group (analog, S/PDIF and so on).
    pub channelGroup: i32,
    /// Out: the sample type, see [`crate::SampleType`].
    pub type_: i32,
    /// Out: the channel name, a NUL-terminated C string.
    pub name: [u8; 32],
}

/// One channel's double buffer (`ASIOBufferInfo`).
#[repr(C, packed(4))]
#[derive(Clone, Copy)]
pub struct AsioBufferInfo {
    /// In: 1 for an input channel, 0 for an output.
    pub isInput: i32,
    /// In: the channel index.
    pub channelNum: i32,
    /// Out: the two halves of the double buffer.
    pub buffers: [*mut c_void; 2],
}

/// Timing of one buffer switch (`AsioTimeInfo`).
#[repr(C, packed(4))]
#[derive(Clone, Copy)]
pub struct AsioTimeInfo {
    /// The absolute speed (1.0 = nominal).
    pub speed: f64,
    /// The system time of `samplePosition`, in nanoseconds.
    pub systemTime: AsioTimeStamp,
    /// The sample position of this buffer.
    pub samplePosition: AsioSamples,
    /// The current sample rate.
    pub sampleRate: f64,
    /// Which of the fields above are valid.
    pub flags: u32,
    /// Reserved.
    pub reserved: [u8; 12],
}

/// Time code of one buffer switch (`ASIOTimeCode`).
#[repr(C, packed(4))]
#[derive(Clone, Copy)]
pub struct AsioTimeCode {
    /// The speed relative to nominal.
    pub speed: f64,
    /// The time code position in samples.
    pub timeCodeSamples: AsioSamples,
    /// Which of the fields above are valid.
    pub flags: u32,
    /// Reserved.
    pub future: [u8; 64],
}

/// The parameter block of `bufferSwitchTimeInfo` (`ASIOTime`).
#[repr(C, packed(4))]
#[derive(Clone, Copy)]
pub struct AsioTime {
    /// Reserved, must be zero.
    pub reserved: [i32; 4],
    /// Timing information.
    pub timeInfo: AsioTimeInfo,
    /// Time code, valid only when its flags say so.
    pub timeCode: AsioTimeCode,
}

/// A clock source (`ASIOClockSource`).
#[repr(C, packed(4))]
#[derive(Clone, Copy)]
pub struct AsioClockSource {
    /// The index to pass to `setClockSource`.
    pub index: i32,
    /// The channel this clock comes in on, for instance S/PDIF.
    pub associatedChannel: i32,
    /// The channel group this clock belongs to.
    pub associatedGroup: i32,
    /// 1 if this is the current clock source.
    pub isCurrentSource: i32,
    /// The clock source name, a NUL-terminated C string.
    pub name: [u8; 32],
}

/// The parameter of the IoFormat `future` calls (`ASIOIoFormat`), a
/// 512-byte block whose first field is the format.
#[repr(C, packed(4))]
#[derive(Clone, Copy)]
pub struct AsioIoFormat {
    /// [`K_ASIO_PCM_FORMAT`], [`K_ASIO_DSD_FORMAT`] or, when the driver
    /// rejects the request, [`K_ASIO_FORMAT_INVALID`].
    pub FormatType: i32,
    /// Reserved, zero.
    pub future: [u8; 508],
}

impl AsioIoFormat {
    /// A block asking for the given format, with the reserved part zeroed.
    pub fn new(format: i32) -> Self {
        AsioIoFormat {
            FormatType: format,
            future: [0; 508],
        }
    }
}

/// The host functions a driver calls (`ASIOCallbacks`). These are plain C
/// function pointers (the C calling convention), not C++ methods.
#[repr(C, packed(4))]
#[derive(Clone, Copy)]
pub struct AsioCallbacks {
    /// Fill output half `index` (0 or 1). The second argument says whether
    /// the host may process right away (ignored by this crate).
    pub bufferSwitch: unsafe extern "C" fn(index: i32, directProcess: i32),
    /// The driver's sample rate changed, to the given rate (0 if unknown).
    pub sampleRateDidChange: unsafe extern "C" fn(rate: f64),
    /// A message from the driver, see the `K_ASIO_*` selectors.
    pub asioMessage:
        unsafe extern "C" fn(selector: i32, value: i32, message: *mut c_void, opt: *mut f64) -> i32,
    /// Like `bufferSwitch`, with timing. Returns its first argument.
    pub bufferSwitchTimeInfo: unsafe extern "C" fn(
        params: *mut AsioTime,
        index: i32,
        directProcess: i32,
    ) -> *mut AsioTime,
}

/// The GUID layout COM uses (`GUID` / `CLSID` / `IID`), 16 bytes.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub struct Guid {
    /// The first 8 hex digits.
    pub data1: u32,
    /// The next 4 hex digits.
    pub data2: u16,
    /// The next 4 hex digits.
    pub data3: u16,
    /// The last 16 hex digits, as bytes in written order.
    pub data4: [u8; 8],
}

// IUnknown's methods are `__stdcall` (COM's STDMETHODCALLTYPE), which Rust
// calls "system". IASIO's own methods are plain C++ virtual functions: on
// 32-bit x86 MSVC that is `__thiscall` (the object pointer in ECX), on x86_64
// and aarch64 there is a single calling convention, which "system" names.
// The macro picks the right one for each IASIO method type.
#[cfg(target_arch = "x86")]
macro_rules! asio_method {
    (($($arg:ty),*) -> $ret:ty) => { unsafe extern "thiscall" fn(*mut IAsio $(, $arg)*) -> $ret };
    (($($arg:ty),*)) => { unsafe extern "thiscall" fn(*mut IAsio $(, $arg)*) };
}
#[cfg(not(target_arch = "x86"))]
macro_rules! asio_method {
    (($($arg:ty),*) -> $ret:ty) => { unsafe extern "system" fn(*mut IAsio $(, $arg)*) -> $ret };
    (($($arg:ty),*)) => { unsafe extern "system" fn(*mut IAsio $(, $arg)*) };
}

/// An ASIO driver object: a pointer to its table of virtual methods.
#[repr(C)]
pub struct IAsio {
    /// The driver's method table.
    pub vtbl: *const IAsioVtbl,
}

/// The method table of `IASIO`, in declaration order (IUnknown first).
#[repr(C)]
pub struct IAsioVtbl {
    /// `IUnknown::QueryInterface`.
    pub QueryInterface:
        unsafe extern "system" fn(*mut IAsio, riid: *const Guid, out: *mut *mut c_void) -> i32,
    /// `IUnknown::AddRef`.
    pub AddRef: unsafe extern "system" fn(*mut IAsio) -> u32,
    /// `IUnknown::Release`.
    pub Release: unsafe extern "system" fn(*mut IAsio) -> u32,
    /// Initialise the driver; takes the application's window handle and
    /// returns 1 (ASIOTrue) on success.
    pub init: asio_method!((*mut c_void) -> i32),
    /// Write the driver name (at most 32 bytes with the NUL).
    pub getDriverName: asio_method!((*mut u8)),
    /// The driver's version number.
    pub getDriverVersion: asio_method!(() -> i32),
    /// Write the last error message (at most 124 bytes with the NUL).
    pub getErrorMessage: asio_method!((*mut u8)),
    /// Start streaming.
    pub start: asio_method!(() -> AsioErrorCode),
    /// Stop streaming; no buffer switch follows once it returns.
    pub stop: asio_method!(() -> AsioErrorCode),
    /// Input and output channel counts.
    pub getChannels: asio_method!((*mut i32, *mut i32) -> AsioErrorCode),
    /// Input and output latencies, in samples.
    pub getLatencies: asio_method!((*mut i32, *mut i32) -> AsioErrorCode),
    /// Minimum, maximum, preferred buffer size and granularity, in samples.
    pub getBufferSize: asio_method!((*mut i32, *mut i32, *mut i32, *mut i32) -> AsioErrorCode),
    /// `ASE_OK` if the rate is supported in the current mode.
    pub canSampleRate: asio_method!((f64) -> AsioErrorCode),
    /// The current sample rate.
    pub getSampleRate: asio_method!((*mut f64) -> AsioErrorCode),
    /// Set the sample rate (the DSD bit rate in DSD mode).
    pub setSampleRate: asio_method!((f64) -> AsioErrorCode),
    /// List the clock sources.
    pub getClockSources: asio_method!((*mut AsioClockSource, *mut i32) -> AsioErrorCode),
    /// Choose a clock source.
    pub setClockSource: asio_method!((i32) -> AsioErrorCode),
    /// The current sample position and its time stamp.
    pub getSamplePosition: asio_method!((*mut AsioSamples, *mut AsioTimeStamp) -> AsioErrorCode),
    /// Describe one channel.
    pub getChannelInfo: asio_method!((*mut AsioChannelInfo) -> AsioErrorCode),
    /// Allocate double buffers for the given channels and register the
    /// callbacks.
    pub createBuffers: asio_method!((*mut AsioBufferInfo, i32, i32, *mut AsioCallbacks) -> AsioErrorCode),
    /// Free the buffers from `createBuffers`.
    pub disposeBuffers: asio_method!(() -> AsioErrorCode),
    /// Open the driver's own settings window.
    pub controlPanel: asio_method!(() -> AsioErrorCode),
    /// Extension calls, see the `K_ASIO_*_IO_FORMAT` selectors.
    pub future: asio_method!((i32, *mut c_void) -> AsioErrorCode),
    /// Tell the driver the output half is filled (lowers latency where
    /// supported).
    pub outputReady: asio_method!(() -> AsioErrorCode),
}

// Layout checks for 64-bit targets (x86_64 and aarch64 Windows; also any
// other 64-bit target, so the tests on a Mac check the same thing). With
// pack(4) nothing is padded to more than 4 bytes:
//
// - AsioSamples / AsioTimeStamp: 4 + 4 = 8.
// - AsioDriverInfo: 4 + 4 + 32 + 124 = 164, then the 8-byte pointer at
//   offset 164 (allowed by pack(4)) = 172.
// - AsioChannelInfo: 5 * 4 + 32 = 52.
// - AsioBufferInfo: 4 + 4 + 2 * 8 = 24.
// - AsioTimeInfo: 8 (speed) + 8 + 8 + 8 (sampleRate) + 4 (flags) + 12 = 48.
// - AsioTimeCode: 8 + 8 + 4 + 64 = 84.
// - AsioTime: 4 * 4 + 48 + 84 = 148.
// - AsioClockSource: 4 * 4 + 32 = 48.
// - AsioIoFormat: 4 + 508 = 512, as the SDK spells out.
// - AsioCallbacks: 4 function pointers = 32.
// - Guid: 4 + 2 + 2 + 8 = 16.
#[cfg(target_pointer_width = "64")]
const _: () = {
    use core::mem::{offset_of, size_of};
    assert!(size_of::<AsioSamples>() == 8);
    assert!(size_of::<AsioDriverInfo>() == 172);
    assert!(offset_of!(AsioDriverInfo, sysRef) == 164);
    assert!(size_of::<AsioChannelInfo>() == 52);
    assert!(size_of::<AsioBufferInfo>() == 24);
    assert!(offset_of!(AsioBufferInfo, buffers) == 8);
    assert!(size_of::<AsioTimeInfo>() == 48);
    assert!(offset_of!(AsioTimeInfo, sampleRate) == 24);
    assert!(offset_of!(AsioTimeInfo, flags) == 32);
    assert!(size_of::<AsioTimeCode>() == 84);
    assert!(size_of::<AsioTime>() == 148);
    assert!(offset_of!(AsioTime, timeCode) == 64);
    assert!(size_of::<AsioClockSource>() == 48);
    assert!(size_of::<AsioIoFormat>() == 512);
    assert!(size_of::<AsioCallbacks>() == 32);
    assert!(size_of::<Guid>() == 16);
    // 3 IUnknown methods + 21 IASIO methods.
    assert!(size_of::<IAsioVtbl>() == 24 * 8);
};

// The same checks for 32-bit x86, where pointers are 4 bytes.
#[cfg(target_pointer_width = "32")]
const _: () = {
    use core::mem::size_of;
    assert!(size_of::<AsioDriverInfo>() == 168);
    assert!(size_of::<AsioChannelInfo>() == 52);
    assert!(size_of::<AsioBufferInfo>() == 16);
    assert!(size_of::<AsioTime>() == 148);
    assert!(size_of::<AsioIoFormat>() == 512);
    assert!(size_of::<AsioCallbacks>() == 16);
    assert!(size_of::<IAsioVtbl>() == 24 * 4);
};
