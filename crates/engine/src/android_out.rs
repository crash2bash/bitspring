//! Android output: AAudio for the samples, the Java AudioManager (over JNI) for devices.
//!
//! - **Bit-perfect** (Android 14+, USB DACs): Android can hand a USB device to one app
//!   unmixed — `AudioManager.setPreferredMixerAttributes` with a *bit-perfect* mixer. We pick
//!   one at the track's rate in the DAC's own integer format, open an AAudio stream in exactly
//!   that format, and Android passes our samples through untouched (no volume, no resampling).
//!   This is also what carries DoP.
//! - **Everything else** (speaker, wired, Bluetooth, older Android): the Android mixer, at its
//!   own rate; we resample to it ourselves rather than leave it to Android.

use std::ops::Deref;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, anyhow, bail};
use jni::objects::{JIntArray, JObject, JObjectArray, JString, JValue};
use jni::{JNIEnv, JavaVM};
use ndk::audio::{
    AudioCallbackResult, AudioContentType, AudioDirection, AudioError, AudioFormat, AudioPerformanceMode,
    AudioSharingMode, AudioStream, AudioStreamBuilder, AudioUsage,
};

use crate::decode::Track;
use crate::output::{self, OutBuf, OutFormat, PlayOptions, Playback, SignalPath, StreamFormat, Transport};
use crate::rate::RateRange;

// android.media constants (AudioDeviceInfo.TYPE_*, AudioFormat.ENCODING_*, …).
const GET_DEVICES_OUTPUTS: i32 = 2;
const MIXER_BEHAVIOR_BIT_PERFECT: i32 = 1;
const ENCODING_PCM_16BIT: i32 = 2;
const ENCODING_PCM_FLOAT: i32 = 4;
const ENCODING_PCM_24BIT_PACKED: i32 = 21;
const ENCODING_PCM_32BIT: i32 = 22;
const CHANNEL_OUT_STEREO: i32 = 12;
const USAGE_MEDIA: i32 = 1;
const CONTENT_TYPE_MUSIC: i32 = 2;
// AAudio formats the ndk crate has no names for.
const AAUDIO_FORMAT_PCM_I24_PACKED: i32 = 3;
const AAUDIO_FORMAT_PCM_I32: i32 = 4;

// ---------------------------------------------------------------------------------------------
// JNI plumbing
// ---------------------------------------------------------------------------------------------

/// Run `f` with a JNI env attached to this thread and the app's Context.
fn with_env<R>(f: impl FnOnce(&mut JNIEnv, &JObject) -> jni::errors::Result<R>) -> Result<R> {
    let ctx = ndk_context::android_context();
    // SAFETY: ndk-context holds the process's JavaVM and a global ref to the app Context,
    // valid for the life of the process.
    let vm = unsafe { JavaVM::from_raw(ctx.vm().cast()) }?;
    let mut env = vm.attach_current_thread_permanently()?;
    let context = unsafe { JObject::from_raw(ctx.context().cast()) };
    let r = env.with_local_frame(64, |env| f(env, &context));
    if env.exception_check().unwrap_or(false) {
        let _ = env.exception_describe();
        let _ = env.exception_clear();
    }
    r.map_err(|e| anyhow!("JNI: {e}"))
}

fn sdk_int(env: &mut JNIEnv) -> jni::errors::Result<i32> {
    env.get_static_field("android/os/Build$VERSION", "SDK_INT", "I")?.i()
}

fn audio_manager<'a>(env: &mut JNIEnv<'a>, context: &JObject) -> jni::errors::Result<JObject<'a>> {
    let name = env.new_string("audio")?;
    env.call_method(context, "getSystemService", "(Ljava/lang/String;)Ljava/lang/Object;", &[JValue::Object(&name)])?
        .l()
}

fn int_array(env: &mut JNIEnv, obj: JObject) -> jni::errors::Result<Vec<i32>> {
    let arr = JIntArray::from(obj);
    let len = env.get_array_length(&arr)? as usize;
    let mut v = vec![0; len];
    env.get_int_array_region(&arr, 0, &mut v)?;
    Ok(v)
}

fn java_string(env: &mut JNIEnv, obj: JObject) -> jni::errors::Result<String> {
    if obj.is_null() {
        return Ok(String::new());
    }
    let s = env.call_method(&obj, "toString", "()Ljava/lang/String;", &[])?.l()?;
    Ok(env.get_string(&JString::from(s))?.into())
}

