extern crate alsa_sys_all;
extern crate libc;
use alsa_sys_all::*;

use futures_util::FutureExt;
use inferno_aoip::device_info::DeviceId;
use inferno_aoip::device_server::{
    AtomicSample, Clock, DeviceServer, ExternalBufferParameters, MediaClock, RealTimeClockReceiver,
    Sample, Settings, TransferNotifier,
};
use inferno_aoip::utils::run_future_in_new_thread;
use itertools::Itertools;
use lazy_static::lazy_static;
use libc::{c_char, c_int, c_void, eventfd, EFD_CLOEXEC, EFD_NONBLOCK, EPIPE, POLLIN, POLLOUT};
use log::{debug, error, warn};
use std::collections::BTreeMap;
use std::ffi::CStr;
use std::mem::zeroed;
use std::num::Wrapping;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot};

struct StartArgs {
    channels: Vec<ExternalBufferParameters<Sample>>,
    start_time_rx: oneshot::Receiver<Clock>,
    clock_rx_tx: oneshot::Sender<RealTimeClockReceiver>,
    current_timestamp: Arc<AtomicUsize>,
    on_transfer: Option<TransferNotifier>,
}

enum Command {
    StartReceiver(StartArgs),
    StartTransmitter(StartArgs),
    StopReceiver,
    StopTransmitter,
    Shutdown,
}

struct InfernoInstance {
    commands_sender: mpsc::Sender<Command>,
    thread: JoinHandle<()>,
    capturing: bool,
    playing: bool,
}

const PLUGIN_NAME: [u8; 23] = *b"Inferno virtual device\0";

lazy_static! {
    static ref global_instances: RwLock<BTreeMap<DeviceId, Arc<Mutex<InfernoInstance>>>> =
        RwLock::new(BTreeMap::new());
    static ref global_initialized: Mutex<bool> = false.into();
}

fn get_or_create_instance(settings: &Settings) -> Arc<Mutex<InfernoInstance>> {
    let mut instances_locked = global_instances.write().unwrap();
    let entry = instances_locked.entry(settings.self_info.factory_device_id);
    entry
        .or_insert_with(|| {
            //self_info.sample_rate = sample_rate;
            // TODO make tx & rx channels based on (*io).channels
            // this requires a complicated refactor to allow adding channels to the Dante network dynamically at any time, not just on DeviceServer start
            // because we don't know beforehand whether the app will be capture&playback, playback-only or capture-only
            // so we don't know whether we should wait for the second prepare call to gather all channels counts

            //assert_eq!(self_info.sample_rate, sample_rate);

            let (commands_sender, mut commands_rx) = mpsc::channel(16);

            let settings = settings.clone();
            let thread = run_future_in_new_thread("Inferno main", move || {
                async move {
                    // TODO DeviceServer::start may fail internally (e.g. if other process using Inferno is already listening on network ports)
                    // retrying infinitely will spam the log. (Audacity behaves this way if Inferno is already running)
                    let mut device_server = DeviceServer::start(settings).await;
                    loop {
                        match commands_rx.recv().await {
                            Some(command) => match command {
                                Command::StartReceiver(StartArgs {
                                    channels,
                                    start_time_rx,
                                    clock_rx_tx,
                                    current_timestamp,
                                    on_transfer,
                                }) => {
                                    clock_rx_tx.send(device_server.get_realtime_clock_receiver());
                                    device_server
                                        .receive_to_external_buffer(
                                            channels,
                                            start_time_rx,
                                            current_timestamp,
                                            on_transfer,
                                        )
                                        .await;
                                    debug!("started receiver");
                                }
                                Command::StartTransmitter(StartArgs {
                                    channels,
                                    start_time_rx,
                                    clock_rx_tx,
                                    current_timestamp,
                                    on_transfer,
                                }) => {
                                    clock_rx_tx.send(device_server.get_realtime_clock_receiver());
                                    device_server
                                        .transmit_from_external_buffer(
                                            channels,
                                            start_time_rx,
                                            current_timestamp,
                                            on_transfer,
                                        )
                                        .await;
                                    debug!("started transmitter");
                                }
                                Command::StopReceiver => {
                                    device_server.stop_receiver().await;
                                    debug!("stopped receiver");
                                }
                                Command::StopTransmitter => {
                                    device_server.stop_transmitter().await;
                                    debug!("stopped transmitter");
                                }
                                Command::Shutdown => {
                                    break;
                                }
                            },
                            None => {
                                break;
                            }
                        }
                    }
                    device_server.shutdown().await;
                }
                .boxed_local()
            });

            Arc::new(Mutex::new(InfernoInstance {
                commands_sender,
                thread,
                capturing: false.into(),
                playing: false.into(),
            }))
        })
        .clone()
}

