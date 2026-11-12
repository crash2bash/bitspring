//! Playing on network players: UPnP/DLNA renderers (Sonos, WiiM, Bluesound, Linn, …) and
//! Google Cast devices (Chromecast, Android TV / Google TV, Nest speakers).
//!
//! The player fetches a live stream from a small HTTP server of ours: the same samples the
//! local path would send a DAC, after the same processing, encoded as FLAC (or WAV / L16 for
//! players that don't take FLAC). One stream carries track after track of the same format,
//! so albums play without gaps on any player. Only the remote control differs by protocol
//! ([`Remote`]).

pub mod cast;
pub mod encode;
#[cfg(test)]
mod fake;
pub mod http;
pub mod mdns;
pub mod player;
pub mod upnp;

use std::net::IpAddr;

use anyhow::Result;

use encode::Codec;

/// What the player shows about the stream.
#[derive(Debug, Clone, Default)]
pub struct Meta {
    pub title: String,
    pub artist: Option<String>,
    pub album: Option<String>,
    /// Cover art, served by us.
    pub cover: Option<String>,
}

/// What a player says it is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Playing,
    Paused,
    Stopped,
    Transitioning,
    NoMedia,
    Other,
}

#[derive(Debug, Clone, Copy)]
pub struct Status {
    pub state: State,
    /// Seconds into the current stream, if the player says.
    pub position: Option<f64>,
}

/// The remote control of one network player, one protocol's half of a
/// [`player::NetPlayer`]. Calls may take a moment: only the player's control thread makes
/// them.
pub trait Remote: Send {
    /// Play `url` from now on (paused if not `play`).
    fn load(&mut self, url: &str, mime: &str, meta: &Meta, play: bool) -> Result<()>;
    fn play(&mut self) -> Result<()>;
    fn pause(&mut self) -> Result<()>;
    fn stop(&mut self) -> Result<()>;
    fn status(&mut self) -> Result<Status>;
}

/// A network player of either kind.
#[derive(Debug, Clone)]
pub enum Target {
    Upnp(upnp::Renderer),
    Cast(cast::Device),
}

impl Target {
    pub fn uid(&self) -> &str {
        match self {
            Self::Upnp(r) => &r.uid,
            Self::Cast(d) => &d.uid,
        }
    }

    pub fn name(&self) -> &str {
        match self {
            Self::Upnp(r) => &r.name,
            Self::Cast(d) => &d.name,
        }
    }

    pub fn ip(&self) -> IpAddr {
        match self {
            Self::Upnp(r) => r.ip,
            Self::Cast(d) => d.ip,
        }
    }

    /// Maker and model, as shown under the name ("WiiM Pro", "SHIELD Android TV").
    pub fn model(&self) -> String {
        match self {
            Self::Upnp(r) => {
                let maker = r.manufacturer.trim_end_matches(", Inc.").trim_end_matches(" Inc.").trim();
                if maker.is_empty() || r.model.to_lowercase().starts_with(&maker.to_lowercase()) {
                    r.model.trim().to_string()
                } else {
                    format!("{maker} {}", r.model.trim())
                }
            }
            Self::Cast(d) => d.model.clone(),
        }
    }

    /// The protocol, as shown ("UPnP", "Chromecast").
    pub fn protocol(&self) -> &'static str {
        match self {
            Self::Upnp(_) => "UPnP",
            Self::Cast(_) => "Chromecast",
        }
    }

    pub fn codec(&self) -> Codec {
        match self {
            Self::Upnp(r) => r.codec,
            Self::Cast(_) => Codec::Flac,
        }
    }

    /// The MIME type the player listed for that codec, if it did.
    pub fn mime(&self) -> Option<String> {
        match self {
            Self::Upnp(r) => r.mime.clone(),
            Self::Cast(_) => None,
        }
    }

    pub fn max_rate(&self) -> u32 {
        match self {
            Self::Upnp(r) => r.max_rate,
            Self::Cast(_) => cast::MAX_RATE,
        }
    }

    pub fn max_bits(&self) -> u16 {
        match self {
            Self::Upnp(r) => r.max_bits,
            Self::Cast(_) => 24,
        }
    }

    pub fn has_volume(&self) -> bool {
        match self {
            Self::Upnp(r) => r.has_volume,
            Self::Cast(_) => true,
        }
    }

    /// The player's own volume, 0…100.
    pub fn volume(&self) -> Result<u32> {
        match self {
            Self::Upnp(r) => r.volume(),
            Self::Cast(d) => d.volume(),
        }
    }

    pub fn set_volume(&self, volume: u32) -> Result<()> {
        match self {
            Self::Upnp(r) => r.set_volume(volume),
            Self::Cast(d) => d.set_volume(volume),
        }
    }

    pub(crate) fn remote(&self) -> Box<dyn Remote> {
        match self {
            Self::Upnp(r) => Box::new(upnp::UpnpRemote::new(r.clone())),
            Self::Cast(d) => Box::new(cast::CastRemote::new(d.clone())),
        }
    }
}

/// Start looking for network players of every kind (in the background).
pub fn start_discovery() {
    upnp::start_discovery();
    cast::start_discovery();
}

/// Look again now, unless a search ran moments ago.
pub fn refresh() {
    upnp::refresh();
    cast::refresh();
}

/// The network players found so far, by name. (A device that speaks both protocols shows
/// up under each.)
pub fn targets() -> Vec<Target> {
    let mut v: Vec<Target> = upnp::renderers().into_iter().map(Target::Upnp).collect();
    v.extend(cast::devices().into_iter().map(Target::Cast));
    v.sort_by_key(|t| t.name().to_lowercase());
    v
}

/// One network player by uid.
pub fn find(uid: &str) -> Option<Target> {
    if uid.starts_with("upnp:") {
        upnp::find(uid).map(Target::Upnp)
    } else if uid.starts_with("cast:") {
        cast::find(uid).map(Target::Cast)
    } else {
        None
    }
}

/// A uid that names a network player.
pub fn is_network_uid(uid: &str) -> bool {
    uid.starts_with("upnp:") || uid.starts_with("cast:")
}