/// The output AudioDeviceInfo objects, as local refs.
fn device_objects<'a>(env: &mut JNIEnv<'a>, am: &JObject) -> jni::errors::Result<Vec<JObject<'a>>> {
    let arr = JObjectArray::from(
        env.call_method(am, "getDevices", "(I)[Landroid/media/AudioDeviceInfo;", &[GET_DEVICES_OUTPUTS.into()])?.l()?,
    );
    let len = env.get_array_length(&arr)?;
    (0..len).map(|i| env.get_object_array_element(&arr, i)).collect()
}

/// One bit-perfect format a USB device offers.
#[derive(Debug, Clone, Copy, PartialEq)]
struct BitPerfect {
    rate: u32,
    encoding: i32,
}

impl BitPerfect {
    fn out_format(self) -> Option<(OutFormat, i32, u32)> {
        // (our writer, AAudio format, bits)
        match self.encoding {
            ENCODING_PCM_32BIT => Some((OutFormat::I32 { bits: 32 }, AAUDIO_FORMAT_PCM_I32, 32)),
            ENCODING_PCM_24BIT_PACKED => Some((OutFormat::I24Packed, AAUDIO_FORMAT_PCM_I24_PACKED, 24)),
            ENCODING_PCM_16BIT => Some((OutFormat::I16, i32::from(AudioFormat::PCM_I16), 16)),
            ENCODING_PCM_FLOAT => Some((OutFormat::F32, i32::from(AudioFormat::PCM_Float), 32)),
            _ => None,
        }
    }
    /// Integer formats first (what the DAC chip takes), widest first.
    fn rank(self) -> u32 {
        match self.encoding {
            ENCODING_PCM_32BIT => 0,
            ENCODING_PCM_24BIT_PACKED => 1,
            ENCODING_PCM_16BIT => 2,
            _ => 3,
        }
    }
}

/// Stereo bit-perfect mixer formats for a device (Android 14+, USB only), each with the Java
/// AudioMixerAttributes object to apply it.
fn bit_perfect_attrs<'a>(env: &mut JNIEnv<'a>, am: &JObject, dev: &JObject) -> jni::errors::Result<Vec<(BitPerfect, JObject<'a>)>> {
    if sdk_int(env)? < 34 {
        return Ok(Vec::new());
    }
    let list = env
        .call_method(am, "getSupportedMixerAttributes", "(Landroid/media/AudioDeviceInfo;)Ljava/util/List;", &[JValue::Object(dev)])?
        .l()?;
    let n = env.call_method(&list, "size", "()I", &[])?.i()?;
    let mut out = Vec::new();
    for i in 0..n {
        let attr = env.call_method(&list, "get", "(I)Ljava/lang/Object;", &[i.into()])?.l()?;
        if env.call_method(&attr, "getMixerBehavior", "()I", &[])?.i()? != MIXER_BEHAVIOR_BIT_PERFECT {
            continue;
        }
        let fmt = env.call_method(&attr, "getFormat", "()Landroid/media/AudioFormat;", &[])?.l()?;
        let rate = env.call_method(&fmt, "getSampleRate", "()I", &[])?.i()?;
        let encoding = env.call_method(&fmt, "getEncoding", "()I", &[])?.i()?;
        let mask = env.call_method(&fmt, "getChannelMask", "()I", &[])?.i()?;
        let bp = BitPerfect { rate: rate.max(0) as u32, encoding };
        if mask == CHANNEL_OUT_STEREO && rate > 0 && bp.out_format().is_some() {
            out.push((bp, attr));
        }
    }
    Ok(out)
}

fn media_attributes<'a>(env: &mut JNIEnv<'a>) -> jni::errors::Result<JObject<'a>> {
    let b = env.new_object("android/media/AudioAttributes$Builder", "()V", &[])?;
    let b = env.call_method(&b, "setUsage", "(I)Landroid/media/AudioAttributes$Builder;", &[USAGE_MEDIA.into()])?.l()?;
    let b = env
        .call_method(&b, "setContentType", "(I)Landroid/media/AudioAttributes$Builder;", &[CONTENT_TYPE_MUSIC.into()])?
        .l()?;
    env.call_method(&b, "build", "()Landroid/media/AudioAttributes;", &[])?.l()
}

fn find_device<'a>(env: &mut JNIEnv<'a>, am: &JObject, id: i32) -> jni::errors::Result<Option<JObject<'a>>> {
    for d in device_objects(env, am)? {
        if env.call_method(&d, "getId", "()I", &[])?.i()? == id {
            return Ok(Some(d));
        }
    }
    Ok(None)
}

