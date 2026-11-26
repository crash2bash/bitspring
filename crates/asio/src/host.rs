// SPDX-License-Identifier: GPL-3.0-only
//
// The Windows side of bitspring-asio: driver discovery, the host thread and
// the realtime callbacks. Uses the ASIO interface from the Steinberg ASIO SDK
// 2.3.4 under its GPLv3 licence option. ASIO is a registered trademark of
// Steinberg Media Technologies GmbH.

//! Driver discovery, the host thread and the realtime callbacks.
//!
//! # Threading
//!
//! [`Host::new`] starts one thread, "bitspring-asio", that lives as long as
//! the `Host`. It joins a single-threaded COM apartment, creates a hidden
//! message-only window (its handle is what drivers get in `init`, as the
//! application window), and then loops: it runs the commands the `Host`
//! sends it, pumps the window messages drivers rely on, and sleeps in
//! `MsgWaitForMultipleObjectsEx` until either a new command (signalled by an
//! event) or a window message arrives. Every IASIO call except the audio
//! callback's `outputReady` happens on this thread, so drivers that need
//! their creating thread, an STA, or a message pump get all three.
//!
//! # The audio callback
//!
//! The driver calls plain C functions with no user pointer, so the running
//! stream is found through a process-global [`AtomicPtr`] (ASIO allows only
//! one driver per process anyway). The stream holds the buffer addresses of
//! both halves of every channel, the byte length of each, the render closure
//! and a few flags. The callback does not allocate, lock or send: it builds
//! the slices for the render closure in a fixed array on its own stack.

use std::cell::UnsafeCell;
use std::ffi::c_void;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr::{self, null_mut};
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU64, AtomicUsize, Ordering::SeqCst};
use std::sync::mpsc::{self, TryRecvError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{CloseHandle, ERROR_NO_MORE_ITEMS, ERROR_SUCCESS, HANDLE, HWND};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoFreeUnusedLibrariesEx, CoInitializeEx,
    CoUninitialize,
};
use windows::Win32::System::Registry::{
    HKEY, HKEY_LOCAL_MACHINE, KEY_READ, RRF_RT_REG_SZ, RegCloseKey, RegEnumKeyExW, RegGetValueW,
    RegOpenKeyExW,
};
use windows::Win32::System::Threading::{CreateEventW, SetEvent};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DestroyWindow, DispatchMessageW, HWND_MESSAGE, MSG, MWMO_INPUTAVAILABLE,
    MsgWaitForMultipleObjectsEx, PM_REMOVE, PeekMessageW, QS_ALLINPUT, TranslateMessage,
    WINDOW_EX_STYLE, WINDOW_STYLE,
};
use windows::core::{HRESULT, PCWSTR, PWSTR, w};

use crate::sys::*;
use crate::{
    AsioError, BufferSizes, Caps, ChannelInfo, DSD_RATES, DriverEntry, DriverInfo, Error, Events,
    MAX_CHANNELS, MessageEffect, PCM_RATES, RenderFn, SampleType, StreamConfig, StreamInfo,
    answer_message, c_string, choose_buffer_size, future_succeeded, is_dsd_rate, parse_clsid,
    ranges_disjoint, rates_equal,
};

// ---------------------------------------------------------------------------
// Discovery
// ---------------------------------------------------------------------------

/// Lists the installed ASIO drivers.
///
/// Drivers register under `HKEY_LOCAL_MACHINE\SOFTWARE\ASIO`, one subkey
/// each, with a `CLSID` string value and usually a `Description` (the
/// display name; the subkey name is used when it is missing). A 64-bit
/// process sees the 64-bit drivers there. Entries without a valid CLSID are
/// skipped. The list is empty when no driver is installed.
pub fn drivers() -> Vec<DriverEntry> {
    let mut list = Vec::new();
    let mut root = HKEY::default();
    // SAFETY: valid key, NUL-terminated path, valid out pointer.
    if unsafe {
        RegOpenKeyExW(
            HKEY_LOCAL_MACHINE,
            w!("SOFTWARE\\ASIO"),
            None,
            KEY_READ,
            &mut root,
        )
    } != ERROR_SUCCESS
    {
        return list;
    }
    for index in 0.. {
        let mut name = [0u16; 256];
        let mut len = name.len() as u32;
        // SAFETY: `name` holds `len` UTF-16 units; the other outputs are unused.
        let rc = unsafe {
            RegEnumKeyExW(
                root,
                index,
                Some(PWSTR(name.as_mut_ptr())),
                &mut len,
                None,
                None,
                None,
                None,
            )
        };
        if rc == ERROR_NO_MORE_ITEMS {
            break;
        }
        if rc != ERROR_SUCCESS {
            // A name too long for the buffer, or a key removed while we
            // read: skip that one entry.
            continue;
        }
        let key = &name[..(len as usize).min(name.len())];
        let mut subkey = key.to_vec();
        subkey.push(0);
        let Some(clsid) = registry_string(root, &subkey, w!("CLSID")) else {
            continue;
        };
        if parse_clsid(&clsid).is_none() {
            continue;
        }
        let name = registry_string(root, &subkey, w!("Description"))
            .filter(|d| !d.is_empty())
            .unwrap_or_else(|| String::from_utf16_lossy(key).trim().to_owned());
        list.push(DriverEntry { name, clsid });
    }
    // SAFETY: `root` was opened above.
    unsafe {
        let _ = RegCloseKey(root);
    }
    list
}

