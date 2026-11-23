//! `hifi` — developer CLI for the engine: list devices, inspect files, play bit-perfect.

use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use clap::{Parser, Subcommand};
use hifi_engine::output::{self, Device, DeviceInfo, PlayOptions};
use hifi_engine::player::{self as outputs, Output, Player};
use hifi_engine::dsp::crossfeed::CrossfeedLevel;
use hifi_engine::dsp::dsd_encode::{DsdOut, DsdRate, ModOrder};
use hifi_engine::dsp::upsample::{self, FilterKind, FilterSpec};
use hifi_engine::dsp::{DspSettings, Upsample};
use hifi_engine::dsp::lossy::LossySettings;
use hifi_engine::dsp::loudness::{self, Known, LevelMode, LevelSettings, Measured};
use hifi_engine::dsd::{self, DsdMode};
use hifi_engine::{Track, load_track, open_file};

#[derive(Parser)]
#[command(name = "hifi", about = "Bit-perfect audio player (engine dev CLI)")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// List output devices and the sample rates they support.
    Devices,
    /// Decode a file and print what's inside.
    Info { file: PathBuf },
    /// Play files, one after another without a break where they share a format.
    Play {
        #[arg(required = true)]
        files: Vec<PathBuf>,
        /// Device index (from `devices`) or part of its name/UID. Default: system output.
        #[arg(short, long)]
        device: Option<String>,
        /// Shared mode: don't take exclusive (hog) access.
        #[arg(long)]
        shared: bool,
        /// Upsample to the highest same-family rate up to this many kHz (e.g. 192, 384).
        #[arg(long, value_name = "KHZ")]
        upsample: Option<f64>,
        /// With --upsample or --dsd: a long filter instead of the standard one (linear, minimum
        /// or apodizing), where the DAC runs at twice the source rate or more.
        #[arg(long, value_name = "KIND")]
        filter: Option<String>,
        /// Length of the long filter (a power of two from 4096 to 16777216; minimum phase up to
        /// 1048576; apodizing has its own).
        #[arg(long, value_name = "N", default_value_t = 65_536)]
        taps: usize,
        /// PCM files: encode to DSD at this rate (64, 128 or 256) and send it as DoP. Needs
        /// exclusive access and a DAC that takes DoP at the wire rate (176.4, 352.8 or 705.6
        /// kHz); otherwise the file plays as PCM.
        #[arg(long, value_name = "RATE")]
        dsd: Option<u32>,
        /// With --dsd: order of the modulator, 5 or 7 (the seventh: lower noise in the audio band,
        /// a little more work).
        #[arg(long, value_name = "ORDER", default_value_t = 7)]
        modulator: u32,
        /// AutoEQ ParametricEQ.txt profile to apply.
        #[arg(long, value_name = "FILE")]
        eq: Option<PathBuf>,
        /// Headphone crossfeed: default, chu-moy or jan-meier.
        #[arg(long, value_name = "LEVEL")]
        crossfeed: Option<String>,
        /// Lossy files: lower the level by exactly the decoded overshoot (anti-clipping).
        #[arg(long)]
        headroom: bool,
        /// Lossy files: synthesise the octave above the encoder's low-pass.
        #[arg(long)]
        restore_highs: bool,
        /// DSD files: send the bitstream as DoP (the DAC must support it). Default: convert to PCM.
        #[arg(long)]
        dop: bool,
        /// Volume levelling: track (each file equally loud) or album (the files given, as one).
        #[arg(long, value_name = "MODE")]
        level: Option<String>,
        /// With other processing on: lower a track whose true peak would overshoot full scale.
        #[arg(long)]
        peak_headroom: bool,
    },
    /// Measure files as volume levelling does: loudness (EBU R128), true peak, and the gain to
    /// −18 LUFS; then the same for all of them as one album.
    Loudness {
        #[arg(required = true)]
        files: Vec<PathBuf>,
    },
    /// Show (or set, 0–100) a network player's own volume.
    Volume { device: String, set: Option<u32> },
    /// Set a device's sample rate (Hz), like Audio MIDI Setup does.
    #[cfg(target_os = "macos")]
    Rate { device: String, hz: f64 },
    /// List the physical stream formats a device advertises.
    #[cfg(target_os = "macos")]
    Formats { device: String },
}