/// Ask Android for the bit-perfect mixer at `want` on device `id`. True if it agreed.
fn prefer_bit_perfect(id: i32, want: BitPerfect) -> Result<bool> {
    with_env(|env, ctx| {
        let am = audio_manager(env, ctx)?;
        let Some(dev) = find_device(env, &am, id)? else { return Ok(false) };
        let Some((_, attr)) = bit_perfect_attrs(env, &am, &dev)?.into_iter().find(|(bp, _)| *bp == want) else {
            return Ok(false);
        };
        let aa = media_attributes(env)?;
        env.call_method(
            &am,
            "setPreferredMixerAttributes",
            "(Landroid/media/AudioAttributes;Landroid/media/AudioDeviceInfo;Landroid/media/AudioMixerAttributes;)Z",
            &[JValue::Object(&aa), JValue::Object(&dev), JValue::Object(&attr)],
        )?
        .z()
    })
}

/// Give the device back to the normal mixer (other apps' sound, system volume).
fn clear_bit_perfect(id: i32) {
    let _ = with_env(|env, ctx| {
        let am = audio_manager(env, ctx)?;
        let Some(dev) = find_device(env, &am, id)? else { return Ok(()) };
        let aa = media_attributes(env)?;
        env.call_method(
            &am,
            "clearPreferredMixerAttributes",
            "(Landroid/media/AudioAttributes;Landroid/media/AudioDeviceInfo;)Z",
            &[JValue::Object(&aa), JValue::Object(&dev)],
        )?;
        Ok(())
    });
}

/// The Android mixer's own rate.
fn mixer_rate(env: &mut JNIEnv, am: &JObject) -> jni::errors::Result<u32> {
    let key = env.new_string("android.media.property.OUTPUT_SAMPLE_RATE")?;
    let v = env.call_method(am, "getProperty", "(Ljava/lang/String;)Ljava/lang/String;", &[JValue::Object(&key)])?.l()?;
    Ok(java_string(env, v)?.parse().unwrap_or(48_000))
}

// ---------------------------------------------------------------------------------------------
// Devices
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct DeviceInfo {
    pub device: Device,
    pub name: String,
    pub uid: String,
    pub transport: Transport,
    pub output_channels: u32,
    /// The Android mixer's rate (or, for a bit-perfect USB device, its highest rate).
    pub nominal_rate: f64,
    /// Rates we can play at untouched: the bit-perfect ones, else just the mixer's.
    pub rates: Vec<RateRange>,
    pub is_default: bool,
    bit_perfect: Vec<BitPerfect>,
    mixer_rate: u32,
}

/// An output, by its AudioDeviceInfo id (stable while it stays connected).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Device {
    pub id: i32,
}

/// Device types worth listing, with how they connect, a name for the built-in ones, and a
/// priority for picking the default (higher wins: what the user plugged in last-ish).
fn classify(kind: i32) -> Option<(Transport, Option<&'static str>, u8)> {
    Some(match kind {
        2 => (Transport::BuiltIn, Some("Динамик"), 1),            // TYPE_BUILTIN_SPEAKER
        3 | 4 => (Transport::BuiltIn, Some("Наушники"), 3),       // TYPE_WIRED_HEADSET / HEADPHONES
        5 | 6 | 19 => (Transport::Other(kind as u32), None, 2),    // LINE_ANALOG / LINE_DIGITAL / AUX_LINE
        8 | 26 | 27 | 30 => (Transport::Bluetooth, None, 2),        // BLUETOOTH_A2DP / BLE_*
        9 | 10 | 29 => (Transport::Hdmi, None, 2),                  // HDMI / HDMI_ARC / HDMI_EARC
        11 | 12 | 22 => (Transport::Usb, None, 4),                  // USB_DEVICE / ACCESSORY / HEADSET
        13 => (Transport::Other(kind as u32), Some("Док-станция"), 2),
        _ => return None, // earpiece, telephony, remote submix, buses…
    })
}

