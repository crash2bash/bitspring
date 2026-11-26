//! Every output the app can play on — the system's audio devices and network players — and
//! one player type for both, so the app drives them the same way.

use std::sync::Arc;

use anyhow::Result;

use crate::decode::Track;
use crate::dsp::eq::EqPreset;
use crate::net::player::{self as net_player, NetPlayer};
use crate::net::{self, Target};
use crate::output::{self, DeviceInfo, PlayOptions, SignalPath, Transport};
use crate::rate::RateRange;
use crate::UserError;

/// Network players are found on desktops (and Android); iOS needs Apple's multicast
/// entitlement for the search, so it stays with local outputs for now.
pub const NETWORK: bool = cfg!(not(target_os = "ios"));

#[derive(Debug, Clone)]
pub enum Output {
    Device(DeviceInfo),
    Network(Target),
}

impl Output {
    pub fn uid(&self) -> &str {
        match self {
            Self::Device(d) => &d.uid,
            Self::Network(t) => t.uid(),
        }
    }

    pub fn name(&self) -> &str {
        match self {
            Self::Device(d) => &d.name,
            Self::Network(t) => t.name(),
        }
    }

    pub fn transport(&self) -> Transport {
        match self {
            Self::Device(d) => d.transport,
            Self::Network(_) => Transport::Network,
        }
    }

    pub fn rates(&self) -> Vec<RateRange> {
        match self {
            Self::Device(d) => d.rates.clone(),
            Self::Network(t) => net_player::rates(t),
        }
    }

    pub fn output_channels(&self) -> u32 {
        match self {
            Self::Device(d) => d.output_channels,
            Self::Network(_) => 2,
        }
    }

    pub fn nominal_rate(&self) -> f64 {
        match self {
            Self::Device(d) => d.nominal_rate,
            Self::Network(_) => 0.0,
        }
    }

    pub fn is_default(&self) -> bool {
        matches!(self, Self::Device(d) if d.is_default)
    }

    /// The output can carry DSD at `dsd_rate` as DoP (network players can't).
    pub fn supports_dop(&self, dsd_rate: u32) -> bool {
        match self {
            Self::Device(d) => d.device.supports_dop(dsd_rate),
            Self::Network(_) => false,
        }
    }

    /// The output takes DSD at `dsd_rate` natively, as raw DSD without DoP's markers (a Linux
    /// card the kernel gives a DSD format, a Windows ASIO driver with a DSD mode; network
    /// players never). Where both
    /// work, the engine sends it natively.
    pub fn supports_native_dsd(&self, dsd_rate: u32) -> bool {
        match self {
            Self::Device(d) => d.device.supports_native_dsd(dsd_rate),
            Self::Network(_) => false,
        }
    }

    pub fn is_network(&self) -> bool {
        matches!(self, Self::Network(_))
    }

    /// Every playback has the device to itself, whether exclusive access is asked for or not
    /// (Windows: an ASIO driver, which is never shared).
    pub fn always_exclusive(&self) -> bool {
        match self {
            #[cfg(windows)]
            Self::Device(d) => d.device.always_exclusive(),
            _ => false,
        }
    }

    /// The output's driver has a settings window of its own (Windows: an ASIO driver's
    /// control panel, for its buffer size and the like).
    pub fn has_control_panel(&self) -> bool {
        match self {
            #[cfg(windows)]
            Self::Device(d) => d.device.has_control_panel(),
            _ => false,
        }
    }

    /// Open that window. Blocks while the driver shows it modally (some do), so call it off
    /// the UI thread. Errors where there is none, or (ASIO) while another ASIO driver plays.
    pub fn control_panel(&self) -> Result<()> {
        match self {
            #[cfg(windows)]
            Self::Device(d) => d.device.control_panel(),
            _ => anyhow::bail!("this output has no control panel"),
        }
    }
}

/// Before the app quits: let go of what an output keeps open between playbacks (Windows: the
/// ASIO driver, which is closed properly and left in PCM mode for the next app). Nothing to
/// do elsewhere.
pub fn release_outputs() {
    #[cfg(windows)]
    output::asio_release();
}

/// The system's devices, then the network players found so far.
pub fn outputs() -> Result<Vec<Output>> {
    let mut v: Vec<Output> = output::output_devices()?.into_iter().map(Output::Device).collect();
    if NETWORK {
        v.extend(net::targets().into_iter().map(Output::Network));
    }
    Ok(v)
}

/// One output by uid. A network player not seen yet gets a few seconds to answer.
pub fn find(uid: &str) -> Result<Option<Output>> {
    if net::is_network_uid(uid) {
        return Ok(if NETWORK { net::find(uid).map(Output::Network) } else { None });
    }
    Ok(output::output_devices()?.into_iter().find(|d| d.uid == uid).map(Output::Device))
}

/// Start looking for network players in the background. (Not at launch: the first search
/// makes macOS ask for local network access, which should come when the user looks for
/// network players.)
pub fn start_discovery() {
    if NETWORK {
        net::start_discovery();
    }
}