fn get_instance(settings: &Settings) -> Option<Arc<Mutex<InfernoInstance>>> {
    let instances_locked = global_instances.read().unwrap();
    instances_locked
        .get(&settings.self_info.factory_device_id)
        .map(|a| a.clone())
}

struct StreamInfo {
    boundary: snd_pcm_uframes_t,
    boundary_add: Wrapping<snd_pcm_sframes_t>,
}

#[repr(C)]
struct MyIOPlug {
    io: snd_pcm_ioplug_t,
    callbacks: snd_pcm_ioplug_callback_t,
    settings: Settings,
    //config: BTreeMap<String, String>,
    ref_time: Instant,
    use_flows_clock: bool,
    stream_info: Option<StreamInfo>,
    buffers_valid: Arc<RwLock<bool>>,
    media_clock: MediaClock,
    clock_receiver: Option<RealTimeClockReceiver>,
    start_time: Option<Clock>,
    start_time_tx: Option<oneshot::Sender<Clock>>,
    current_timestamp: Arc<AtomicUsize>,
    on_transfer_eventfd: libc::c_int,
    on_transfer_enabled: bool,
    last_transfer_buffer_offset: snd_pcm_uframes_t,
    transfer_offset_add: Wrapping<Clock>,
    // TODO refactor multiple Options to single Option<struct>
}

unsafe fn get_private<'a>(io: *mut snd_pcm_ioplug_t) -> &'a mut MyIOPlug {
    &mut *((*io).private_data as *mut MyIOPlug)
}