/// Reads a `REG_SZ` value of `root\subkey` (`subkey` NUL-terminated UTF-16),
/// trimmed. `None` when it is missing, empty or of another type.
fn registry_string(root: HKEY, subkey: &[u16], value: PCWSTR) -> Option<String> {
    let mut size = 0u32;
    // SAFETY: NUL-terminated strings; asking for the size only.
    let rc = unsafe {
        RegGetValueW(
            root,
            PCWSTR(subkey.as_ptr()),
            value,
            RRF_RT_REG_SZ,
            None,
            None,
            Some(&mut size),
        )
    };
    if rc != ERROR_SUCCESS || size == 0 {
        return None;
    }
    let mut buf = vec![0u16; (size as usize).div_ceil(2)];
    let mut size = (buf.len() * 2) as u32;
    // SAFETY: `buf` holds `size` bytes.
    let rc = unsafe {
        RegGetValueW(
            root,
            PCWSTR(subkey.as_ptr()),
            value,
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr().cast()),
            Some(&mut size),
        )
    };
    if rc != ERROR_SUCCESS {
        return None;
    }
    buf.truncate((size as usize / 2).min(buf.len()));
    while buf.last() == Some(&0) {
        buf.pop();
    }
    let text = String::from_utf16_lossy(&buf).trim().to_owned();
    (!text.is_empty()).then_some(text)
}

// ---------------------------------------------------------------------------
// Process-global state shared with the driver's callbacks
// ---------------------------------------------------------------------------

/// Whether some `Host` in this process has a driver loaded. ASIO allows
/// one driver per process: the callbacks have no user pointer, and the
/// SDK's own host code keeps a single global driver too.
static LOADED: AtomicBool = AtomicBool::new(false);

/// The running stream, or null. Set just before `start()`, cleared during
/// stop; the callbacks tolerate null.
static STREAM: AtomicPtr<Stream> = AtomicPtr::new(null_mut());

/// How many buffer-switch callbacks are inside [`serve`] right now. Stop
/// clears [`STREAM`] and then waits for this to reach zero before freeing
/// the stream; see [`unpublish`].
static IN_CALLBACK: AtomicUsize = AtomicUsize::new(0);

/// The rate (as `f64` bits) the running stream was set to, 0 without one.
/// `sampleRateDidChange` compares against it.
static EXPECTED_RATE: AtomicU64 = AtomicU64::new(0);

/// What the driver reported, until [`Host::take_events`] collects it.
static EVENTS: EventFlags = EventFlags {
    reset: AtomicBool::new(false),
    rate_changed: AtomicBool::new(false),
    new_rate: AtomicU64::new(0),
    dropouts: AtomicU64::new(0),
    latencies_changed: AtomicBool::new(false),
    render_panicked: AtomicBool::new(false),
};

/// The lock-free form of [`Events`] the callbacks write into.
struct EventFlags {
    reset: AtomicBool,
    rate_changed: AtomicBool,
    new_rate: AtomicU64,
    dropouts: AtomicU64,
    latencies_changed: AtomicBool,
    render_panicked: AtomicBool,
}

impl EventFlags {
    /// Reads and resets everything.
    fn take(&self) -> Events {
        let sample_rate_changed = self
            .rate_changed
            .swap(false, SeqCst)
            .then(|| f64::from_bits(self.new_rate.load(SeqCst)));
        Events {
            reset_requested: self.reset.swap(false, SeqCst),
            sample_rate_changed,
            dropouts: self.dropouts.swap(0, SeqCst),
            latencies_changed: self.latencies_changed.swap(false, SeqCst),
            render_panicked: self.render_panicked.swap(false, SeqCst),
        }
    }

    /// Forgets the reset and rate change notices. Used after the host's own
    /// reconfiguration (switching between PCM and DSD, setting a rate),
    /// which makes many drivers ask for a reset that only echoes the change
    /// just made; passing it on would make the owner restart in a loop.
    fn forget_own_changes(&self) {
        self.reset.store(false, SeqCst);
        self.rate_changed.store(false, SeqCst);
    }
}

// ---------------------------------------------------------------------------
// The host handle
// ---------------------------------------------------------------------------

/// A command for the host thread.
type Job = Box<dyn FnOnce(&mut Session) + Send>;

/// The event that wakes the host thread when a command is queued.
#[derive(Clone, Copy)]
struct Wake(HANDLE);

// SAFETY: an event handle may be signalled and closed from any thread.
unsafe impl Send for Wake {}
// SAFETY: as above; `SetEvent` is thread-safe.
unsafe impl Sync for Wake {}

/// An ASIO host: one dedicated thread on which at most one driver is
/// loaded and every driver call is made.
///
/// The handle is `Send` and `Sync`; each method sends a command to the host
/// thread and waits for its answer. Only one driver can be loaded in a
/// process at a time, across all `Host`s. Dropping the `Host` stops any
/// stream, closes the driver and ends the thread.
///
/// Never call a `Host` method from inside the render closure: the host may
/// be waiting for that very callback to return.
pub struct Host {
    jobs: Option<mpsc::Sender<Job>>,
    wake: Wake,
    thread: Option<JoinHandle<()>>,
}

const _: () = {
    const fn send_and_sync<T: Send + Sync>() {}
    send_and_sync::<Host>();
};