/// Look for network players again now.
pub fn refresh() {
    if NETWORK {
        net::refresh();
    }
}

enum Kind {
    Device(output::Player),
    Net(NetPlayer),
}

/// A track playing on an output.
pub struct Player {
    kind: Kind,
    /// What happens to the signal on its way (updated by the app as tracks follow on).
    pub path: SignalPath,
}

macro_rules! each {
    ($self:ident, $p:ident => $e:expr) => {
        match &$self.kind {
            Kind::Device($p) => $e,
            Kind::Net($p) => $e,
        }
    };
}

impl Player {
    pub fn start(out: &Output, track: Arc<Track>, opts: &PlayOptions) -> Result<Self> {
        Ok(match out {
            Output::Device(d) => {
                #[allow(clippy::clone_on_copy)] // Copy on macOS only
                let p = output::Player::start(d.device.clone(), track, opts)?;
                let path = p.path.clone();
                Self { kind: Kind::Device(p), path }
            }
            Output::Network(t) => {
                let p = NetPlayer::start(t.clone(), track, opts)?;
                let path = p.path.clone();
                Self { kind: Kind::Net(p), path }
            }
        })
    }

    pub fn is_network(&self) -> bool {
        matches!(self.kind, Kind::Net(_))
    }

    pub fn heard(&self) -> Arc<Track> {
        each!(self, p => p.heard())
    }
    pub fn upcoming(&self) -> Option<Arc<Track>> {
        each!(self, p => p.upcoming())
    }
    pub fn switches(&self) -> u64 {
        each!(self, p => p.switches())
    }
    pub fn queue_next(&self, track: Arc<Track>) -> bool {
        each!(self, p => p.queue_next(track))
    }
    /// Line up `track` to cross into it over `secs` (see
    /// [`output::Playback::queue_next_crossfading`]). A network player joins it on gapless
    /// instead: it gets each track as a stream of its own.
    pub fn queue_next_crossfading(&self, track: Arc<Track>, secs: f64) -> bool {
        match &self.kind {
            Kind::Device(p) => p.queue_next_crossfading(track, secs),
            Kind::Net(p) => p.queue_next(track),
        }
    }
    /// A crossfade is under way (the end of the track before still sounds).
    pub fn crossfading(&self) -> bool {
        match &self.kind {
            Kind::Device(p) => p.crossfading(),
            Kind::Net(_) => false,
        }
    }
    /// Carry on with `track` in place of the one being heard, at the same position (see
    /// [`output::Playback::swap`]). Never on a network player: what it plays was sent ahead.
    pub fn swap(&self, track: Arc<Track>) -> bool {
        match &self.kind {
            Kind::Device(p) => p.swap(track),
            Kind::Net(_) => false,
        }
    }
    pub fn clear_next(&self) {
        each!(self, p => p.clear_next())
    }
    pub fn position_secs(&self) -> f64 {
        each!(self, p => p.position_secs())
    }
    pub fn duration_secs(&self) -> f64 {
        each!(self, p => p.duration_secs())
    }
    pub fn is_finished(&self) -> bool {
        each!(self, p => p.is_finished())
    }
    pub fn is_paused(&self) -> bool {
        each!(self, p => p.is_paused())
    }
    pub fn set_paused(&self, paused: bool) {
        each!(self, p => p.set_paused(paused))
    }
    pub fn seek(&self, secs: f64) -> bool {
        each!(self, p => p.seek(secs))
    }
    /// Fades at pause, seek and stop on or off, for what plays now (a network player has
    /// none: it does its own muting).
    pub fn set_fades(&self, on: bool) {
        if let Kind::Device(p) = &self.kind {
            p.set_fades(on);
        }
    }

    /// Before dropping a player that sounds: let it fade out (see [`output::Playback::fade_out`]).
    /// A network player stops by itself when told to.
    pub fn fade_out(&self) {
        if let Kind::Device(p) = &self.kind {
            p.fade_out();
        }
    }
    pub fn output_peak(&self) -> Option<f32> {
        each!(self, p => p.output_peak())
    }
    pub fn take_meter(&self) -> [f32; 2] {
        each!(self, p => p.take_meter())
    }
    pub fn underruns(&self) -> u64 {
        each!(self, p => p.underruns())
    }
    /// The processing's share of a core (see [`output::Playback::load`]).
    pub fn load(&self) -> Option<f32> {
        each!(self, p => p.load())
    }
    pub fn is_buffering(&self) -> bool {
        each!(self, p => p.is_buffering())
    }
    pub fn buffered_secs(&self) -> f64 {
        each!(self, p => p.buffered_secs())
    }
    pub fn source_error(&self) -> Option<String> {
        each!(self, p => p.source_error())
    }
    pub fn output_error(&self) -> Option<UserError> {
        each!(self, p => p.output_error())
    }
    pub fn update_eq(&self, eq: &EqPreset, bypass: bool) -> bool {
        each!(self, p => p.update_eq(eq, bypass))
    }
}