fn list() -> Result<Vec<DeviceInfo>> {
    let raw = with_env(|env, ctx| {
        let am = audio_manager(env, ctx)?;
        let mixer = mixer_rate(env, &am)?;
        let mut out = Vec::new();
        for d in device_objects(env, &am)? {
            let id = env.call_method(&d, "getId", "()I", &[])?.i()?;
            let kind = env.call_method(&d, "getType", "()I", &[])?.i()?;
            let Some((transport, builtin, prio)) = classify(kind) else { continue };
            let name_obj = env.call_method(&d, "getProductName", "()Ljava/lang/CharSequence;", &[])?.l()?;
            let product = java_string(env, name_obj)?;
            let counts_obj = env.call_method(&d, "getChannelCounts", "()[I", &[])?.l()?;
            let channels = int_array(env, counts_obj)?.into_iter().max().unwrap_or(2).max(2) as u32;
            let bp: Vec<BitPerfect> = if transport == Transport::Usb {
                bit_perfect_attrs(env, &am, &d)?.into_iter().map(|(b, _)| b).collect()
            } else {
                Vec::new()
            };
            out.push((id, transport, builtin.map(str::to_string).unwrap_or(product), channels, prio, bp, mixer));
        }
        Ok(out)
    })?;
    let best = raw.iter().map(|r| r.4).max().unwrap_or(0);
    let mut seen_default = false;
    Ok(raw
        .into_iter()
        .map(|(id, transport, name, channels, prio, bit_perfect, mixer)| {
            let mut rates: Vec<u32> = bit_perfect.iter().map(|b| b.rate).collect();
            rates.sort_unstable();
            rates.dedup();
            if rates.is_empty() {
                rates.push(mixer);
            }
            let is_default = !seen_default && prio == best;
            seen_default |= is_default;
            DeviceInfo {
                device: Device { id },
                uid: format!("android:{id}"),
                name: if name.trim().is_empty() { "Аудиовыход".into() } else { name },
                transport,
                output_channels: channels,
                nominal_rate: *rates.last().unwrap() as f64,
                rates: rates.iter().map(|&r| RateRange::exact(r as f64)).collect(),
                is_default,
                bit_perfect,
                mixer_rate: mixer,
            }
        })
        .collect())
}

pub fn output_devices() -> Result<Vec<DeviceInfo>> {
    list()
}

impl Device {
    pub fn default_output() -> Result<Self> {
        list()?.into_iter().find(|d| d.is_default).map(|d| d.device).ok_or_else(|| anyhow!("нет устройства вывода"))
    }

    pub fn info(&self) -> Result<DeviceInfo> {
        list()?.into_iter().find(|d| d.device == *self).ok_or_else(|| anyhow!("устройство не найдено — отключено?"))
    }

    /// DoP needs the bit-perfect mixer at `dsd_rate / 16` in an integer format of ≥ 24 bits.
    pub fn supports_dop(&self, dsd_rate: u32) -> bool {
        self.info().is_ok_and(|i| {
            i.bit_perfect.iter().any(|b| b.rate == dsd_rate / 16 && matches!(b.encoding, ENCODING_PCM_32BIT | ENCODING_PCM_24BIT_PACKED))
        })
    }
}

// ---------------------------------------------------------------------------------------------
// Playback
// ---------------------------------------------------------------------------------------------

/// What the AAudio callback reads.
struct IoContext {
    playback: Playback,
    format: OutFormat,
    channels: usize,
}

pub struct Player {
    device: Device,
    // Field order = drop order: the stream (and its callback) goes before the state it reads.
    stream: AudioStream,
    io: Box<IoContext>,
    bit_perfect: bool,
    failure: Arc<Mutex<Option<String>>>,
    pub path: SignalPath,
}

impl Deref for Player {
    type Target = Playback;
    fn deref(&self) -> &Playback {
        &self.io.playback
    }
}

// SAFETY: AudioStream is a handle to a thread-safe AAudio stream; `io` is only read by the
// callback through its own pointer.
unsafe impl Send for Player {}
unsafe impl Sync for Player {}