unsafe fn plugin_pointer_impl(io: *mut snd_pcm_ioplug_t) -> snd_pcm_sframes_t {
    let this = get_private(io);
    // ALSA may ask for the pointer before the stream was ever prepared
    // (snd_pcm_status/snd_pcm_avail right after open - JACK does, see
    // teodly/inferno#8). There is no stream yet, so nothing has moved.
    // Unwrapping stream_info here panicked, and a panic in an extern "C"
    // callback aborts the host process.
    let Some(boundary_u) = this.stream_info.as_ref().map(|si| si.boundary) else {
        return 0;
    };
    // boundary comes from ALSA as unsigned; ALSA keeps it within the signed
    // range, but a value that does not fit must not panic either.
    let Ok(boundary) = snd_pcm_sframes_t::try_from(boundary_u) else {
        error!("boundary {boundary_u} does not fit in a signed frame count, reporting xrun");
        return -(EPIPE as snd_pcm_sframes_t);
    };
    let cur = this
        .current_timestamp
        .load(Ordering::SeqCst /*TODO: really needed?*/);

    // TODO may be non-monotonic in edge cases (switching between clocks)
    // TODO rethink int sizes here
    let mut ptr: snd_pcm_sframes_t = if this.start_time.is_some() && (cur != usize::MAX) {
        //println!("using current_timestamp: {cur}");
        // It is important to use actual input/output clock here because sample precision is required.
        // Otherwise app may overwrite not-yet-sent samples or read not-yet-received ones.
        cur.wrapping_sub(this.start_time.unwrap()) as snd_pcm_sframes_t
    } else {
        //println!("using own clock");
        // ... but fall back to system clock when not transmitting/receiving right now
        if let Some(clock_receiver) = &mut this.clock_receiver {
            if clock_receiver.update() {
                if let Some(overlay) = clock_receiver.get() {
                    this.media_clock.update_overlay(*overlay);
                }
            }
        }
        // here we must use long now_in_timebase (not wrapping) because boundary may be not power-of-2
        // TODO: optimize by setting plugin_pointer callback depending on whether boundary is power-of-2
        let now_samples_opt = this.media_clock.now_in_timebase((*io).rate as u64);
        if now_samples_opt.is_some() && this.start_time.is_none() {
            /*warn!("warning: setting start_time in plugin_pointer, not plugin_start");
            this.start_time = Some(now_samples_opt.unwrap() as usize);
            if let Some(start_time_tx) = this.start_time_tx.take() {
                if let Err(e) = start_time_tx.send(now_samples_opt.unwrap()) {
                    error!("failed to send start timestamp: {e}. tx/rx will not work.");
                }
            } else {
                error!("failed to send start timestamp: start_time_tx already used, BUG");
            }*/
            warn!("looks like app is calling plugin_pointer before clock is valid, returning 0");
        }
        if now_samples_opt.is_some() && this.start_time.is_some() {
            (now_samples_opt.unwrap() as usize).wrapping_sub(this.start_time.unwrap())
                as snd_pcm_sframes_t
        } else {
            //log::debug!("clock not ready... {} {}", now_samples_opt.is_some(), this.start_time.is_some());
            0
        }
        //now_samples_opt.map(|now_samples| now_samples.wrapping_sub(this.start_time.unwrap())).unwrap_or(0) as i64
    };

    let max_diff = boundary >> 2;
    let appl_ptr = ((*io).appl_ptr as snd_pcm_sframes_t)
        .wrapping_add(this.stream_info.as_ref().unwrap().boundary_add.0);
    let mut diff = ptr.wrapping_sub(appl_ptr);
    // handle situation when our "hardware" pointer wraps around boundary but the application pointer not yet (or vice versa):
    if diff.saturating_abs() > max_diff {
        let d = diff.wrapping_add(boundary);
        if d.saturating_abs() <= max_diff {
            diff = d;
        } else {
            let d = diff.wrapping_sub(boundary);
            if d.saturating_abs() <= max_diff {
                diff = d;
            } else {
                error!("very large hw-appl ptr diff: {diff} = {ptr} - {appl_ptr}, boundary_add {} ({boundary}). reporting xrun because something is clearly wrong", this.stream_info.as_ref().unwrap().boundary_add.0);
                return (-EPIPE).try_into().unwrap(); // report xrun
            }
        }
    }

    let (dir, buffered) = match (*io).stream {
        SND_PCM_STREAM_CAPTURE => ("capture", diff),
        SND_PCM_STREAM_PLAYBACK => ("playback", (0 as snd_pcm_sframes_t).wrapping_sub(diff)),
        _ => ("???", 0),
    };

    if buffered < 0
        && ((*io).state == SND_PCM_STATE_RUNNING || (*io).state == SND_PCM_STATE_DRAINING)
    {
        if let Ok(hw_ptr) = ptr.try_into() {
            let ioplug_avail = snd_pcm_ioplug_avail(io, hw_ptr, (*io).appl_ptr);
            let ioplug_hw_avail = snd_pcm_ioplug_hw_avail(io, hw_ptr, (*io).appl_ptr);
            warn!("XRUN: buffered for {dir}: {buffered} samples, avail {ioplug_avail}, hw_avail {ioplug_hw_avail}, hw_ptr {hw_ptr}, appl_ptr {}", (*io).appl_ptr);
        } else {
            warn!("severe clock discontinuity");
            return (-EPIPE).try_into().unwrap(); // report xrun
        }

        // workaround for pipewire: grace period during startup:
        if ptr > 100000 || this.stream_info.as_ref().unwrap().boundary_add.0 != 0 {
            return (-EPIPE).try_into().unwrap(); // report xrun
                                                 // TODO check for xruns in ExternalRingBuffer because this function may be called too seldom
                                                 // FIXME we're restarting the whole transmitter/receiver on xrun which results in a LONG break, unacceptable!
        }
    }

    ptr = ptr.wrapping_add(this.stream_info.as_ref().unwrap().boundary_add.0);
    if ptr >= boundary {
        this.stream_info.as_mut().unwrap().boundary_add -= boundary;
        ptr -= boundary;
    }
    //log::debug!("pointer: {ptr}");

    ptr
}

// --- FFI panic guard -------------------------------------------------------
//
// Every entry point ALSA calls is an extern "C" function, and a Rust panic
// cannot unwind out of one: it aborts the whole host process ("panic in a
// function that cannot unwind", teodly/inferno#8). The host is whatever
// application opened the PCM - a DAW, JACK, PipeWire, a recorder - and one
// unwrap() on a dead channel or a poisoned lock inside the plugin took it
// down. ffi_guard turns a panic in a callback into an error return, so ALSA
// reports a failed call (or an xrun, for the pointer) and the host carries on.

fn ffi_guard<R>(name: &str, on_panic: R, f: impl FnOnce() -> R) -> R {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        Ok(r) => r,
        Err(payload) => {
            let msg = payload
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| payload.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "unknown panic".to_string());
            error!("panic in ALSA callback {name}: {msg}; returning an error instead of aborting the host");
            on_panic
        }
    }
}

unsafe extern "C" fn plugin_pointer(io: *mut snd_pcm_ioplug_t) -> snd_pcm_sframes_t {
    ffi_guard("plugin_pointer", -(EPIPE as snd_pcm_sframes_t), || unsafe { plugin_pointer_impl(io) })
}

unsafe extern "C" fn plugin_prepare(io: *mut snd_pcm_ioplug_t) -> c_int {
    ffi_guard("plugin_prepare", -libc::EIO, || unsafe { plugin_prepare_impl(io) })
}