impl Host {
    /// Starts the host thread (STA, hidden window, message loop). No driver
    /// is loaded yet; see [`Host::open`].
    pub fn new() -> Result<Host, Error> {
        // SAFETY: plain call; an auto-reset event, initially unsignalled.
        let event =
            unsafe { CreateEventW(None, false, false, PCWSTR::null()) }.map_err(|e| Error::Os {
                call: "CreateEventW",
                code: e.code().0,
            })?;
        let wake = Wake(event);
        let (jobs, inbox) = mpsc::channel::<Job>();
        let (ready_tx, ready_rx) = mpsc::channel();
        let spawned = thread::Builder::new()
            .name("bitspring-asio".into())
            .spawn(move || host_thread(inbox, wake, ready_tx));
        let close_event = || {
            // SAFETY: the event is ours and no longer used.
            unsafe {
                let _ = CloseHandle(event);
            }
        };
        let thread = match spawned {
            Ok(thread) => thread,
            Err(_) => {
                close_event();
                return Err(Error::HostGone);
            }
        };
        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Host {
                jobs: Some(jobs),
                wake,
                thread: Some(thread),
            }),
            failed => {
                let _ = thread.join();
                close_event();
                Err(match failed {
                    Ok(Err(e)) => e,
                    _ => Error::HostGone,
                })
            }
        }
    }

    /// Runs `job` on the host thread and returns its result.
    fn call<R: Send + 'static>(
        &self,
        job: impl FnOnce(&mut Session) -> R + Send + 'static,
    ) -> Result<R, Error> {
        let (tx, rx) = mpsc::sync_channel(1);
        let jobs = self.jobs.as_ref().ok_or(Error::HostGone)?;
        jobs.send(Box::new(move |session: &mut Session| {
            let _ = tx.send(job(session));
        }))
        .map_err(|_| Error::HostGone)?;
        // SAFETY: the event lives until `Drop` has joined the thread.
        unsafe {
            let _ = SetEvent(self.wake.0);
        }
        rx.recv().map_err(|_| Error::HostGone)
    }

    /// Loads and initialises the driver with this CLSID (as in
    /// [`DriverEntry::clsid`]). Fails with [`Error::AlreadyLoaded`] while
    /// any driver is loaded in this process (close it first),
    /// [`Error::CreateFailed`] if COM cannot create it, and
    /// [`Error::InitFailed`] with the driver's own message if its `init`
    /// refuses (no device connected, device in use by another program...).
    pub fn open(&self, clsid: &str) -> Result<(), Error> {
        let clsid = clsid.to_owned();
        self.call(move |s| s.open(&clsid))?
    }

    /// Stops any stream, frees the buffers and releases the driver, which
    /// lets Windows unload its DLL. Does nothing when no driver is open.
    pub fn close(&self) -> Result<(), Error> {
        self.call(|s| s.close())
    }

    /// Describes the open driver in its current mode (PCM or DSD).
    pub fn info(&self) -> Result<DriverInfo, Error> {
        self.call(|s| s.info())?
    }

    /// Whether the driver accepts this sample rate in its current mode.
    pub fn can_sample_rate(&self, rate: f64) -> Result<bool, Error> {
        self.call(move |s| s.loaded().map(|d| d.asio.can_sample_rate(rate)))?
    }

    /// Whether the driver has a native DSD mode (it answers the IoFormat
    /// `future` call for DSD).
    pub fn can_dsd(&self) -> Result<bool, Error> {
        self.call(|s| {
            s.loaded()
                .map(|d| d.asio.io_format(K_ASIO_CAN_DO_IO_FORMAT, K_ASIO_DSD_FORMAT))
        })?
    }

    /// Finds out which PCM rates of [`PCM_RATES`] and DSD rates of
    /// [`DSD_RATES`] the driver accepts, and the channel sample types in
    /// each mode. Switches the driver to DSD and back to PCM to do so, then
    /// restores the previous PCM rate. DSD counts as available only if the
    /// channels really report a DSD sample type in DSD mode. Not while a
    /// stream runs.
    pub fn probe(&self) -> Result<Caps, Error> {
        self.call(|s| s.probe())?
    }

    /// Prepares buffers and starts a stream.
    ///
    /// The order follows the SDK and working DSD hosts: switch the IoFormat
    /// (to DSD, or back to PCM when the driver has the call, since an
    /// earlier DSD session may have left it in DSD), check the channels,
    /// set the rate and wait up to a second for the driver to report it,
    /// choose the buffer size, read the channel sample type (which must be the
    /// same on every stream channel, and a 1-bit DSD type in DSD mode), then
    /// create the buffers, fill both halves with silence and start.
    pub fn start(&self, config: StreamConfig) -> Result<StreamInfo, Error> {
        self.call(move |s| s.start(config))?
    }

    /// Stops the stream and frees its buffers; the render closure is
    /// dropped once the driver can no longer call it. Does nothing when no
    /// stream runs. If the driver's `stop` reports an error the stream is
    /// torn down anyway and the error returned.
    pub fn stop(&self) -> Result<(), Error> {
        self.call(|s| match s.driver.as_mut() {
            Some(d) => d.stop_stream(),
            None => Ok(()),
        })?
    }

    /// Opens the driver's own settings window. Many drivers ask for a reset
    /// afterwards (see [`Events::reset_requested`]).
    pub fn control_panel(&self) -> Result<(), Error> {
        self.call(|s| {
            s.loaded()
                .and_then(|d| check("controlPanel", d.asio.control_panel()))
        })?
    }

    /// Returns what the driver reported since the last call and clears it.
    /// Cheap and lock-free; poll it from the owner's own loop.
    pub fn take_events(&self) -> Events {
        EVENTS.take()
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        // Closing the channel tells the thread to clean up and leave.
        drop(self.jobs.take());
        // SAFETY: the event is still open; it is closed below, after join.
        unsafe {
            let _ = SetEvent(self.wake.0);
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        // SAFETY: no one uses the event any more.
        unsafe {
            let _ = CloseHandle(self.wake.0);
        }
    }
}

// ---------------------------------------------------------------------------
// The host thread
// ---------------------------------------------------------------------------

/// The body of the "bitspring-asio" thread.
fn host_thread(inbox: mpsc::Receiver<Job>, wake: Wake, ready: mpsc::Sender<Result<(), Error>>) {
    // SAFETY: plain call on this new thread.
    let hr = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
    if hr.is_err() {
        let _ = ready.send(Err(Error::Os {
            call: "CoInitializeEx",
            code: hr.0,
        }));
        return;
    }
    // A message-only window of the predefined STATIC class: nothing to
    // register, never visible, but a real window handle with a queue on
    // this thread, which is what drivers expect as the "application window".
    // SAFETY: constant class and title strings; HWND_MESSAGE as parent.
    let hwnd = match unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            w!("STATIC"),
            w!("BitSpring ASIO"),
            WINDOW_STYLE::default(),
            0,
            0,
            0,
            0,
            Some(HWND_MESSAGE),
            None,
            None,
            None,
        )
    } {
        Ok(hwnd) => hwnd,
        Err(e) => {
            let _ = ready.send(Err(Error::Os {
                call: "CreateWindowExW",
                code: e.code().0,
            }));
            // SAFETY: balances the successful CoInitializeEx above.
            unsafe { CoUninitialize() };
            return;
        }
    };
    let mut session = Session { hwnd, driver: None };
    let _ = ready.send(Ok(()));

    'run: loop {
        pump_messages();
        loop {
            match inbox.try_recv() {
                Ok(job) => {
                    job(&mut session);
                    pump_messages();
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => break 'run,
            }
        }
        // Sleep until a command is queued (the event) or a window message
        // arrives; MWMO_INPUTAVAILABLE also wakes for messages already
        // queued. The timeout is only a safety net.
        // SAFETY: the event outlives this thread (closed after join).
        unsafe {
            MsgWaitForMultipleObjectsEx(Some(&[wake.0]), 250, QS_ALLINPUT, MWMO_INPUTAVAILABLE);
        }
    }

    drop(session); // closes the driver
    // SAFETY: our window; balances CoInitializeEx.
    unsafe {
        let _ = DestroyWindow(hwnd);
        CoUninitialize();
    }
}

