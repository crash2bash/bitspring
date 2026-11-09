//! Errors a listener gets to see, as codes with parameters rather than sentences.
//!
//! The engine raises them inside `anyhow` errors (`bail!(UserError::DeviceBusy)`, or as a
//! `.context(..)`), so they travel through `?` like any other error. A UI recovers the code
//! with [`UserError::of`] and words it in the listener's language; `Display` gives an English
//! sentence for the CLI and logs. Anything without a code (a corrupt file, an OS error) comes
//! out as [`UserError::Other`] with the error's own text.

use serde::Serialize;
use std::fmt;

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "code", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum UserError {
    /// Another app holds the device in exclusive mode.
    DeviceBusy,
    /// iOS: another app has the audio session.
    AudioBusy,
    /// iOS: the audio session couldn't be set up (`detail`: the system's reason).
    AudioSetup { detail: String },
    /// The device went away mid-stream.
    DeviceUnplugged,
    /// The device stopped asking for audio.
    DeviceStalled,
    /// The chosen device isn't there any more.
    DeviceNotFound,
    /// No output device at all.
    NoDevice,
    /// The device reports no sample rate we can use.
    NoRates { device: String },
    /// The device refused the stream format.
    FormatRefused,
    /// The device doesn't run at `khz` in exclusive mode.
    RateRefused { khz: f64 },
    /// macOS: the device didn't switch to `khz` (it stayed at `current` kHz).
    RateStuck { khz: f64, current: f64 },
    /// Windows: exclusive mode is switched off for this device in Sound settings.
    ExclusiveDisabled,
    /// No resampler for this ratio (`from` → `to` Hz).
    Resample { from: u32, to: u32 },
    /// `device` can't take DoP for this DSD rate (it needs `khz`, 24 bits, exclusive mode).
    DopRefused { device: String, dsd: String, khz: f64 },
    /// iOS: the volume left maximum during DoP, so DoP is muted until it's back.
    DopVolume,
    /// Android: the stream on `device` didn't open (`detail`: AAudio's reason).
    OpenFailed { device: String, detail: String },
    /// Android: `device` opened at another format (`khz`) than asked for; a retry helps.
    OtherFormat { device: String, khz: f64 },
    /// Android: AAudio failed mid-stream.
    OutputFailed { detail: String },
    /// The AutoEQ file couldn't be read (`detail`: the OS's reason).
    ProfileUnreadable { detail: String },
    /// The AutoEQ file has no filter lines.
    ProfileEmpty,
    /// iOS: copying an imported file `name` into the library failed.
    CopyFailed { name: String, detail: String },
    /// iOS: the app has no folder to keep music in.
    NoAppFolder,
    /// The playlist isn't there (deleted elsewhere).
    PlaylistNotFound,
    /// A playlist file or pasted text couldn't be read or understood.
    PlaylistUnreadable { detail: String },
    /// The playlist file holds no tracks.
    PlaylistNoTracks,
    /// Export: the file name ends in no format we write (.m3u8, .pls, .xspf, .csv).
    PlaylistFormat,
    /// A file couldn't be written (`detail`: the OS's reason).
    WriteFailed { detail: String },
    /// This build has no streaming services (App Store builds).
    StreamingOff,
    /// Not signed in to `service` (Qobuz, TIDAL).
    SignInNeeded { service: String },
    /// The sign-in to `service` has run out.
    SignInExpired { service: String },
    /// The sign-in window was closed before signing in.
    SignInCancelled,
    /// `service` gives only a 30-second preview of this track (the plan doesn't cover it).
    StreamPreview { service: String },
    /// `service` gives this track only encrypted, which the player doesn't decode.
    StreamEncrypted { service: String },
    /// `service` doesn't offer this track in the account's country or plan.
    StreamRestricted { service: String },
    /// `service` changed its player in a way this app doesn't follow yet.
    StreamChanged { service: String },
    /// `service` couldn't be reached (`detail`: the network's reason).
    ServiceUnreachable { service: String, detail: String },
    /// Anything else, in the words of whatever failed.
    Other { message: String },
}

impl UserError {
    /// The code inside `e`, if one was raised anywhere along its chain; else its text.
    pub fn of(e: &anyhow::Error) -> UserError {
        e.downcast_ref::<UserError>().cloned().unwrap_or_else(|| UserError::Other { message: format!("{e:#}") })
    }

    /// An error without a code.
    pub fn other(e: impl fmt::Display) -> UserError {
        UserError::Other { message: e.to_string() }
    }
}

impl From<anyhow::Error> for UserError {
    fn from(e: anyhow::Error) -> Self {
        UserError::of(&e)
    }
}