unsafe extern "C" fn plugin_start(io: *mut snd_pcm_ioplug_t) -> c_int {
    ffi_guard("plugin_start", -libc::EIO, || unsafe { plugin_start_impl(io) })
}

unsafe extern "C" fn plugin_stop(io: *mut snd_pcm_ioplug_t) -> c_int {
    ffi_guard("plugin_stop", -libc::EIO, || unsafe { plugin_stop_impl(io) })
}

unsafe extern "C" fn plugin_close(io: *mut snd_pcm_ioplug_t) -> c_int {
    ffi_guard("plugin_close", -libc::EIO, || unsafe { plugin_close_impl(io) })
}

unsafe extern "C" fn plugin_demangle_revents(
    io: *mut snd_pcm_ioplug_t,
    pfd: *mut libc::pollfd,
    nfds: ::std::os::raw::c_uint,
    revents: *mut ::std::os::raw::c_ushort,
) -> ::std::os::raw::c_int {
    ffi_guard("plugin_demangle_revents", -libc::EIO, || unsafe {
        plugin_demangle_revents_impl(io, pfd, nfds, revents)
    })
}

unsafe extern "C" fn plugin_transfer(
    io: *mut snd_pcm_ioplug_t,
    areas: *const snd_pcm_channel_area_t,
    offset: snd_pcm_uframes_t,
    size: snd_pcm_uframes_t,
) -> snd_pcm_sframes_t {
    ffi_guard("plugin_transfer", -(libc::EIO as snd_pcm_sframes_t), || unsafe {
        plugin_transfer_impl(io, areas, offset, size)
    })
}

fn get_app_name() -> Option<String> {
    Some(
        std::env::current_exe()
            .ok()?
            .file_name()?
            .to_string_lossy()
            .to_string(),
    )
}

unsafe fn plugin_prepare_impl(io: *mut snd_pcm_ioplug_t) -> c_int {
    debug!("plugin_prepare called");

    let this = get_private(io);

    let channels_areas = snd_pcm_ioplug_mmap_areas(io);
    if channels_areas.is_null() {
        error!("snd_pcm_ioplug_mmap_areas returned null, unable to get audio memory addresses");
        return -libc::EINVAL;
    }

    let bits_per_sample = (8 * size_of::<Sample>()) as u32;
    let channels_areas = std::slice::from_raw_parts(channels_areas, (*io).channels as usize);
    if channels_areas.len() > 0 {
        let area = &channels_areas[0];
        debug!("got buffer size {} samples * {} channels, first channel: address {:x} with first {}b, step {}b", (*io).buffer_size, channels_areas.len(), area.addr as usize, area.first, area.step);
    }
    for area in channels_areas {
        if (area.first % 8) != 0 || (area.step % 8) != 0 {
            error!("sample size is not measured in whole bytes, unsupported");
            return -libc::EINVAL;
        }
        if (area.first % bits_per_sample) != 0 || (area.step % bits_per_sample) != 0 {
            error!("samples not aligned, unsupported");
            return -libc::EINVAL;
        }
    }
    debug!("period size: {}", (*io).period_size);

    let channels_buffers = channels_areas
        .iter()
        .enumerate()
        .map(|(_ch_index, area)| {
            ExternalBufferParameters::<Sample>::new(
                area.addr.byte_offset((area.first / 8) as isize) as *const AtomicSample,
                ((*io).buffer_size as usize) * channels_areas.len()
                    - ((area.first / bits_per_sample) as usize),
                (area.step / bits_per_sample) as usize,
                this.buffers_valid.clone(),
                None,
            )
        })
        .collect();

    let mut swparams = std::ptr::null_mut::<snd_pcm_sw_params_t>();
    let r = snd_pcm_sw_params_malloc(&mut swparams);
    if r != 0 {
        error!("snd_pcm_sw_params_malloc failed");
        return r;
    }
    let r = snd_pcm_sw_params_current((*io).pcm, swparams);
    let boundary: snd_pcm_uframes_t = if r == 0 {
        let mut value = 0;
        snd_pcm_sw_params_get_boundary(swparams, &mut value);
        value
    } else {
        error!("snd_pcm_sw_params_current failed");
        return r;
    };
    snd_pcm_sw_params_free(swparams);
    assert!(boundary != 0);
    debug!("boundary: {boundary}");
    this.stream_info = Some(StreamInfo {
        boundary,
        boundary_add: Wrapping(0),
    });
    this.start_time = None;

    let (start_time_tx, start_time_rx) = oneshot::channel::<Clock>();
    let (clock_rx_tx, clock_rx_rx) = oneshot::channel::<RealTimeClockReceiver>();

    let inferno_instance = get_or_create_instance(&this.settings);
    {
        let mut common = inferno_instance.lock().unwrap();
        this.start_time_tx = None;
        this.current_timestamp.store(usize::MAX, Ordering::SeqCst);
        let args = StartArgs {
            channels: channels_buffers,
            start_time_rx,
            clock_rx_tx,
            current_timestamp: if this.use_flows_clock {
                this.current_timestamp.clone()
            } else {
                Default::default()
            },
            on_transfer: if this.on_transfer_enabled {
                let efd = this.on_transfer_eventfd;
                Some(TransferNotifier {
                    callback: Box::new(move || {
                        libc::write(efd, [1u64].as_ptr() as *const c_void, 8);
                    }),
                    max_interval_samples: (*io).period_size.try_into().unwrap(),
                })
            } else {
                None
            },
        };
        let mut err = false;
        match (*io).stream {
            SND_PCM_STREAM_CAPTURE => {
                if common.capturing {
                    err = common
                        .commands_sender
                        .blocking_send(Command::StopReceiver)
                        .is_err();
                }
                common.capturing = !err;
                err |= common
                    .commands_sender
                    .blocking_send(Command::StartReceiver(args))
                    .is_err();
            }
            SND_PCM_STREAM_PLAYBACK => {
                if common.playing {
                    err = common
                        .commands_sender
                        .blocking_send(Command::StopTransmitter)
                        .is_err();
                }
                common.playing = !err;
                err |= common
                    .commands_sender
                    .blocking_send(Command::StartTransmitter(args))
                    .is_err();
            }
            _ => {
                error!("unknown stream direction");
                return -libc::EINVAL;
            }
        }
        if err {
            error!("BUG: error sending to Inferno thread");
            return -libc::EPIPE;
        }
    }

    this.clock_receiver = match clock_rx_rx.blocking_recv() {
        Ok(clk) => Some(clk),
        Err(_) => {
            error!("no clock available (couldn't receive clock receiver)");
            return -libc::EINVAL;
        }
    };
    this.start_time_tx = Some(start_time_tx);
    if let Some(clock_receiver) = &mut this.clock_receiver {
        let mut ctr = 0;
        loop {
            clock_receiver.update();
            if let Some(overlay) = clock_receiver.get() {
                this.media_clock.update_overlay(*overlay);
            }
            if this.media_clock.is_ready() {
                break;
            }
            std::thread::sleep(Duration::from_millis(250));
            ctr += 1;
            if ctr >= 20 {
                error!("no clock available (timeout waiting for overlay update)");
                return -libc::ETIMEDOUT;
            }
        }
    }
    *this.buffers_valid.write().unwrap() = true;

    0
}