/// Dispatches every queued window message of this thread.
fn pump_messages() {
    let mut msg = MSG::default();
    // SAFETY: `msg` is a valid MSG; all windows of this thread.
    while unsafe { PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE) }.as_bool() {
        // SAFETY: a message just taken from the queue.
        unsafe {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

/// The host thread's state.
struct Session {
    hwnd: HWND,
    driver: Option<Loaded>,
}

/// An open driver.
struct Loaded {
    asio: Asio,
    /// The running stream (owned, from `Box::into_raw`), or null.
    stream: *mut Stream,
    /// Whether this host switched the driver to DSD; it is switched back
    /// to PCM on close, as a courtesy to the next program using it.
    dsd_mode: bool,
}

impl Drop for Session {
    fn drop(&mut self) {
        self.close();
    }
}

impl Session {
    fn loaded(&self) -> Result<&Loaded, Error> {
        self.driver.as_ref().ok_or(Error::NotOpen)
    }

    fn open(&mut self, text: &str) -> Result<(), Error> {
        let clsid = parse_clsid(text).ok_or_else(|| Error::BadClsid(text.to_owned()))?;
        if self.driver.is_some()
            || LOADED
                .compare_exchange(false, true, SeqCst, SeqCst)
                .is_err()
        {
            return Err(Error::AlreadyLoaded);
        }
        let asio = match Asio::create(&clsid) {
            Ok(asio) => asio,
            Err(e) => {
                LOADED.store(false, SeqCst);
                return Err(e);
            }
        };
        if !asio.init(self.hwnd) {
            let message = asio.error_message();
            asio.release();
            unload_unused_libraries();
            LOADED.store(false, SeqCst);
            return Err(Error::InitFailed(message));
        }
        EVENTS.take();
        self.driver = Some(Loaded {
            asio,
            stream: null_mut(),
            dsd_mode: false,
        });
        Ok(())
    }

    fn close(&mut self) {
        let Some(mut d) = self.driver.take() else {
            return;
        };
        let _ = d.stop_stream();
        if d.dsd_mode {
            d.asio.io_format(K_ASIO_SET_IO_FORMAT, K_ASIO_PCM_FORMAT);
        }
        d.asio.release();
        unload_unused_libraries();
        LOADED.store(false, SeqCst);
        EVENTS.take();
    }

    fn info(&self) -> Result<DriverInfo, Error> {
        let asio = &self.loaded()?.asio;
        let (inputs, outputs) = asio.channels()?;
        let outs = (0..outputs)
            .map(|ch| asio.channel_info(ch))
            .collect::<Result<Vec<_>, _>>()?;
        let latencies = asio.latencies().ok();
        Ok(DriverInfo {
            name: asio.driver_name(),
            version: asio.driver_version(),
            input_channels: inputs,
            output_channels: outputs,
            outputs: outs,
            buffer_sizes: asio.buffer_sizes()?,
            input_latency: latencies.map(|l| l.0),
            output_latency: latencies.map(|l| l.1),
            sample_rate: asio.sample_rate().ok(),
        })
    }

    fn probe(&mut self) -> Result<Caps, Error> {
        let d = self.driver.as_mut().ok_or(Error::NotOpen)?;
        if !d.stream.is_null() {
            return Err(Error::Running);
        }
        let asio = &d.asio;
        let previous = asio.sample_rate().ok();
        if asio.io_format(K_ASIO_CAN_DO_IO_FORMAT, K_ASIO_PCM_FORMAT) {
            asio.io_format(K_ASIO_SET_IO_FORMAT, K_ASIO_PCM_FORMAT);
        }
        let (_, outputs) = asio.channels()?;
        let pcm_rates = PCM_RATES
            .iter()
            .copied()
            .filter(|&r| asio.can_sample_rate(r as f64))
            .collect();
        let pcm_types = channel_types(asio, outputs)?;

        let mut dsd_rates = Vec::new();
        let mut dsd_types = Vec::new();
        if asio.io_format(K_ASIO_CAN_DO_IO_FORMAT, K_ASIO_DSD_FORMAT)
            && asio.io_format(K_ASIO_SET_IO_FORMAT, K_ASIO_DSD_FORMAT)
        {
            let rates: Vec<u32> = DSD_RATES
                .iter()
                .copied()
                .filter(|&r| asio.can_sample_rate(r as f64))
                .collect();
            // The channel types only turn into DSD types once a DSD rate is
            // set, so set the lowest one before reading them.
            if let Some(&lowest) = rates.first()
                && set_rate(asio, lowest as f64).is_ok()
            {
                let outputs = asio.channels().map(|c| c.1).unwrap_or(outputs);
                let types = channel_types(asio, outputs).unwrap_or_default();
                if !types.is_empty() && types.iter().all(|t| t.is_dsd()) {
                    dsd_rates = rates;
                    dsd_types = types;
                }
            }
            asio.io_format(K_ASIO_SET_IO_FORMAT, K_ASIO_PCM_FORMAT);
        }
        d.dsd_mode = false;
        if let Some(rate) = previous.filter(|&r| r > 0.0 && !is_dsd_rate(r)) {
            let _ = set_rate(asio, rate);
        }
        EVENTS.forget_own_changes();
        Ok(Caps {
            pcm_rates,
            dsd_rates,
            output_channels: outputs,
            pcm_types,
            dsd_types,
        })
    }

    fn start(&mut self, config: StreamConfig) -> Result<StreamInfo, Error> {
        let d = self.driver.as_mut().ok_or(Error::NotOpen)?;
        if !d.stream.is_null() {
            return Err(Error::Running);
        }
        let StreamConfig {
            dsd,
            rate,
            channels,
            buffer_size,
            render,
        } = config;
        if channels.is_empty() {
            return Err(Error::InvalidConfig("no output channels".into()));
        }
        if channels.len() > MAX_CHANNELS {
            return Err(Error::InvalidConfig(format!(
                "more than {MAX_CHANNELS} channels"
            )));
        }
        if !(rate.is_finite() && rate > 0.0) {
            return Err(Error::InvalidConfig(format!("sample rate {rate}")));
        }
        let asio = &d.asio;

        // 1. PCM or DSD mode. Must happen before buffers exist.
        if dsd {
            if !asio.io_format(K_ASIO_CAN_DO_IO_FORMAT, K_ASIO_DSD_FORMAT) {
                return Err(Error::Unsupported(
                    "the driver has no native DSD mode".into(),
                ));
            }
            if !asio.io_format(K_ASIO_SET_IO_FORMAT, K_ASIO_DSD_FORMAT) {
                return Err(Error::Unsupported(
                    "the driver refused to switch to DSD".into(),
                ));
            }
            d.dsd_mode = true;
        } else {
            if asio.io_format(K_ASIO_CAN_DO_IO_FORMAT, K_ASIO_PCM_FORMAT) {
                // Errors ignored: drivers without DSD may not know the call.
                asio.io_format(K_ASIO_SET_IO_FORMAT, K_ASIO_PCM_FORMAT);
            }
            d.dsd_mode = false;
        }

        // 2. Channels.
        let (_, outputs) = asio.channels()?;
        for (i, &ch) in channels.iter().enumerate() {
            if ch < 0 || ch >= outputs {
                return Err(Error::InvalidConfig(format!(
                    "output channel {ch} does not exist (the driver has {outputs})"
                )));
            }
            if channels[..i].contains(&ch) {
                return Err(Error::InvalidConfig(format!(
                    "output channel {ch} is listed twice"
                )));
            }
        }

        // 3. Rate, waiting until the driver reports it.
        let actual = set_rate(asio, rate)?;

        // 4. Buffer size (1-bit samples in DSD mode). Asked after the rate,
        // since some drivers offer different sizes at different rates.
        let sizes = asio.buffer_sizes()?;
        let size = choose_buffer_size(
            sizes.min,
            sizes.max,
            buffer_size.unwrap_or(sizes.preferred),
            sizes.granularity,
            dsd,
        )
        .ok_or_else(|| {
            Error::Unsupported(format!(
                "no usable buffer size (the driver allows {} to {}, step {})",
                sizes.min, sizes.max, sizes.granularity
            ))
        })?;

        // 5. Sample type, now that mode and rate are set.
        let mut sample_type = None;
        for &ch in &channels {
            let t = asio.channel_info(ch)?.sample_type;
            match sample_type {
                None => sample_type = Some(t),
                Some(s) if s != t => {
                    return Err(Error::Unsupported(
                        "the channels use different sample formats".into(),
                    ));
                }
                Some(_) => {}
            }
        }
        let sample_type =
            sample_type.ok_or_else(|| Error::InvalidConfig("no output channels".into()))?;
        match (dsd, sample_type) {
            (true, SampleType::DsdInt8Lsb1 | SampleType::DsdInt8Msb1) => {}
            (true, t) => {
                return Err(Error::Unsupported(format!(
                    "in DSD mode the driver gives samples of type {t:?}"
                )));
            }
            (false, t) if t.bytes_per_sample().is_some() => {}
            (false, t) => {
                return Err(Error::Unsupported(format!(
                    "in PCM mode the driver gives samples of type {t:?}"
                )));
            }
        }
        let bytes = sample_type
            .bytes_per_buffer(size)
            .ok_or_else(|| Error::Unsupported(format!("buffer size {size} for {sample_type:?}")))?;

        // 6. Buffers. The stream (with the callback table the driver keeps a
        // pointer to) lives on the heap until after disposeBuffers.
        let stream = Box::into_raw(Box::new(Stream {
            callbacks: CALLBACKS,
            asio: asio.0,
            channels: channels.len(),
            bytes,
            silence: sample_type.silence_byte(),
            buffers: [[null_mut(); 2]; MAX_CHANNELS],
            render: UnsafeCell::new(render),
            output_ready: false,
            muted: AtomicBool::new(false),
            failed: AtomicBool::new(false),
        }));
        let mut infos: Vec<AsioBufferInfo> = channels
            .iter()
            .map(|&ch| AsioBufferInfo {
                isInput: 0,
                channelNum: ch,
                buffers: [null_mut(); 2],
            })
            .collect();
        // SAFETY: `infos` has one entry per channel; the callback table stays
        // valid until the stream is freed, after disposeBuffers.
        let code = unsafe { asio.create_buffers(&mut infos, size, &raw mut (*stream).callbacks) };
        if code != ASE_OK {
            // SAFETY: never published; the driver has no buffers for it.
            drop(unsafe { Box::from_raw(stream) });
            return Err(Error::Asio {
                call: "createBuffers",
                error: AsioError::from_raw(code),
            });
        }
        let abort = move |error: Error| {
            let _ = asio.dispose_buffers();
            // SAFETY: not published, buffers disposed: nothing refers to it.
            drop(unsafe { Box::from_raw(stream) });
            Err(error)
        };
        {
            // SAFETY: not published yet, so this is the only reference.
            let s = unsafe { &mut *stream };
            for (slot, info) in s.buffers.iter_mut().zip(&infos) {
                let halves = info.buffers;
                *slot = [halves[0].cast(), halves[1].cast()];
            }
            let ranges: Vec<(usize, usize)> = s.buffers[..s.channels]
                .iter()
                .flatten()
                .map(|&p| (p as usize, (p as usize).saturating_add(bytes)))
                .collect();
            if ranges.iter().any(|r| r.0 == 0) || !ranges_disjoint(&ranges) {
                return abort(Error::Unsupported(
                    "the driver returned invalid buffers".into(),
                ));
            }
            s.fill_silence(0);
            s.fill_silence(1);
            // As in the SDK's sample host: ask once; if the driver supports
            // outputReady, call it after every buffer from now on.
            s.output_ready = asio.output_ready() == ASE_OK;
        }
        let output_latency = asio.latencies().ok().map(|l| l.1);

        // 7. Publish and start.
        EXPECTED_RATE.store(actual.to_bits(), SeqCst);
        EVENTS.forget_own_changes();
        STREAM.store(stream, SeqCst);
        let code = asio.start();
        if code != ASE_OK {
            EXPECTED_RATE.store(0, SeqCst);
            if !unpublish() {
                // A callback is stuck inside the stream: leak it rather than
                // free memory in use.
                let _ = asio.dispose_buffers();
                return Err(Error::Asio {
                    call: "start",
                    error: AsioError::from_raw(code),
                });
            }
            return abort(Error::Asio {
                call: "start",
                error: AsioError::from_raw(code),
            });
        }
        d.stream = stream;
        Ok(StreamInfo {
            buffer_size: size,
            bytes_per_channel: bytes,
            sample_type,
            rate: actual,
            output_latency,
        })
    }
}

impl Loaded {
    /// Stops and tears down the running stream, if any.
    ///
    /// The order makes it safe to free the render closure: first the stream
    /// is muted (callbacks write silence without calling it), then the
    /// driver is stopped (after which the SDK promises no more buffer
    /// switches), then the global pointer is cleared and any callback still
    /// inside is waited for, then the buffers are disposed and the stream
    /// freed. A driver that breaks the promise finds a null pointer.
    fn stop_stream(&mut self) -> Result<(), Error> {
        let stream = std::mem::replace(&mut self.stream, null_mut());
        if stream.is_null() {
            return Ok(());
        }
        // SAFETY: owned by us and alive; `muted` is atomic.
        unsafe { (*stream).muted.store(true, SeqCst) };
        let code = self.asio.stop();
        let quiet = unpublish();
        EXPECTED_RATE.store(0, SeqCst);
        let _ = self.asio.dispose_buffers();
        if quiet {
            // SAFETY: unpublished and no callback inside it any more.
            drop(unsafe { Box::from_raw(stream) });
        }
        // Otherwise a callback never returned: the stream is leaked on
        // purpose, since freeing it under that callback would be worse.
        check("stop", code)
    }
}

/// Clears [`STREAM`] and waits (up to 2 s) until no callback is inside
/// [`serve`]. Returns false if one is still inside after that.
///
/// Why this is enough: a callback first increments [`IN_CALLBACK`] and only
/// then loads [`STREAM`]; here the pointer is cleared first and the counter
/// read after. All four operations are sequentially consistent, so either
/// the callback's increment comes first (and this loop waits for it), or
/// the clearing comes first (and the callback reads null).
fn unpublish() -> bool {
    STREAM.store(null_mut(), SeqCst);
    let deadline = Instant::now() + Duration::from_secs(2);
    while IN_CALLBACK.load(SeqCst) != 0 {
        if Instant::now() >= deadline {
            return false;
        }
        thread::yield_now();
    }
    true
}

/// Sets the rate and waits up to a second for the driver to report it (some
/// drivers switch the clock asynchronously), pumping messages meanwhile.
/// Returns the rate the driver reports. Skips the call when the driver
/// already runs at that rate. A driver whose `getSampleRate` never answers
/// is trusted.
fn set_rate(asio: &Asio, rate: f64) -> Result<f64, Error> {
    if let Ok(now) = asio.sample_rate()
        && rates_equal(now, rate)
    {
        return Ok(now);
    }
    asio.set_sample_rate(rate)?;
    let deadline = Instant::now() + Duration::from_secs(1);
    let mut last = None;
    loop {
        if let Ok(now) = asio.sample_rate() {
            if rates_equal(now, rate) {
                return Ok(now);
            }
            last = Some(now);
        }
        if Instant::now() >= deadline {
            return match last {
                Some(got) => Err(Error::RateNotApplied { wanted: rate, got }),
                None => Ok(rate),
            };
        }
        pump_messages();
        thread::sleep(Duration::from_millis(10));
    }
}

/// The sample type of each of the first `outputs` output channels.
fn channel_types(asio: &Asio, outputs: i32) -> Result<Vec<SampleType>, Error> {
    (0..outputs)
        .map(|ch| asio.channel_info(ch).map(|c| c.sample_type))
        .collect()
}

/// Asks COM to unload driver DLLs nobody uses any more, right away.
fn unload_unused_libraries() {
    // SAFETY: plain call on a COM thread.
    unsafe { CoFreeUnusedLibrariesEx(0, None) };
}

/// Turns an ASIO return code into a `Result`.
fn check(call: &'static str, code: AsioErrorCode) -> Result<(), Error> {
    if code == ASE_OK {
        Ok(())
    } else {
        Err(Error::Asio {
            call,
            error: AsioError::from_raw(code),
        })
    }
}

// ---------------------------------------------------------------------------
// The IASIO object
// ---------------------------------------------------------------------------

/// A loaded driver object, called through its method table. Only used on
/// the host thread (and `outputReady` from the callback, through the raw
/// pointer kept in the stream).
struct Asio(*mut IAsio);

impl Asio {
    /// Creates the driver object.
    ///
    /// This does what the SDK's own host code does:
    /// `CoCreateInstance(clsid, NULL, CLSCTX_INPROC_SERVER, clsid, &p)`.
    /// The interface id asked for is the driver's CLSID itself: IASIO has
    /// no fixed IID, every driver answers to its own class id. The `windows`
    /// crate's `CoCreateInstance` wrapper insists on a typed interface with
    /// a compile-time IID, so the raw function is declared here with the
    /// same `link!` macro the `windows` crate uses internally (no import
    /// library needed), and returns the bare `IASIO*` pointer.
    fn create(clsid: &Guid) -> Result<Asio, Error> {
        windows::core::link!("ole32.dll" "system" fn CoCreateInstance(rclsid: *const Guid, outer: *mut c_void, context: u32, riid: *const Guid, out: *mut *mut c_void) -> HRESULT);
        let mut out = null_mut();
        // SAFETY: valid GUID pointers and out pointer; COM is initialised
        // on this thread.
        let hr =
            unsafe { CoCreateInstance(clsid, null_mut(), CLSCTX_INPROC_SERVER.0, clsid, &mut out) };
        if hr.is_err() || out.is_null() {
            return Err(Error::CreateFailed(hr.0));
        }
        Ok(Asio(out.cast()))
    }

    fn vt(&self) -> &IAsioVtbl {
        // SAFETY: a live COM object starts with its method table pointer.
        unsafe { &*(*self.0).vtbl }
    }

    fn init(&self, hwnd: HWND) -> bool {
        // SAFETY: live object; the window outlives the driver.
        unsafe { (self.vt().init)(self.0, hwnd.0) != 0 }
    }

    fn release(&self) {
        // SAFETY: drops our only reference; not used afterwards.
        unsafe { (self.vt().Release)(self.0) };
    }

    // Name and message buffers are far larger than the SDK's 32 and 124
    // bytes, in case a driver writes past them.
    fn driver_name(&self) -> String {
        let mut buf = [0u8; 256];
        // SAFETY: live object, writable buffer.
        unsafe { (self.vt().getDriverName)(self.0, buf.as_mut_ptr()) };
        c_string(&buf)
    }

    fn driver_version(&self) -> i32 {
        // SAFETY: live object.
        unsafe { (self.vt().getDriverVersion)(self.0) }
    }

    fn error_message(&self) -> String {
        let mut buf = [0u8; 256];
        // SAFETY: live object, writable buffer.
        unsafe { (self.vt().getErrorMessage)(self.0, buf.as_mut_ptr()) };
        c_string(&buf)
    }

    fn start(&self) -> AsioErrorCode {
        // SAFETY: live object.
        unsafe { (self.vt().start)(self.0) }
    }

    fn stop(&self) -> AsioErrorCode {
        // SAFETY: live object.
        unsafe { (self.vt().stop)(self.0) }
    }

    fn dispose_buffers(&self) -> AsioErrorCode {
        // SAFETY: live object.
        unsafe { (self.vt().disposeBuffers)(self.0) }
    }

    fn control_panel(&self) -> AsioErrorCode {
        // SAFETY: live object.
        unsafe { (self.vt().controlPanel)(self.0) }
    }

    fn output_ready(&self) -> AsioErrorCode {
        // SAFETY: live object.
        unsafe { (self.vt().outputReady)(self.0) }
    }

    fn channels(&self) -> Result<(i32, i32), Error> {
        let (mut inputs, mut outputs) = (0, 0);
        // SAFETY: live object, valid out pointers.
        check("getChannels", unsafe {
            (self.vt().getChannels)(self.0, &mut inputs, &mut outputs)
        })?;
        Ok((inputs, outputs))
    }

    fn latencies(&self) -> Result<(i32, i32), Error> {
        let (mut input, mut output) = (0, 0);
        // SAFETY: live object, valid out pointers.
        check("getLatencies", unsafe {
            (self.vt().getLatencies)(self.0, &mut input, &mut output)
        })?;
        Ok((input, output))
    }

    fn buffer_sizes(&self) -> Result<BufferSizes, Error> {
        let (mut min, mut max, mut preferred, mut granularity) = (0, 0, 0, 0);
        // SAFETY: live object, valid out pointers.
        check("getBufferSize", unsafe {
            (self.vt().getBufferSize)(self.0, &mut min, &mut max, &mut preferred, &mut granularity)
        })?;
        Ok(BufferSizes {
            min,
            max,
            preferred,
            granularity,
        })
    }

    fn can_sample_rate(&self, rate: f64) -> bool {
        // SAFETY: live object.
        unsafe { (self.vt().canSampleRate)(self.0, rate) == ASE_OK }
    }

    fn sample_rate(&self) -> Result<f64, Error> {
        let mut rate = 0.0;
        // SAFETY: live object, valid out pointer.
        check("getSampleRate", unsafe {
            (self.vt().getSampleRate)(self.0, &mut rate)
        })?;
        Ok(rate)
    }

    fn set_sample_rate(&self, rate: f64) -> Result<(), Error> {
        // SAFETY: live object.
        check("setSampleRate", unsafe {
            (self.vt().setSampleRate)(self.0, rate)
        })
    }

    /// Describes output channel `channel`.
    fn channel_info(&self, channel: i32) -> Result<ChannelInfo, Error> {
        let mut info = AsioChannelInfo {
            channel,
            isInput: 0,
            isActive: 0,
            channelGroup: 0,
            type_: 0,
            name: [0; 32],
        };
        // SAFETY: live object, valid in/out struct.
        check("getChannelInfo", unsafe {
            (self.vt().getChannelInfo)(self.0, &mut info)
        })?;
        let name = info.name;
        Ok(ChannelInfo {
            channel,
            name: c_string(&name),
            group: info.channelGroup,
            sample_type: SampleType::from_raw(info.type_),
        })
    }

    /// One IoFormat `future` call (`K_ASIO_CAN_DO_IO_FORMAT` or
    /// `K_ASIO_SET_IO_FORMAT`) for `format`. True when the driver reports
    /// success and did not mark the format invalid.
    fn io_format(&self, selector: i32, format: i32) -> bool {
        let mut block = AsioIoFormat::new(format);
        // SAFETY: live object; a 512-byte parameter block as the SDK defines.
        let code = unsafe { (self.vt().future)(self.0, selector, (&raw mut block).cast()) };
        let reported = block.FormatType;
        future_succeeded(code) && reported == format
    }

    /// `createBuffers` for output channels.
    ///
    /// # Safety
    /// `callbacks` must stay valid until `disposeBuffers` has returned.
    unsafe fn create_buffers(
        &self,
        infos: &mut [AsioBufferInfo],
        size: i32,
        callbacks: *mut AsioCallbacks,
    ) -> AsioErrorCode {
        // SAFETY: live object; `infos.len()` entries; caller keeps `callbacks` alive.
        unsafe {
            (self.vt().createBuffers)(
                self.0,
                infos.as_mut_ptr(),
                infos.len() as i32,
                size,
                callbacks,
            )
        }
    }
}

// ---------------------------------------------------------------------------
// The stream and the realtime callbacks
// ---------------------------------------------------------------------------

/// Everything the realtime callback needs, allocated once per stream.
struct Stream {
    /// The table registered with `createBuffers`; the driver keeps a
    /// pointer to it.
    callbacks: AsioCallbacks,
    /// The driver, for `outputReady`.
    asio: *mut IAsio,
    /// How many entries of `buffers` are in use.
    channels: usize,
    /// Bytes per buffer half per channel.
    bytes: usize,
    /// The byte that means silence in this sample format.
    silence: u8,
    /// Both halves of each channel's double buffer, from `createBuffers`.
    buffers: [[*mut u8; 2]; MAX_CHANNELS],
    /// The owner's closure. Only the callback calls it; the host thread only
    /// drops it, after [`unpublish`] made sure no callback runs.
    render: UnsafeCell<RenderFn>,
    /// Call `outputReady` after each buffer.
    output_ready: bool,
    /// Set by stop: write silence, do not render.
    muted: AtomicBool,
    /// The render closure panicked: silence from then on.
    failed: AtomicBool,
}

impl Stream {
    /// Fills buffer half `half` of every channel with silence.
    fn fill_silence(&self, half: usize) {
        for halves in &self.buffers[..self.channels] {
            // SAFETY: each half is a driver buffer of `bytes` bytes.
            unsafe { ptr::write_bytes(halves[half], self.silence, self.bytes) };
        }
    }

    /// Fills buffer half `index` (the realtime part).
    fn serve(&self, index: i32) {
        let half = (index & 1) as usize;
        if self.muted.load(SeqCst) || self.failed.load(SeqCst) {
            self.fill_silence(half);
        } else {
            let rendered = {
                // The slices live in a fixed array on this stack frame, so
                // nothing is allocated. Unused entries stay empty slices.
                let mut views: [&mut [u8]; MAX_CHANNELS] =
                    std::array::from_fn(|_| Default::default());
                for (view, halves) in views.iter_mut().zip(&self.buffers[..self.channels]) {
                    // SAFETY: the driver's buffers are `bytes` long and were
                    // checked not to overlap, so these slices are disjoint;
                    // the driver reads this half only after we return.
                    *view = unsafe { std::slice::from_raw_parts_mut(halves[half], self.bytes) };
                }
                // SAFETY: only the callback touches the closure while the
                // stream is published, and callbacks do not overlap.
                let render = unsafe { &mut *self.render.get() };
                catch_unwind(AssertUnwindSafe(|| render(&mut views[..self.channels])))
            };
            if rendered.is_err() {
                self.failed.store(true, SeqCst);
                EVENTS.render_panicked.store(true, SeqCst);
                self.fill_silence(half);
            }
        }
        if self.output_ready {
            // SAFETY: the driver outlives the stream; the SDK intends this
            // call from the buffer switch.
            unsafe { ((*(*self.asio).vtbl).outputReady)(self.asio) };
        }
    }
}

/// The callback table handed to every `createBuffers`.
const CALLBACKS: AsioCallbacks = AsioCallbacks {
    bufferSwitch: on_buffer_switch,
    sampleRateDidChange: on_sample_rate_did_change,
    asioMessage: on_asio_message,
    bufferSwitchTimeInfo: on_buffer_switch_time_info,
};

/// Serves one buffer switch for the published stream, if any.
fn serve(index: i32) {
    /// Leaves the callback count even if something unwinds.
    struct Leave;
    impl Drop for Leave {
        fn drop(&mut self) {
            IN_CALLBACK.fetch_sub(1, SeqCst);
        }
    }
    IN_CALLBACK.fetch_add(1, SeqCst);
    let _leave = Leave;
    let stream = STREAM.load(SeqCst);
    if !stream.is_null() {
        // SAFETY: published streams stay alive until unpublish() has seen
        // this callback leave.
        unsafe { (*stream).serve(index) };
    }
}

// The four callbacks. None may unwind into the driver: each catches panics.

extern "C" fn on_buffer_switch(index: i32, _direct_process: i32) {
    let _ = catch_unwind(|| serve(index));
}

extern "C" fn on_buffer_switch_time_info(
    params: *mut AsioTime,
    index: i32,
    _direct_process: i32,
) -> *mut AsioTime {
    let _ = catch_unwind(|| serve(index));
    params
}

extern "C" fn on_sample_rate_did_change(rate: f64) {
    let _ = catch_unwind(|| {
        let expected = f64::from_bits(EXPECTED_RATE.load(SeqCst));
        if expected > 0.0 && !rates_equal(expected, rate) {
            EVENTS.new_rate.store(rate.to_bits(), SeqCst);
            EVENTS.rate_changed.store(true, SeqCst);
        }
    });
}

extern "C" fn on_asio_message(
    selector: i32,
    value: i32,
    _message: *mut c_void,
    _opt: *mut f64,
) -> i32 {
    catch_unwind(|| {
        let (answer, effect) = answer_message(selector, value);
        match effect {
            MessageEffect::None => {}
            MessageEffect::Reset => EVENTS.reset.store(true, SeqCst),
            MessageEffect::Dropout => {
                EVENTS.dropouts.fetch_add(1, SeqCst);
            }
            MessageEffect::LatenciesChanged => EVENTS.latencies_changed.store(true, SeqCst),
        }
        answer
    })
    .unwrap_or(0)
}