impl Player {
    pub fn start(device: Device, track: Arc<Track>, opts: &PlayOptions) -> Result<Self> {
        let info = device.info()?;
        let dop_ok = track.dsd.is_some_and(|d| device.supports_dop(d.rate));
        let bit_perfect_on = opts.exclusive && !info.bit_perfect.is_empty();
        let rates = if bit_perfect_on { info.rates.clone() } else { vec![RateRange::exact(info.mixer_rate as f64)] };
        let opts_eff = PlayOptions { exclusive: bit_perfect_on, ..opts.clone() };
        let plan = output::plan(&track, &opts_eff, &rates, &info.name, dop_ok)?;
        let target = plan.target as u32;

        // The bit-perfect format at the target rate: integer first, ≥24-bit for DoP.
        let chosen = bit_perfect_on
            .then(|| {
                let mut c: Vec<BitPerfect> = info
                    .bit_perfect
                    .iter()
                    .copied()
                    .filter(|b| b.rate == target)
                    .filter(|b| !plan.dop || matches!(b.encoding, ENCODING_PCM_32BIT | ENCODING_PCM_24BIT_PACKED))
                    .collect();
                c.sort_by_key(|b| b.rank());
                c.first().copied()
            })
            .flatten();
        let bit_perfect = match chosen {
            Some(bp) => prefer_bit_perfect(device.id, bp).unwrap_or_else(|e| {
                log::warn!("bit-perfect mixer: {e:#}");
                false
            }),
            None => false,
        };
        if plan.dop && !bit_perfect {
            return Err(output::dop_error(&info.name, &track));
        }
        let (format, aaudio_format, bits) = match chosen.filter(|_| bit_perfect) {
            Some(bp) => bp.out_format().unwrap(),
            None => (OutFormat::F32, i32::from(AudioFormat::PCM_Float), 32),
        };
        // Without the bit-perfect mixer the stream runs at the mixer's rate.
        let (plan, rate) = if bit_perfect || !bit_perfect_on {
            (plan, target)
        } else {
            let p = output::plan(&track, &PlayOptions { exclusive: false, ..opts.clone() }, &[RateRange::exact(info.mixer_rate as f64)], &info.name, false)?;
            (p, info.mixer_rate)
        };

        let channels = 2usize;
        let playback = Playback::new(track.clone(), opts, &plan, rate as f64)?;
        let io = Box::new(IoContext { playback, format, channels });
        let io_addr = &*io as *const IoContext as usize;
        let failure = Arc::new(Mutex::new(None));
        let stream = {
            let failure = failure.clone();
            let result = AudioStreamBuilder::new()
                .map_err(|e| anyhow!("AAudio: {e}"))?
                .direction(AudioDirection::Output)
                .device_id(device.id)
                .sample_rate(rate as i32)
                .channel_count(channels as i32)
                .format(AudioFormat::from(aaudio_format))
                .sharing_mode(AudioSharingMode::Shared)
                .performance_mode(AudioPerformanceMode::None)
                .usage(AudioUsage::Media)
                .content_type(AudioContentType::Music)
                .data_callback(Box::new(move |_s, data, frames| {
                    // SAFETY: `io` is boxed and outlives the stream (field order); AAudio
                    // hands a buffer of `frames` frames in the stream's format.
                    let io = unsafe { &*(io_addr as *const IoContext) };
                    let len = frames.max(0) as usize * io.channels * io.format.bytes();
                    let bytes = unsafe { std::slice::from_raw_parts_mut(data as *mut u8, len) };
                    io.playback.shared().render(&mut [OutBuf { data: bytes, channels: io.channels, format: io.format }]);
                    AudioCallbackResult::Continue
                }))
                .error_callback(Box::new(move |_s, e| {
                    let msg = if e == AudioError::Disconnected { "устройство отключено".to_string() } else { format!("ошибка вывода: {e}") };
                    *failure.lock().unwrap() = Some(msg);
                    // SAFETY: as above.
                    unsafe { &*(io_addr as *const IoContext) }.playback.set_paused(true);
                }))
                .open_stream();
            match result {
                Ok(s) => s,
                Err(e) => {
                    if bit_perfect {
                        clear_bit_perfect(device.id);
                    }
                    bail!("не удалось открыть {}: {e}", info.name.trim());
                }
            }
        };
        let got = stream.sample_rate() as u32;
        if got != rate || i32::from(stream.format()) != aaudio_format {
            if bit_perfect {
                clear_bit_perfect(device.id);
            }
            bail!("{} открылся в другом формате ({} Гц) — попробуйте ещё раз", info.name.trim(), got);
        }
        let physical = StreamFormat {
            sample_rate: rate as f64,
            bits,
            float: format == OutFormat::F32,
            channels: channels as u32,
            non_mixable: bit_perfect,
        };
        let path = output::signal_path(&track, opts, &plan, rate as f64, Some(physical), format, bit_perfect, info.transport);
        stream.request_start().map_err(|e| anyhow!("AAudio start: {e}")).context("старт вывода")?;
        Ok(Self { device, stream, io, bit_perfect, failure, path })
    }

    pub fn device(&self) -> Device {
        self.device.clone()
    }

    /// The device went away (unplugged) or AAudio failed mid-stream; playback is paused.
    pub fn output_error(&self) -> Option<String> {
        self.failure.lock().unwrap().clone()
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        let _ = self.stream.request_stop();
        if self.bit_perfect {
            clear_bit_perfect(self.device.id);
        }
        // Then the stream closes (its field drops first), then `io` joins the render thread.
    }
}