unsafe fn plugin_start_impl(io: *mut snd_pcm_ioplug_t) -> c_int {
    let appl_ptr = (*io).appl_ptr as snd_pcm_sframes_t;
    debug!("plugin_start called with appl_ptr: {appl_ptr}");
    let this = get_private(io);

    if let Some(clock_receiver) = &mut this.clock_receiver {
        clock_receiver.update();
        debug!("clock_receiver updated");
        if let Some(overlay) = clock_receiver.get() {
            this.media_clock.update_overlay(*overlay);
            debug!("media_clock overlay updated");
        } else {
            warn!("no overlay for media_clock");
        }
    }
    // start_time is used (mostly via channels) in:
    // * flows_rx and flows_ts for calculating ringbuffer positions
    // * plugin_pointer for calculating pointer
    // all these calculations are wrapping so we can use wrapping clock
    let now_samples_opt = this.media_clock.wrapping_now_in_timebase((*io).rate as u64);
    if now_samples_opt.is_some() && this.start_time.is_none() {
        this.start_time = Some(now_samples_opt.unwrap() as usize);
        if let Err(e) = this
            .start_time_tx
            .take()
            .unwrap()
            .send(now_samples_opt.unwrap())
        {
            error!("failed to send start timestamp: {e}. tx/rx will not work.");
        }
    } else {
        warn!(
            "can't set start_time in plugin_start: now_samples_opt: {:?}, start_time: {:?}",
            now_samples_opt, this.start_time
        );
    }
    if (*io).stream == SND_PCM_STREAM_PLAYBACK && this.on_transfer_enabled {
        libc::write(
            this.on_transfer_eventfd,
            [1u64].as_ptr() as *const c_void,
            8,
        );
    }
    0
}

