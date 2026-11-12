//! Playing on network players: UPnP/DLNA renderers (Sonos, WiiM, Bluesound, Linn, …).
//!
//! The renderer fetches a live stream from a small HTTP server of ours: the same samples the
//! local path would send a DAC, after the same processing, encoded as FLAC (or WAV / L16 for
//! players that don't take FLAC). One stream carries track after track of the same format,
//! so albums play without gaps on any renderer.

pub mod encode;
#[cfg(test)]
mod fake;
pub mod http;
pub mod player;
pub mod upnp;