fn main() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::Devices => {
            list_devices()?;
            list_renderers();
            Ok(())
        }
        Cmd::Info { file } => info(file),
        Cmd::Volume { device, set } => {
            let Output::Network(t) = pick_output(Some(&device))? else { bail!("not a network player") };
            if let Some(v) = set {
                t.set_volume(v)?;
            }
            println!("{}: volume {}", t.name(), t.volume()?);
            Ok(())
        }
        Cmd::Loudness { files } => loudness(files),
        Cmd::Play { files, device, shared, upsample, filter, taps, dsd, modulator, eq, crossfeed, headroom, restore_highs, dop, level, peak_headroom } => {
            let kind = match filter.as_deref() {
                None => None,
                Some("linear") => Some(FilterKind::Linear),
                Some("minimum") => Some(FilterKind::Minimum),
                Some("apodizing") => Some(FilterKind::Apodizing),
                Some(other) => bail!("unknown filter {other:?}"),
            };
            if kind.is_some() && upsample.is_none() && dsd.is_none() {
                bail!("--filter needs --upsample or --dsd");
            }
            let dsd_out = match dsd {
                None => None,
                Some(m) => {
                    let Some(rate) = DsdRate::from_multiple(m) else { bail!("unknown DSD rate {m} (64, 128 or 256)") };
                    let Some(order) = ModOrder::from_number(modulator) else { bail!("unknown modulator order {modulator} (5 or 7)") };
                    Some(DsdOut { rate, order })
                }
            };
            let dsp = DspSettings {
                upsample: upsample.map_or(Upsample::Off, |k| Upsample::UpTo(k * 1000.0)),
                upsample_filter: kind.map(|kind| FilterSpec { kind, taps }),
                eq: match eq {
                    Some(p) => hifi_engine::dsp::eq_io::import(
                        &p.file_stem().unwrap_or_default().to_string_lossy(),
                        &std::fs::read_to_string(&p)?,
                    )?
                    .into_iter()
                    .next(),
                    None => None,
                },
                crossfeed: match crossfeed.as_deref() {
                    None => None,
                    Some("default") => Some(CrossfeedLevel::DEFAULT),
                    Some("chu-moy") => Some(CrossfeedLevel::CHU_MOY),
                    Some("jan-meier") => Some(CrossfeedLevel::JAN_MEIER),
                    Some(other) => bail!("unknown crossfeed level {other:?}"),
                },
                lossy: LossySettings { headroom, restore_highs },
                convolution: None,
                eq_bypass: false,
                level: LevelSettings {
                    mode: match level.as_deref() {
                        None => LevelMode::Off,
                        Some("track") => LevelMode::Track,
                        Some("album") => LevelMode::Album,
                        Some(other) => bail!("unknown levelling mode {other:?}"),
                    },
                    headroom: peak_headroom,
                },
                dsd_out,
            };
            let mode = if dop { DsdMode::Dop } else { DsdMode::Pcm };
            play(files, device, mode, PlayOptions { exclusive: !shared, dsp, ..Default::default() })
        }
        #[cfg(target_os = "macos")]
        Cmd::Rate { device, hz } => {
            let d = pick_device(Some(&device))?;
            d.device.set_nominal_rate(hz)?;
            println!("{} → {} kHz", d.name, d.device.nominal_rate()? / 1000.0);
            Ok(())
        }
        #[cfg(target_os = "macos")]
        Cmd::Formats { device } => {
            let d = pick_device(Some(&device))?;
            println!("{}:", d.name);
            println!("  stereo channels (L, R): {:?}", d.device.preferred_stereo_channels()?);
            if let Some(f) = d.device.current_physical_format()? {
                println!(
                    "  current: {} bit {}, {}, {} kHz",
                    f.bits,
                    if f.float { "float" } else { "integer" },
                    if f.non_mixable { "non-mixable" } else { "mixable" },
                    f.sample_rate / 1000.0
                );
            }
            for (f, r) in d.device.physical_formats()? {
                println!(
                    "  {:>2} bit {:<7} {} ch {:<11} @ {}",
                    f.bits,
                    if f.float { "float" } else { "integer" },
                    f.channels,
                    if f.non_mixable { "non-mixable" } else { "mixable" },
                    output::format_rates(&[r])
                );
            }
            Ok(())
        }
    }
}

