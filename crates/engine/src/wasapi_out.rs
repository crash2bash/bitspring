//! Windows output through WASAPI.
//!
//! - **Exclusive mode** (the default) is Windows' bit-perfect path: the Windows mixer is
//!   bypassed, the DAC runs at the track's rate in its own integer format (32 or 24 bit), and
//!   our samples reach it unchanged. This is also what carries DoP.
//! - **Shared mode** goes through the Windows mixer at the rate set in Sound settings, so we
//!   resample to it ourselves (with our own resampler rather than Windows').
//!
//! WASAPI objects belong to the COM apartment of the thread that made them, so everything a
//! stream needs is created and used on its own "hifi-wasapi" thread.

use std::ops::Deref;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use anyhow::{Context, Result, anyhow, bail};
use wasapi::{Direction, SampleType, StreamMode, WasapiError, WaveFormat};
use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_EnumeratorName;
use windows::Win32::Media::Audio::{
    AUDCLNT_E_BUFFER_SIZE_NOT_ALIGNED, AUDCLNT_E_DEVICE_IN_USE, AUDCLNT_E_EXCLUSIVE_MODE_NOT_ALLOWED,
    AUDCLNT_E_UNSUPPORTED_FORMAT,
};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::Com::STGM_READ;
use windows::Win32::System::Threading::AvSetMmThreadCharacteristicsW;
use windows::Win32::System::Com::StructuredStorage::PropVariantToStringAlloc;
use windows::core::w;

use crate::decode::Track;
use crate::output::{self, OutBuf, OutFormat, PlayOptions, Playback, SignalPath, StreamFormat, Transport};
use crate::rate::RateRange;

/// Rates worth probing: every common rate of both families.
const RATES: [u32; 10] = [44_100, 48_000, 88_200, 96_000, 176_400, 192_000, 352_800, 384_000, 705_600, 768_000];

/// Exclusive formats in order of preference: integer before float (what DAC chips take), then
/// the widest. 24-in-32 is MSB-aligned, so it takes the same 32-bit words as true 32-bit.
const EXCLUSIVE_FORMATS: [(usize, usize, SampleType, OutFormat); 5] = [
    (32, 32, SampleType::Int, OutFormat::I32 { bits: 32 }),
    (32, 24, SampleType::Int, OutFormat::I32 { bits: 32 }),
    (24, 24, SampleType::Int, OutFormat::I24Packed),
    (16, 16, SampleType::Int, OutFormat::I16),
    (32, 32, SampleType::Float, OutFormat::F32),
];

/// Make sure this thread can talk to COM. A thread that is already in a single-threaded
/// apartment (the UI thread) says so with an error, and works as it is.
fn com() {
    let _ = wasapi::initialize_mta();
}

fn wasapi_err(e: WasapiError) -> anyhow::Error {
    if let WasapiError::Windows(w) = &e {
        let code = w.code();
        if code == AUDCLNT_E_DEVICE_IN_USE {
            return anyhow!("устройство занято другим приложением в монопольном режиме");
        }
        if code == AUDCLNT_E_EXCLUSIVE_MODE_NOT_ALLOWED {
            return anyhow!(
                "монопольный режим запрещён: Параметры звука → устройство → Дополнительно → «Разрешить приложениям использовать устройство в монопольном режиме»"
            );
        }
        if code == AUDCLNT_E_UNSUPPORTED_FORMAT {
            return anyhow!("устройство не принимает этот формат");
        }
    }
    anyhow!("WASAPI: {e}")
}

#[derive(Debug, Clone)]
pub struct DeviceInfo {
    pub device: Device,
    pub name: String,
    pub uid: String,
    pub transport: Transport,
    pub output_channels: u32,
    /// Rate of the Windows mixer (Sound settings → device → Format).
    pub nominal_rate: f64,
    /// Rates the device accepts in exclusive mode (or just the mixer rate, if none).
    pub rates: Vec<RateRange>,
    pub is_default: bool,
}

/// An output endpoint, by its WASAPI id (stable across reboots and replugs).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Device {
    pub id: String,
}

fn exact(hz: u32) -> RateRange {
    RateRange { min: hz as f64, max: hz as f64 }
}

impl Device {
    pub fn default_output() -> Result<Self> {
        com();
        let e = wasapi::DeviceEnumerator::new().map_err(wasapi_err)?;
        let d = e.get_default_device(Direction::Render).map_err(wasapi_err)?;
        Ok(Self { id: d.get_id().map_err(wasapi_err)? })
    }

