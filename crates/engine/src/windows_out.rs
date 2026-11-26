//! Windows outputs, as one list: the WASAPI endpoints (see [`crate::wasapi_out`]) and the
//! installed ASIO drivers (see `asio_out`), each driver an entry of its own next to its
//! device's WASAPI one.

use std::ops::Deref;
use std::sync::Arc;

use anyhow::{Result, bail};

use crate::UserError;
use crate::asio_out;
use crate::decode::Track;
use crate::output::{PlayOptions, Playback, SignalPath, Transport};
use crate::rate::RateRange;
use crate::wasapi_out;

/// An output: a WASAPI endpoint, or an ASIO driver by its CLSID (`{XXXXXXXX-…}`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Device {
    Wasapi(wasapi_out::Device),
    Asio { clsid: String },
}

#[derive(Debug, Clone)]
pub struct DeviceInfo {
    pub device: Device,
    pub name: String,
    /// The WASAPI endpoint id, or `asio:{CLSID}`: stable, and the per-device settings key.
    pub uid: String,
    pub transport: Transport,
    pub output_channels: u32,
    /// Where the mixer's channels go, as a WAVE channel mask (0 for ASIO).
    pub channel_mask: u32,
    /// Rate of the Windows mixer; for ASIO, the rate the driver runs at.
    pub nominal_rate: f64,
    /// Rates the device takes untouched (exclusive mode; every ASIO rate is).
    pub rates: Vec<RateRange>,
    pub is_default: bool,
}

impl From<wasapi_out::DeviceInfo> for DeviceInfo {
    fn from(d: wasapi_out::DeviceInfo) -> Self {
        Self {
            device: Device::Wasapi(d.device),
            name: d.name,
            uid: d.uid,
            transport: d.transport,
            output_channels: d.output_channels,
            channel_mask: d.channel_mask,
            nominal_rate: d.nominal_rate,
            rates: d.rates,
            is_default: d.is_default,
        }
    }
}

impl Device {
    /// The system's default output (always a WASAPI endpoint).
    pub fn default_output() -> Result<Self> {
        Ok(Self::Wasapi(wasapi_out::Device::default_output()?))
    }

    pub fn info(&self) -> Result<DeviceInfo> {
        match self {
            Self::Wasapi(d) => Ok(d.info()?.into()),
            Self::Asio { clsid } => asio_out::info(clsid),
        }
    }

    /// Can this output carry a `dsd_rate` stream as DoP (see [`wasapi_out::Device::supports_dop`];
    /// ASIO: PCM at `dsd_rate / 16` in a ≥24-bit integer format). Whether the DAC decodes DoP
    /// can't be queried: that's the user's per-device switch.
    pub fn supports_dop(&self, dsd_rate: u32) -> bool {
        match self {
            Self::Wasapi(d) => d.supports_dop(dsd_rate),
            Self::Asio { clsid } => asio_out::supports_dop(clsid, dsd_rate),
        }
    }

    /// Does this output take a `dsd_rate` stream (2 822 400 for DSD64) as native DSD: an ASIO
    /// driver whose DSD mode runs at that rate. Never a WASAPI endpoint.
    pub fn supports_native_dsd(&self, dsd_rate: u32) -> bool {
        match self {
            Self::Wasapi(d) => d.supports_native_dsd(dsd_rate),
            Self::Asio { clsid } => asio_out::supports_native_dsd(clsid, dsd_rate),
        }
    }

    /// [`Self::supports_native_dsd`] as far as it is known, never asking a driver (an ASIO
    /// driver's DSD mode is probed only when it is about to be used): for the device list.
    pub fn native_dsd_known(&self, dsd_rate: u32) -> bool {
        match self {
            Self::Wasapi(d) => d.supports_native_dsd(dsd_rate),
            Self::Asio { clsid } => asio_out::native_dsd_known(clsid, dsd_rate),
        }
    }

