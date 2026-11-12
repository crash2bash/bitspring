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
use hifi_engine::dsp::{DspSettings, Upsample};
use hifi_engine::dsp::lossy::LossySettings;
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
        Cmd::Play { files, device, shared, upsample, eq, crossfeed, headroom, restore_highs, dop } => {
            let dsp = DspSettings {
                upsample: upsample.map_or(Upsample::Off, |k| Upsample::UpTo(k * 1000.0)),
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
    let open = |file: &PathBuf| -> Result<Arc<Track>> {
        print!("Opening {}… ", file.display());
        std::io::stdout().flush()?;
        let t0 = Instant::now();
        let track = open_file(file, dsd_mode)?;
        println!("{:.2?}", t0.elapsed());
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
            if ahead.is_none() && i + 1 < files.len() && dur - pos < 20.0 {
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
            println!("Peak:    {:.1} dBFS, underruns: {}", 20.0 * peak.max(1e-9).log10(), player.underruns());
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
        dsp.push(format!("resample {} → {} Hz", p.source_rate, p.device_rate));
    }
    if let Some(name) = &p.eq {
        dsp.push(format!("EQ {name}"));
    }
    if let Some(cf) = p.crossfeed {
        dsp.push(format!("crossfeed {} Hz / {} dB", cf.cutoff_hz, cf.feed_db));
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