    fn open(&self) -> Result<wasapi::Device> {
        com();
        let e = wasapi::DeviceEnumerator::new().map_err(wasapi_err)?;
        e.get_device(&self.id).map_err(|_| anyhow!("устройство не найдено — отключено?"))
    }

    pub fn info(&self) -> Result<DeviceInfo> {
        let d = self.open()?;
        let default = Self::default_output().ok();
        let name = d.get_friendlyname().unwrap_or_else(|_| "Audio device".into());
        let client = d.get_iaudioclient().map_err(wasapi_err)?;
        let mix = client.get_mixformat().map_err(wasapi_err)?;
        let probe = |hz: u32| exclusive_format(&client, hz, 2, false).is_some();
        let mut rates: Vec<RateRange> = RATES.iter().copied().filter(|&hz| probe(hz)).map(exact).collect();
        if rates.is_empty() {
            rates.push(exact(mix.get_samplespersec()));
        }
        Ok(DeviceInfo {
            device: self.clone(),
            transport: transport(&d, &name),
            output_channels: mix.get_nchannels() as u32,
            nominal_rate: mix.get_samplespersec() as f64,
            rates,
            is_default: default.as_ref() == Some(self),
            uid: self.id.clone(),
            name,
        })
    }

    /// Can this device carry DoP for a `dsd_rate` stream: exclusive mode at `dsd_rate / 16`
    /// with an integer format of at least 24 bits. (Whether the DAC decodes DoP can't be
    /// queried — that's the user's per-device switch.)
    pub fn supports_dop(&self, dsd_rate: u32) -> bool {
        let Ok(d) = self.open() else { return false };
        let Ok(client) = d.get_iaudioclient() else { return false };
        exclusive_format(&client, dsd_rate / 16, 2, true).is_some()
    }
}

/// The best exclusive format at `hz`. `dop`: only integer formats of 24 bits or more.
fn exclusive_format(client: &wasapi::AudioClient, hz: u32, channels: usize, dop: bool) -> Option<(WaveFormat, OutFormat)> {
    EXCLUSIVE_FORMATS
        .iter()
        .filter(|(_, valid, kind, _)| !dop || (*kind == SampleType::Int && *valid >= 24))
        .find_map(|&(store, valid, kind, out)| {
            let wf = WaveFormat::new(store, valid, kind, hz as usize, channels, None);
            client.is_supported_exclusive_with_quirks(&wf).ok().map(|wf| (wf, out))
        })
}

/// How the endpoint is attached, from its PnP enumerator ("USB", "HDAUDIO", "BTHENUM"…).
fn transport(d: &wasapi::Device, name: &str) -> Transport {
    let enumerator = (|| -> windows::core::Result<String> {
        // SAFETY: plain COM calls on a live IMMDevice; the string is freed after copying.
        unsafe {
            let store = d.as_immdevice().OpenPropertyStore(STGM_READ)?;
            let value = store.GetValue(&PKEY_Device_EnumeratorName)?;
            let p = PropVariantToStringAlloc(&value)?;
            let s = p.to_string().unwrap_or_default();
            CoTaskMemFree(Some(p.0 as *const _));
            Ok(s)
        }
    })()
    .unwrap_or_default()
    .to_ascii_uppercase();
    let lname = name.to_lowercase();
    if enumerator.starts_with("BTH") || lname.contains("bluetooth") || lname.contains("hands-free") {
        Transport::Bluetooth
    } else if enumerator == "USB" {
        Transport::Usb
    } else if lname.contains("hdmi") || lname.contains("displayport") || lname.contains("display audio") {
        Transport::Hdmi
    } else if enumerator == "HDAUDIO" || enumerator == "INTELAUDIO" {
        Transport::BuiltIn
    } else if enumerator == "ROOT" || enumerator == "SWD" {
        Transport::Virtual
    } else {
        Transport::Other(0)
    }
}