    /// Every playback has the device to itself (ASIO: the driver is never shared), whatever
    /// `PlayOptions.exclusive` says.
    pub fn always_exclusive(&self) -> bool {
        matches!(self, Self::Asio { .. })
    }

    /// The output has a settings window of its driver's own (ASIO).
    pub fn has_control_panel(&self) -> bool {
        matches!(self, Self::Asio { .. })
    }

    /// Open that window (see `asio_out::control_panel`).
    pub fn control_panel(&self) -> Result<()> {
        match self {
            Self::Asio { clsid } => asio_out::control_panel(clsid),
            Self::Wasapi(_) => bail!("this output has no control panel"),
        }
    }
}

/// The WASAPI endpoints, then the ASIO drivers.
pub fn output_devices() -> Result<Vec<DeviceInfo>> {
    let asio = asio_out::listing();
    match wasapi_out::output_devices() {
        Ok(list) => Ok(list.into_iter().map(DeviceInfo::from).chain(asio).collect()),
        Err(e) if !asio.is_empty() => {
            log::warn!("WASAPI: no endpoints ({e:#}); ASIO only");
            Ok(asio)
        }
        Err(e) => Err(e),
    }
}

/// One output by uid, touching no ASIO driver but the one asked for (an `asio:` uid: as
/// probed before, else that driver alone), so a playback never opens drivers nobody chose.
pub fn find_device(uid: &str) -> Result<Option<DeviceInfo>> {
    if let Some(clsid) = uid.strip_prefix("asio:") {
        return asio_out::find(clsid);
    }
    Ok(wasapi_out::output_devices()?.into_iter().find(|d| d.uid == uid).map(DeviceInfo::from))
}

/// The user refreshed the device list: ASIO drivers that failed to open are tried again.
pub fn refresh() {
    asio_out::retry_failed();
}

/// Before the app quits: close the ASIO driver kept loaded (stopping a stream on it), so it
/// is left as it should be for the next app.
pub fn asio_release() {
    asio_out::release();
}

/// (Boxed: each carries a signal path of its own, and `Player` one more.)
enum Inner {
    Wasapi(Box<wasapi_out::Player>),
    Asio(Box<asio_out::Player>),
}

pub struct Player {
    inner: Inner,
    pub path: SignalPath,
}

impl Deref for Player {
    type Target = Playback;
    fn deref(&self) -> &Playback {
        match &self.inner {
            Inner::Wasapi(p) => p,
            Inner::Asio(p) => p,
        }
    }
}

impl Player {
    pub fn start(device: Device, track: Arc<Track>, opts: &PlayOptions) -> Result<Self> {
        match device {
            Device::Wasapi(d) => {
                // An ASIO driver kept loaded may hold the same hardware: let it go first.
                if opts.exclusive {
                    asio_out::release_idle();
                }
                let p = wasapi_out::Player::start(d, track, opts)?;
                Ok(Self { path: p.path.clone(), inner: Inner::Wasapi(Box::new(p)) })
            }
            Device::Asio { clsid } => {
                let p = asio_out::Player::start(&clsid, track, opts)?;
                Ok(Self { path: p.path.clone(), inner: Inner::Asio(Box::new(p)) })
            }
        }
    }

    pub fn device(&self) -> Device {
        match &self.inner {
            Inner::Wasapi(p) => Device::Wasapi(p.device()),
            Inner::Asio(p) => Device::Asio { clsid: p.clsid().to_string() },
        }
    }

    /// The stream stopped on its own: the device went away, or (ASIO) its driver reset.
    pub fn output_error(&self) -> Option<UserError> {
        match &self.inner {
            Inner::Wasapi(p) => p.output_error(),
            Inner::Asio(p) => p.output_error(),
        }
    }

    /// Dropouts so far: the processing's, and for ASIO the ones the driver reported.
    pub fn underruns(&self) -> u64 {
        match &self.inner {
            Inner::Wasapi(p) => p.underruns(),
            Inner::Asio(p) => p.underruns(),
        }
    }
}