fn list_devices() -> Result<()> {
    outputs::start_discovery();
    for (i, d) in output::output_devices()?.iter().enumerate() {
        println!(
            "{} [{i}] {}  ({:?}, {} ch, now {} kHz)\n      rates: {}\n      uid:   {}",
            if d.is_default { "*" } else { " " },
            d.name,
            d.transport,
            d.output_channels,
            d.nominal_rate / 1000.0,
            output::format_rates(&d.rates),
            d.uid,
        );
    }
    Ok(())
}

/// Network players answer within a few seconds of the search.
fn list_renderers() {
    std::thread::sleep(Duration::from_millis(3500));
    for t in hifi_engine::net::targets() {
        println!(
            "  [net] {}  ({}, {} {}, {:?}, up to {} kHz / {} bit)\n      uid:   {}",
            t.name(),
            t.model(),
            t.protocol(),
            t.ip(),
            t.codec(),
            t.max_rate() as f64 / 1000.0,
            t.max_bits(),
            t.uid()
        );
    }
}

fn loudness(files: Vec<PathBuf>) -> Result<()> {
    let mut all = Vec::new();
    for f in &files {
        let t0 = Instant::now();
        let m = loudness::analyze(f)?;
        let took = t0.elapsed();
        let name = f.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        match m.loudness() {
            Some(l) => println!("{:7.2} LUFS  peak {:6.2} dBTP  gain {:+6.2} dB  {:>7.2?}  {name}", loudness::TARGET_LUFS - l.gain_db, 20.0 * m.peak.max(1e-9).log10(), l.gain_db, took),
            None => println!("silence                                                  {name}"),
        }
        let tags = load_track(f).map(|t| t.meta.gain).unwrap_or_default();
        if !tags.is_empty() {
            println!("        tags: track {:?} dB peak {:?}, album {:?} dB peak {:?}", tags.track_db, tags.track_peak, tags.album_db, tags.album_peak);
        }
        all.push(m);
    }
    if let Some(a) = loudness::album(all.iter()).filter(|_| all.len() > 1) {
        println!("{:7.2} LUFS  peak {:6.2} dBTP  gain {:+6.2} dB  album", loudness::TARGET_LUFS - a.gain_db, 20.0 * a.peak.unwrap_or(0.0).max(1e-9).log10(), a.gain_db);
    }
    Ok(())
}

fn info(file: PathBuf) -> Result<()> {
    if dsd::is_dsd_path(&file) {
        let h = dsd::probe(&file)?;
        println!(
            "{}\n  {} {} ({:.4} MHz), {} ch, {:.1} s\n  as PCM: {} kHz; as DoP: {} kHz / 24 bit",
            file.display(),
            h.container,
            dsd::dsd_name(h.rate),
            h.rate as f64 / 1e6,
            h.channels,
            h.duration_secs(),
            dsd::pcm_rate(h.rate) as f64 / 1000.0,
            h.rate as f64 / 16_000.0,
        );
        if let (Some(a), Some(t)) = (&h.meta.artist, &h.meta.title) {
            println!("  {a} — {t}");
        }
        return Ok(());
    }
    let t0 = Instant::now();
    let track = load_track(&file)?;
    println!(
        "{}\n  {} Hz, {} bit, {} ch, {:.1} s ({} frames)\n  {:.0} MB in RAM, decoded in {:.2?}",
        file.display(),
        track.sample_rate,
        track.bits_per_sample.map_or("?".into(), |b| b.to_string()),
        track.channels,
        track.duration_secs(),
        track.frames(),
        (track.frames() * track.channels * 4) as f64 / 1e6,
        t0.elapsed(),
    );
    if track.lossy {
        let peak_db = 20.0 * (track.peak() as f64).max(1e-9).log10();
        println!("  lossy: decoded peak {peak_db:+.2} dBFS{}", if peak_db > 0.0 { " (clips!)" } else { "" });
        match hifi_engine::dsp::lossy::detect_cutoff(&track) {
            Some(hz) => println!("  encoder low-pass at {:.1} kHz", hz / 1000.0),
            None => println!("  no encoder low-pass found (full band)"),
        }
    }
    Ok(())
}