pub fn output_devices() -> Result<Vec<DeviceInfo>> {
    com();
    let e = wasapi::DeviceEnumerator::new().map_err(wasapi_err)?;
    let list = e.get_device_collection(Direction::Render).map_err(wasapi_err)?;
    let mut out = Vec::new();
    for d in &list {
        let Ok(d) = d else { continue };
        let Ok(id) = d.get_id() else { continue };
        if let Ok(info) = (Device { id }).info() {
            out.push(info);
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------------------------
// Playback
// ---------------------------------------------------------------------------------------------

/// What the stream thread got the device to run in.
struct Negotiated {
    rate: f64,
    format: OutFormat,
    physical: StreamFormat,
}

pub struct Player {
    device: Device,
    playback: Arc<Playback>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    /// Why the stream thread stopped early (device unplugged), if it did.
    failure: Arc<Mutex<Option<String>>>,
    pub path: SignalPath,
}

impl Deref for Player {
    type Target = Playback;
    fn deref(&self) -> &Playback {
        &self.playback
    }
}

impl Player {
    pub fn start(device: Device, track: Arc<Track>, opts: &PlayOptions) -> Result<Self> {
        let info = device.info()?;
        let dop_ok = track.dsd.is_some_and(|d| device.supports_dop(d.rate));
        // Shared mode can only run at the mixer's rate.
        let rates = if opts.exclusive { info.rates.clone() } else { vec![RateRange { min: info.nominal_rate, max: info.nominal_rate }] };
        let plan = output::plan(&track, opts, &rates, &info.name, dop_ok)?;

        let (fmt_tx, fmt_rx) = mpsc::sync_channel::<Result<Negotiated>>(1);
        let (pb_tx, pb_rx) = mpsc::sync_channel::<Option<Arc<Playback>>>(1);
        let stop = Arc::new(AtomicBool::new(false));
        let failure = Arc::new(Mutex::new(None));
        let thread = {
            let (id, exclusive, target, dop) = (device.id.clone(), opts.exclusive, plan.target as u32, plan.dop);
            let (stop, failure) = (stop.clone(), failure.clone());
            std::thread::Builder::new().name("hifi-wasapi".into()).spawn(move || {
                if let Err(e) = stream_thread(&id, exclusive, target, dop, &fmt_tx, &pb_rx, &stop) {
                    // Before negotiation the error goes back to `start`; after, it's a failure.
                    let msg = format!("{e:#}");
                    if fmt_tx.try_send(Err(e)).is_err() {
                        *failure.lock().unwrap() = Some(msg);
                    }
                }
            })?
        };
        let neg = match fmt_rx.recv() {
            Ok(Ok(n)) => n,
            Ok(Err(e)) => {
                let _ = thread.join();
                return Err(e);
            }
            Err(_) => {
                let _ = thread.join();
                bail!("audio thread ended unexpectedly");
            }
        };
        if plan.dop && (!neg.format.carries_dop() || neg.format == OutFormat::F32 || (neg.rate - track.sample_rate as f64).abs() >= 0.5) {
            let _ = pb_tx.send(None);
            let _ = thread.join();
            return Err(output::dop_error(&info.name, &track));
        }
        let path = output::signal_path(&track, opts, &plan, neg.rate, Some(neg.physical), neg.format, opts.exclusive, info.transport);
        let playback = match Playback::new(track, opts, &plan, neg.rate) {
            Ok(p) => Arc::new(p),
            Err(e) => {
                let _ = pb_tx.send(None);
                let _ = thread.join();
                return Err(e);
            }
        };
        let _ = pb_tx.send(Some(playback.clone()));
        Ok(Self { device, playback, stop, thread: Some(thread), failure, path })
    }

    pub fn device(&self) -> Device {
        self.device.clone()
    }

    /// The device went away mid-stream (unplugged, or taken by another app).
    pub fn output_error(&self) -> Option<String> {
        self.failure.lock().unwrap().clone()
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        // `playback` drops after this, joining the render thread.
    }
}

/// Open the device, negotiate the format, report it, then feed the device until `stop`.
fn stream_thread(
    id: &str,
    exclusive: bool,
    target: u32,
    dop: bool,
    fmt_tx: &mpsc::SyncSender<Result<Negotiated>>,
    pb_rx: &mpsc::Receiver<Option<Arc<Playback>>>,
    stop: &AtomicBool,
) -> Result<()> {
    com();
    let device = Device { id: id.to_string() }.open()?;
    let mut client = device.get_iaudioclient().map_err(wasapi_err)?;

    let (wf, format, mode) = if exclusive {
        let (wf, format) = exclusive_format(&client, target, 2, dop)
            .ok_or_else(|| anyhow!("устройство не принимает {} кГц в монопольном режиме", target as f64 / 1000.0))?;
        let (default_period, _) = client.get_device_period().map_err(wasapi_err)?;
        let period = client.calculate_aligned_period_near(default_period, Some(128), &wf).map_err(wasapi_err)?;
        (wf, format, StreamMode::EventsExclusive { period_hns: period })
    } else {
        let mix = client.get_mixformat().map_err(wasapi_err)?;
        let format = match (mix.get_subformat(), mix.get_bitspersample(), mix.get_validbitspersample()) {
            (Ok(SampleType::Float), 32, _) => OutFormat::F32,
            (Ok(SampleType::Int), 16, _) => OutFormat::I16,
            (Ok(SampleType::Int), 24, _) => OutFormat::I24Packed,
            (Ok(SampleType::Int), 32, _) => OutFormat::I32 { bits: 32 },
            _ => bail!("unsupported Windows mixer format"),
        };
        let (default_period, _) = client.get_device_period().map_err(wasapi_err)?;
        (mix, format, StreamMode::EventsShared { autoconvert: false, buffer_duration_hns: 2 * default_period })
    };

    match client.initialize_client(&wf, Direction::Render, mode, None) {
        Ok(()) => {}
        // Exclusive mode wants a period that fits the device's buffer alignment: retry with
        // the size it suggests, as Microsoft's docs prescribe.
        Err(WasapiError::Windows(e)) if e.code() == AUDCLNT_E_BUFFER_SIZE_NOT_ALIGNED => {
            let frames = client.get_buffer_size().map_err(wasapi_err)?;
            let period = wasapi::calculate_period_100ns(frames as i64, wf.get_samplespersec() as i64);
            client = device.get_iaudioclient().map_err(wasapi_err)?;
            client
                .initialize_client(&wf, Direction::Render, StreamMode::EventsExclusive { period_hns: period }, None)
                .map_err(wasapi_err)?;
        }
        Err(e) => return Err(wasapi_err(e)),
    }
    let event = client.set_get_eventhandle().map_err(wasapi_err)?;
    let render = client.get_audiorenderclient().map_err(wasapi_err)?;
    let channels = wf.get_nchannels() as usize;
    let block = wf.get_blockalign() as usize;
    let buffer_frames = client.get_buffer_size().map_err(wasapi_err)? as usize;

    let physical = StreamFormat {
        sample_rate: wf.get_samplespersec() as f64,
        bits: wf.get_validbitspersample() as u32,
        float: matches!(wf.get_subformat(), Ok(SampleType::Float)),
        channels: channels as u32,
        non_mixable: exclusive,
    };
    let rate = wf.get_samplespersec() as f64;
    if fmt_tx.send(Ok(Negotiated { rate, format, physical })).is_err() {
        return Ok(());
    }
    let Ok(Some(playback)) = pb_rx.recv() else { return Ok(()) };

    // Pro Audio scheduling class: the thread wakes on time even under load.
    // SAFETY: plain Win32 call; the handle is released when the thread ends.
    let _mmcss = unsafe { AvSetMmThreadCharacteristicsW(w!("Pro Audio"), &mut 0) };

    let mut scratch = vec![0u8; buffer_frames.max(1) * block];
    let mut write = |frames: usize| -> Result<()> {
        if frames == 0 {
            return Ok(());
        }
        let data = &mut scratch[..frames * block];
        playback.shared().render(&mut [OutBuf { data: &mut *data, channels, format }]);
        render.write_to_device(data, None).map_err(wasapi_err)
    };

    // Fill the whole buffer before starting, so playback opens on audio, not on silence.
    write(client.get_available_space_in_frames().map_err(wasapi_err)? as usize)?;
    client.start_stream().map_err(wasapi_err)?;
    let mut timeouts = 0;
    let result = loop {
        if stop.load(Ordering::Acquire) {
            break Ok(());
        }
        if event.wait_for_event(200).is_err() {
            // No buffer request for 200 ms: the device is gone (or the system is stalling).
            timeouts += 1;
            if timeouts >= 10 {
                break Err(anyhow!("устройство перестало отвечать"));
            }
            continue;
        }
        timeouts = 0;
        let frames = match client.get_available_space_in_frames() {
            Ok(f) => f as usize,
            Err(e) => break Err(wasapi_err(e)).context("устройство отключено"),
        };
        if let Err(e) = write(frames.min(buffer_frames)) {
            break Err(e).context("устройство отключено");
        }
    };
    let _ = client.stop_stream();
    result
}