impl fmt::Display for UserError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        use UserError::*;
        match self {
            DeviceBusy => write!(f, "the device is held by another app in exclusive mode"),
            AudioBusy => write!(f, "another app is using the audio"),
            AudioSetup { detail } => write!(f, "couldn't set up audio: {detail}"),
            DeviceUnplugged => write!(f, "the device was disconnected"),
            DeviceStalled => write!(f, "the device stopped responding"),
            DeviceNotFound => write!(f, "device not found — was it unplugged?"),
            NoDevice => write!(f, "no output device"),
            NoRates { device } => write!(f, "{device} reports no usable sample rates"),
            FormatRefused => write!(f, "the device doesn't accept this format"),
            RateRefused { khz } => write!(f, "the device doesn't accept {khz} kHz in exclusive mode"),
            RateStuck { khz, current } => write!(f, "the device didn't switch to {khz} kHz (still {current} kHz)"),
            ExclusiveDisabled => write!(
                f,
                "exclusive mode is not allowed: Sound settings → device → Advanced → \"Allow applications to take exclusive control of this device\""
            ),
            Resample { from, to } => write!(f, "can't resample {from} Hz to {to} Hz"),
            DopRefused { device, dsd, khz } => {
                write!(f, "{device} can't take DoP for {dsd} (needs {khz} kHz, 24 bits, exclusive mode)")
            }
            DopVolume => write!(f, "iPhone volume is below maximum — DoP is muted; set the volume on the DAC"),
            OpenFailed { device, detail } => write!(f, "couldn't open {device}: {detail}"),
            OtherFormat { device, khz } => write!(f, "{device} opened in another format ({khz} kHz) — try again"),
            OutputFailed { detail } => write!(f, "output error: {detail}"),
            ProfileUnreadable { detail } => write!(f, "couldn't read the profile: {detail}"),
            ProfileEmpty => write!(
                f,
                "no filters found — expected AutoEQ ParametricEQ.txt lines like `Filter 1: ON PK Fc 100 Hz Gain 3 dB Q 1.0`"
            ),
            CopyFailed { name, detail } => write!(f, "couldn't copy {name}: {detail}"),
            NoAppFolder => write!(f, "the app has no music folder"),
            PlaylistNotFound => write!(f, "playlist not found"),
            PlaylistUnreadable { detail } => write!(f, "couldn't read the playlist: {detail}"),
            PlaylistNoTracks => write!(f, "the playlist has no tracks"),
            PlaylistFormat => write!(f, "unknown playlist format: use .m3u8, .pls, .xspf or .csv"),
            WriteFailed { detail } => write!(f, "couldn't write the file: {detail}"),
            StreamingOff => write!(f, "this version of the app has no streaming services"),
            SignInNeeded { service } => write!(f, "sign in to {service}"),
            SignInExpired { service } => write!(f, "the {service} sign-in has expired; sign in again"),
            SignInCancelled => write!(f, "sign-in cancelled"),
            StreamPreview { service } => write!(f, "{service} gives only a 30-second preview of this track: your plan doesn't cover it"),
            StreamEncrypted { service } => write!(f, "{service} gives this track only encrypted, which the player doesn't play"),
            StreamRestricted { service } => write!(f, "{service} doesn't offer this track in your country or plan"),
            StreamChanged { service } => write!(f, "{service} changed its player; this app needs an update"),
            ServiceUnreachable { service, detail } => write!(f, "couldn't reach {service}: {detail}"),
            Other { message } => f.write_str(message),
        }
    }
}

impl std::error::Error for UserError {}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::{Context, anyhow};

    #[test]
    fn code_survives_context_and_question_mark() {
        let inner: anyhow::Result<()> = Err(UserError::DeviceBusy.into());
        let wrapped = inner.context("starting output").unwrap_err();
        assert_eq!(UserError::of(&wrapped), UserError::DeviceBusy);

        let as_context: anyhow::Result<()> = Err(anyhow!("WASAPI: 0x88890004"));
        let e = as_context.context(UserError::DeviceUnplugged).unwrap_err();
        assert_eq!(UserError::of(&e), UserError::DeviceUnplugged);
    }

    #[test]
    fn plain_errors_keep_their_text() {
        let e = anyhow!("disk full").context("open x.flac");
        assert_eq!(UserError::of(&e), UserError::Other { message: "open x.flac: disk full".into() });
    }

    #[test]
    fn serializes_as_tagged_code() {
        let v = serde_json::to_string(&UserError::Resample { from: 44100, to: 12345 }).unwrap();
        assert_eq!(v, r#"{"code":"resample","from":44100,"to":12345}"#);
        let v = serde_json::to_string(&UserError::DeviceBusy).unwrap();
        assert_eq!(v, r#"{"code":"deviceBusy"}"#);
        let v = serde_json::to_string(&UserError::RateStuck { khz: 96.0, current: 44.1 }).unwrap();
        assert_eq!(v, r#"{"code":"rateStuck","khz":96.0,"current":44.1}"#);
    }
}