fn pick_device(query: Option<&str>) -> Result<DeviceInfo> {
    let devices = output::output_devices()?;
    let found = match query {
        None => {
            let def = Device::default_output()?;
            devices.into_iter().find(|d| d.device == def)
        }
        Some(q) => match q.parse::<usize>() {
            Ok(i) => devices.into_iter().nth(i),
            Err(_) => {
                let q = q.to_lowercase();
                devices
                    .into_iter()
                    .find(|d| d.name.to_lowercase().contains(&q) || d.uid.to_lowercase().contains(&q))
            }
        },
    };
    match found {
        Some(d) => Ok(d),
        None => bail!("no such output device; run `hifi devices`"),
    }
}

/// A local device, or else a network player whose name or uid contains `query`.
fn pick_output(query: Option<&str>) -> Result<Output> {
    if let Ok(d) = pick_device(query) {
        return Ok(Output::Device(d));
    }
    let q = query.unwrap_or_default().to_lowercase();
    if let Some(o) = outputs::find(&q)? {
        return Ok(o);
    }
    outputs::start_discovery();
    for _ in 0..20 {
        std::thread::sleep(Duration::from_millis(250));
        if let Some(t) = hifi_engine::net::targets()
            .into_iter()
            .find(|t| t.name().to_lowercase().contains(&q) || t.uid().to_lowercase().contains(&q))
        {
            return Ok(Output::Network(t));
        }
    }
    bail!("no such output; run `hifi devices`")
}

fn play(files: Vec<PathBuf>, device: Option<String>, dsd_mode: DsdMode, opts: PlayOptions) -> Result<()> {
    let out = pick_output(device.as_deref())?;
    let dev_name = out.name().to_string();
    // Network players take PCM only.
    let dsd_mode = if out.is_network() { DsdMode::Pcm } else { dsd_mode };
    // Levelling: what the tags don't say is measured first (the files given are the album).
    let mut measured: Vec<Option<Measured>> = vec![None; files.len()];
    if opts.dsp.levels() {
        for (m, f) in measured.iter_mut().zip(&files) {
            *m = loudness::analyze(f).ok();
        }
    }
    let album = measured.iter().cloned().collect::<Option<Vec<_>>>().and_then(|all| loudness::album(all.iter()));
    let open = |file: &PathBuf| -> Result<Arc<Track>> {
        print!("Opening {}… ", file.display());
        std::io::stdout().flush()?;
        let t0 = Instant::now();
        let track = open_file(file, dsd_mode)?;
        println!("{:.2?}", t0.elapsed());
        if opts.dsp.levels() && !track.is_dop() {
            let i = files.iter().position(|f| f == file).unwrap_or(0);
            let tags = track.meta.gain.known();
            let known = Known { track: tags.track.or(measured[i].as_ref().and_then(|m| m.loudness())), album: tags.album.or(album) };
            track.set_level(loudness::resolve(&opts.dsp.level, opts.dsp.reshapes(), &known));
        }
        Ok(track)
    };

    let stop = Arc::new(AtomicBool::new(false));
    let s = stop.clone();
    ctrlc::set_handler(move || s.store(true, Ordering::Relaxed))?;

    let mut i = 0;
    let mut track = open(&files[0])?;
    loop {
        let mut player = Player::start(&out, track, &opts)?;
        describe(&dev_name, &player.path);
        if let Some(o) = opts.dsp.dsd_out.filter(|_| player.path.dsd_out.is_none() && !player.path.dsd.is_some_and(|d| d.dop)) {
            println!("Note:    DSD{} asked for, but this output can't take DoP at {} kHz: PCM instead", o.rate.multiple(), o.rate.wire_rate() as f64 / 1000.0);
        }
        // A long filter holds seconds of input back: line the next file up that much earlier,
        // or the render thread reaches the end before it is there.
        let held = player.path.upsample_filter.map_or(0.0, |f| upsample::filter(f).startup_frames() as f64 / player.path.source_rate as f64);
        // The next file, opened ahead: joined on, or to start anew if it couldn't be.
        let mut ahead: Option<(usize, Arc<Track>)> = None;
        let mut switches = player.switches();
        while !player.is_finished() && !stop.load(Ordering::Relaxed) {
            if player.switches() != switches {
                switches = player.switches();
                i += 1;
                ahead = None;
                player.path = player.path.for_next(&player.heard());
                println!("\n→ joined on without a break: {}", files[i].display());
                describe(&dev_name, &player.path);
            }
            let (pos, dur) = (player.position_secs(), player.duration_secs());
            if ahead.is_none() && i + 1 < files.len() && dur - pos < 20.0 + held {
                println!();
                let t = open(&files[i + 1])?;
                if player.queue_next(t.clone()) {
                    println!("  lined up to follow without a break");
                } else {
                    println!("  can't follow without a break (another format): starts anew");
                }
                ahead = Some((i + 1, t));
            }
            if let Some(e) = player.output_error() {
                bail!(e);
            }
            print!("\r  {} / {}  ", mmss(pos), mmss(dur));
            std::io::stdout().flush()?;
            std::thread::sleep(Duration::from_millis(200));
        }
        println!();
        if let Some(peak) = player.output_peak() {
            let load = player.load().map_or("–".into(), |l| format!("{:.1}% of a core", l * 100.0));
            println!("Peak:    {:.1} dBFS, underruns: {}, load: {load}", 20.0 * peak.max(1e-9).log10(), player.underruns());
        }
        drop(player); // releases hog mode and restores the device's previous rate
        match ahead {
            Some((j, t)) if !stop.load(Ordering::Relaxed) => {
                i = j;
                track = t;
            }
            _ => return Ok(()),
        }
    }
}