unsafe fn plugin_stop_impl(io: *mut snd_pcm_ioplug_t) -> c_int {
    debug!("plugin_stop called");

    let this = get_private(io);
    *this.buffers_valid.write().unwrap() = false;
    drop(this.start_time_tx.take());

    // TODO blocking_send inside mutex, risk of deadlock?
    if let Some(common_mutex) = get_instance(&this.settings) {
        let mut common = common_mutex.lock().unwrap();
        match (*io).stream {
            SND_PCM_STREAM_CAPTURE => {
                if common.capturing {
                    common
                        .commands_sender
                        .blocking_send(Command::StopReceiver)
                        .unwrap();
                    common.capturing = false;
                } else {
                    warn!("plugin_stop called more than once for capture stream");
                }
            }
            SND_PCM_STREAM_PLAYBACK => {
                if common.playing {
                    common
                        .commands_sender
                        .blocking_send(Command::StopTransmitter)
                        .unwrap();
                    common.playing = false;
                } else {
                    warn!("plugin_stop called more than once for playback stream");
                }
            }
            _ => {
                error!("unknown stream direction");
                return -libc::EINVAL;
            }
        }
        if (!common.capturing) && (!common.playing) {
            //common.commands_sender.blocking_send(Command::Shutdown).unwrap();
            // don't do this, TODO think of something better
        }
    }

    0
}

unsafe fn plugin_demangle_revents_impl(
    io: *mut snd_pcm_ioplug_t,
    pfd: *mut libc::pollfd,
    nfds: ::std::os::raw::c_uint,
    revents: *mut ::std::os::raw::c_ushort,
) -> ::std::os::raw::c_int {
    if pfd.is_null() || nfds < 1 || revents.is_null() {
        return -libc::EINVAL;
    }
    let got_events = (*pfd).revents;
    if (*pfd).fd == (*io).poll_fd {
        let mut out_events = got_events & !(POLLIN | POLLOUT);
        if (got_events & POLLIN) != 0 {
            out_events |= match (*io).stream {
                SND_PCM_STREAM_PLAYBACK => POLLOUT,
                SND_PCM_STREAM_CAPTURE => POLLIN,
                _ => return -libc::EINVAL,
            };
            let mut blackhole: [u64; 1] = [0];
            libc::read((*pfd).fd, blackhole.as_mut_ptr() as *mut c_void, 8);
        }
        *revents = out_events as _;
    } else {
        *revents = got_events as _;
    }
    0
}

unsafe fn plugin_transfer_impl(
    io: *mut snd_pcm_ioplug_t,
    areas: *const snd_pcm_channel_area_t,
    offset: snd_pcm_uframes_t,
    size: snd_pcm_uframes_t,
) -> snd_pcm_sframes_t {
    let this = get_private(io);
    let mut blackhole: [u64; 1] = [0];
    libc::read(
        this.on_transfer_eventfd,
        blackhole.as_mut_ptr() as *mut c_void,
        8,
    );
    size as snd_pcm_sframes_t
}

unsafe fn plugin_close_impl(io: *mut snd_pcm_ioplug_t) -> c_int {
    debug!("plugin_close called");
    let this = get_private(io);
    drop(this.start_time_tx.take());
    {
        let mut instances_locked = global_instances.write().unwrap();
        // TODO blocking_send inside mutex, risk of deadlock?
        if let Some(common_mutex) = instances_locked
            .get(&this.settings.self_info.factory_device_id)
            .map(|a| a.clone())
        {
            let common = common_mutex.lock().unwrap();
            if (!common.capturing) && (!common.playing) {
                if let Err(e) = common.commands_sender.blocking_send(Command::Shutdown) {
                    log::error!("BUG: send shutdown via channel failed: {e:?}");
                }
                instances_locked.remove(&this.settings.self_info.factory_device_id);
            }
        }
    }
    libc::close(this.on_transfer_eventfd); // TODO: shouldn't this be in a different place?

    0
}