fn describe(dev_name: &str, p: &hifi_engine::output::SignalPath) {
    println!("Output:  {} ({:?})", dev_name, p.transport);
    match p.dsd {
        Some(d) => println!(
            "Source:  {} ({:.4} MHz), {}",
            dsd::dsd_name(d.rate),
            d.rate as f64 / 1e6,
            if d.dop { "DoP (DSD bitstream in 24-bit PCM frames)" } else { "converted to PCM" }
        ),
        None => println!(
            "Source:  {} Hz / {} bit",
            p.source_rate,
            p.source_bits.map_or("?".into(), |b| b.to_string())
        ),
    }
    match p.physical {
        Some(f) => println!(
            "DAC:     {} Hz / {} bit {}",
            p.device_rate,
            f.bits,
            if f.float { "float" } else { "integer" }
        ),
        None => println!("DAC:     {} Hz / format unknown", p.device_rate),
    }
    println!(
        "Mode:    {}, {}",
        if p.exclusive { "exclusive (hog)" } else { "shared" },
        if p.integer_mode { "integer mode (no HAL conversion)" } else { "float32 → HAL → DAC" }
    );
    let mut dsp = Vec::new();
    if let Some(d) = p.dsd.filter(|d| !d.dop) {
        dsp.push(format!("{} → PCM {} kHz", dsd::dsd_name(d.rate), p.source_rate as f64 / 1000.0));
    }
    if p.resampled {
        dsp.push(match p.upsample_filter {
            Some(f) => format!("upsample {} → {} Hz, {:?} filter {} taps", p.source_rate, p.device_rate, f.kind, f.taps),
            None => format!("resample {} → {} Hz", p.source_rate, p.device_rate),
        });
    }
    if let Some(name) = &p.eq {
        dsp.push(format!("EQ {name}"));
    }
    if let Some(o) = p.dsd_out {
        dsp.push(format!("encoded to DSD{} ({}th-order modulator), sent as DoP", o.rate.multiple(), o.order.get()));
    }
    if let Some(cf) = p.crossfeed {
        dsp.push(format!("crossfeed {} Hz / {} dB", cf.cutoff_hz, cf.feed_db));
    }
    if let Some(l) = &p.level {
        dsp.push(format!("level {:+.2} dB ({:?}{})", l.gain_db(), l.basis, if l.limited { ", held by the peak" } else { "" }));
    }
    if let Some(l) = &p.lossy {
        if l.gain < 1.0 {
            dsp.push(format!("headroom {:.2} dB{}", l.gain_db(), if l.gain_measured { "" } else { " (default)" }));
        }
        if let Some(hz) = l.restore_from_hz {
            dsp.push(format!("highs restored above {:.1} kHz", hz / 1000.0));
        }
    }
    if !dsp.is_empty() {
        println!("DSP:     {}", dsp.join(", "));
    }
    println!("Path:    {}", if p.bit_perfect { "BIT-PERFECT ✓" } else { "not bit-perfect" });

}

fn mmss(secs: f64) -> String {
    let s = secs as u64;
    format!("{}:{:02}", s / 60, s % 60)
}