unsafe fn plugin_define(
    pcmp: *mut *mut snd_pcm_t,
    name: *const c_char,
    root: *const snd_config_t,
    conf: *const snd_config_t,
    stream: snd_pcm_stream_t,
    mode: c_int,
) -> c_int {
    {
        let mut locked_flag = global_initialized.lock().unwrap();
        if !*locked_flag {
            let logenv = env_logger::Env::default().default_filter_or("debug");
            env_logger::builder()
                .parse_env(logenv)
                .format_timestamp_micros()
                // try_init: a host that already installed a `log` logger
                // made init() panic inside open.
                .try_init()
                .ok();
            *locked_flag = true;
        }
    }

    let mut config = BTreeMap::<String, String>::new();
    let mut pos = snd_config_iterator_first(conf);
    while pos != snd_config_iterator_end(conf) {
        let entry = snd_config_iterator_entry(pos);
        let mut key_container: [*const c_char; 1] = [core::ptr::null()];
        let r1 = snd_config_get_id(entry, key_container.as_mut_ptr());
        let mut value_container: [*mut c_char; 1] = [core::ptr::null_mut()];
        let r2 = snd_config_get_ascii(entry, value_container.as_mut_ptr());
        if r1 == 0 && r2 == 0 && (!key_container[0].is_null()) && (!value_container[0].is_null()) {
            let key = CStr::from_ptr(key_container[0]).to_str();
            let value = CStr::from_ptr(value_container[0]).to_str();
            if key.is_ok() && value.is_ok() {
                config.insert(key.unwrap().to_owned(), value.unwrap().to_owned());
            }
        }
        pos = snd_config_iterator_next(pos);
    }

    let efd = eventfd(0, EFD_CLOEXEC | EFD_NONBLOCK);

    let callbacks = snd_pcm_ioplug_callback_t {
        prepare: Some(plugin_prepare),
        start: Some(plugin_start),
        stop: Some(plugin_stop),
        pointer: Some(plugin_pointer),
        close: Some(plugin_close),
        poll_revents: Some(plugin_demangle_revents),
        //transfer: Some(plugin_transfer),
        ..zeroed()
    };

    let app_name = get_app_name().unwrap_or(format!("process {}", std::process::id().to_string()));
    let settings = Settings::new(&app_name, &app_name, None, &config);

    let disable_pollfd = config
        .get("DISABLE_POLLFD")
        .map(|v| v == "1")
        .unwrap_or(false);

    let use_flows_clock = config
        .get("USE_FLOWS_CLOCK")
        .map(|v| v == "1")
        .unwrap_or(false);

    let myio = Box::into_raw(Box::new(MyIOPlug {
        io: zeroed(),
        callbacks,
        settings,
        ref_time: Instant::now(),
        use_flows_clock,
        stream_info: None,
        buffers_valid: Arc::new(RwLock::new(false)),
        media_clock: MediaClock::new(false /* TODO */),
        clock_receiver: None,
        start_time: None,
        start_time_tx: None,
        current_timestamp: Arc::new(AtomicUsize::new(usize::MAX)),
        on_transfer_eventfd: efd,
        on_transfer_enabled: !disable_pollfd,
        last_transfer_buffer_offset: 0,
        transfer_offset_add: Wrapping(0),
    }));

    let io = &mut (*myio).io;
    io.version = (1 << 16) | (0 << 8) | 2;
    io.name = PLUGIN_NAME.as_ptr() as *const _;
    io.callback = &(*myio).callbacks;
    io.flags = SND_PCM_IOPLUG_FLAG_BOUNDARY_WA;
    io.mmap_rw = 1;

    // despite ALSA PCM plugin documentation saying that poll_fd is optional,
    // SoX actually requires it, misbehaving if not notified about transfers
    if !disable_pollfd {
        io.poll_events = POLLIN as u32;
        io.poll_fd = efd;
    }

    io.private_data = myio as *mut _;

    let self_info = &(*myio).settings.self_info;

    let r = snd_pcm_ioplug_create(io, name, stream, mode);
    if r < 0 {
        error!("snd_pcm_ioplug_create returned {r}");
        return r;
    }

    let r = snd_pcm_ioplug_set_param_list(
        io,
        SND_PCM_IOPLUG_HW_FORMAT as i32,
        1,
        [SND_PCM_FORMAT_S32 as u32].as_ptr(),
    );
    if r < 0 {
        error!("snd_pcm_ioplug_set_param_list SND_PCM_IOPLUG_HW_FORMAT returned {r}");
        return r;
    }

    let r = snd_pcm_ioplug_set_param_list(
        io,
        SND_PCM_IOPLUG_HW_ACCESS as i32,
        2,
        [
            SND_PCM_ACCESS_MMAP_INTERLEAVED as u32,
            SND_PCM_ACCESS_RW_INTERLEAVED as u32,
        ]
        .as_ptr(),
    ); // FIXME investigate why planar doesn't work
    if r < 0 {
        error!("snd_pcm_ioplug_set_param_list SND_PCM_IOPLUG_HW_ACCESS returned {r}");
        return r;
    }

    let r = snd_pcm_ioplug_set_param_list(
        io,
        SND_PCM_IOPLUG_HW_RATE as i32,
        1,
        [self_info.sample_rate].as_ptr(),
    );
    if r < 0 {
        error!("snd_pcm_ioplug_set_param_list SND_PCM_IOPLUG_HW_RATE returned {r}");
        return r;
    }

    let num_channels = match (*io).stream {
        SND_PCM_STREAM_CAPTURE => self_info.rx_channels.len(),
        SND_PCM_STREAM_PLAYBACK => self_info.tx_channels.len(),
        _ => {
            error!("no stream specified, cannot continue");
            return -libc::EINVAL;
        }
    } as u32;

    let r = snd_pcm_ioplug_set_param_list(
        io,
        SND_PCM_IOPLUG_HW_CHANNELS as i32,
        1,
        [num_channels].as_ptr(),
    );
    if r < 0 {
        error!("snd_pcm_ioplug_set_param_list SND_PCM_IOPLUG_HW_CHANNELS returned {r}");
        return r;
    }

    let powers_of_2 = |first, max| {
        core::iter::successors(Some(first), move |n| {
            let r = n * 2;
            if r > max {
                None
            } else {
                Some(r)
            }
        })
    };

    let min_periods = 2;
    let min_samples = 16;
    let min_samples_whole_buffer = 1024; // must be power of 2 and > (max receive latency + ALSA period)
    let max_samples = 65536;
    let bytes_per_sample = size_of::<Sample>() as u32;

    let periods_bytes: Vec<std::os::raw::c_uint> =
        powers_of_2(min_samples, max_samples / min_periods)
            .map(|n| (num_channels * bytes_per_sample * n) as std::os::raw::c_uint)
            .collect();
    debug!("HW_PERIOD_BYTES: {periods_bytes:?}");
    let r = snd_pcm_ioplug_set_param_list(
        io,
        SND_PCM_IOPLUG_HW_PERIOD_BYTES as i32,
        periods_bytes.len() as std::os::raw::c_uint,
        periods_bytes.as_ptr(),
    );
    if r < 0 {
        error!("snd_pcm_ioplug_set_param_minmax SND_PCM_IOPLUG_HW_PERIOD_BYTES returned {r}");
        return r;
    }

    let buffer_sizes: Vec<std::os::raw::c_uint> =
        powers_of_2(min_samples_whole_buffer, max_samples)
            .map(|n| (num_channels * bytes_per_sample * n) as std::os::raw::c_uint)
            .collect();
    debug!("HW_BUFFER_BYTES: {buffer_sizes:?}");
    let r = snd_pcm_ioplug_set_param_list(
        io,
        SND_PCM_IOPLUG_HW_BUFFER_BYTES as i32,
        buffer_sizes.len() as std::os::raw::c_uint,
        buffer_sizes.as_ptr(),
    );
    if r < 0 {
        error!("snd_pcm_ioplug_set_param_minmax SND_PCM_IOPLUG_HW_BUFFER_BYTES returned {r}");
        return r;
    }

    let periods_nums: Vec<std::os::raw::c_uint> =
        powers_of_2(min_periods, buffer_sizes.last().unwrap() / min_samples).collect();
    debug!("HW_PERIODS: {periods_nums:?}");
    let r = snd_pcm_ioplug_set_param_list(
        io,
        SND_PCM_IOPLUG_HW_PERIODS as i32,
        periods_nums.len() as std::os::raw::c_uint,
        periods_nums.as_ptr(),
    );
    if r < 0 {
        error!("snd_pcm_ioplug_set_param_minmax SND_PCM_IOPLUG_HW_PERIODS returned {r}");
        return r;
    }

    *pcmp = (*myio).io.pcm;

    debug!("plugin_define end");
    0
}

#[no_mangle]
pub extern "C" fn _snd_pcm_inferno_open(
    pcmp: *mut *mut snd_pcm_t,
    name: *const c_char,
    root: *const snd_config_t,
    conf: *const snd_config_t,
    stream: snd_pcm_stream_t,
    mode: c_int,
) -> c_int {
    ffi_guard("open", -libc::EIO, || unsafe { plugin_define(pcmp, name, root, conf, stream, mode) })
}

#[no_mangle]
pub extern "C" fn __snd_pcm_inferno_open_dlsym_pcm_001() {}

#[cfg(test)]
mod tests {
    use super::ffi_guard;

    #[test]
    fn ffi_guard_passes_results_through() {
        assert_eq!(ffi_guard("test", -5, || 42), 42);
    }

    #[test]
    fn ffi_guard_turns_panics_into_the_error_value() {
        // Without the guard this panic would cross an extern "C" boundary
        // and abort the host process.
        let r = ffi_guard("test", -5, || -> i32 { panic!("boom") });
        assert_eq!(r, -5);
        let r = ffi_guard("test", -5, || -> i32 {
            let none: Option<i32> = None;
            none.unwrap()
        });
        assert_eq!(r, -5);
    }
}
