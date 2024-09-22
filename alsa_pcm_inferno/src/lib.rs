extern crate alsa_sys_all;
extern crate libc;
use alsa_sys_all::*;

use futures_util::FutureExt;
use inferno_aoip::utils::{run_future_in_new_thread, LogAndForget};
use inferno_aoip::{AtomicSample, DeviceInfo, DeviceServer, ExternalBufferParameters, MediaClock, RealTimeClockReceiver, Sample, SelfInfoBuilder};
use lazy_static::lazy_static;
use libc::{c_char, c_int, c_uint, c_void, free, malloc, EBUSY};
use tokio::sync::{mpsc, oneshot};
use std::borrow::BorrowMut;
use std::collections::VecDeque;
use std::env;
use std::ptr::{null_mut, null};
use std::mem::zeroed;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::thread::JoinHandle;
use std::time::Instant;


enum Command {
    Receive(Vec<ExternalBufferParameters<Sample>>, oneshot::Receiver<usize>, oneshot::Sender<RealTimeClockReceiver>, Arc<AtomicUsize>),
    Transmit(Vec<ExternalBufferParameters<Sample>>, oneshot::Receiver<usize>, oneshot::Sender<RealTimeClockReceiver>, Arc<AtomicUsize>),
    StopReceiver,
    StopTransmitter,
    Shutdown,
}

struct InfernoCommon {
    commands_sender: mpsc::Sender<Command>,
    thread: JoinHandle<()>,
    capturing: bool,
    playing: bool,
}

const SUPPORTED_HW_FORMATS: [u32; 1] = [SND_PCM_FORMAT_S32 as u32];
const PLUGIN_NAME: [u8; 23] = *b"Inferno virtual device\0";
lazy_static! {
    static ref inferno_global: Mutex<Option<InfernoCommon>> = Mutex::new(None);
    static ref rx_channels_count: usize = env::var("INFERNO_RX_CHANNELS").ok().
        map(|s|s.parse().expect("invalid INFERNO_RX_CHANNELS, must be integer")).unwrap_or(2);
    static ref tx_channels_count: usize = env::var("INFERNO_TX_CHANNELS").ok().
        map(|s|s.parse().expect("invalid INFERNO_TX_CHANNELS, must be integer")).unwrap_or(2);
}

fn init_common(self_info: DeviceInfo) {
    let mut common_opt = inferno_global.lock().unwrap();
    if common_opt.is_none() {
        //self_info.sample_rate = sample_rate;
        // TODO make tx & rx channels based on (*io).channels
        // this requires a complicated refactor to allow adding channels to the Dante network dynamically at any time, not just on DeviceServer start
        // because we don't know beforehand whether the app will be capture&playback, playback-only or capture-only
        // so we don't know whether we should wait for the second prepare call to gather all channels counts

        //assert_eq!(self_info.sample_rate, sample_rate);

        let (commands_sender, mut commands_rx) = mpsc::channel(16);

        let thread = run_future_in_new_thread("Inferno main", move || async move {
            let mut device_server = DeviceServer::start(self_info).await;
            loop {
                match commands_rx.recv().await {
                    Some(command) => match command {
                        Command::Receive(channels, start_time_rx, clock_tx, current_timestamp) => {
                            clock_tx.send(device_server.get_realtime_clock_receiver());
                            device_server.receive_to_external_buffer(channels, start_time_rx, current_timestamp).await;
                            println!("started receiver");
                        },
                        Command::Transmit(channels, start_time_rx, clock_tx, current_timestamp) => {
                            clock_tx.send(device_server.get_realtime_clock_receiver());
                            device_server.transmit_from_external_buffer(channels, start_time_rx, current_timestamp).await;
                            println!("started transmitter");
                        },
                        Command::StopReceiver => {
                            device_server.stop_receiver().await;
                            println!("stopped receiver");
                        },
                        Command::StopTransmitter => {
                            device_server.stop_transmitter().await;
                            println!("stopped transmitter");
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
        }.boxed_local());
        *common_opt = Some(InfernoCommon {
            commands_sender,
            thread,
            capturing: false,
            playing: false,
        });
    }
}

struct StreamInfo {
    boundary: usize,
}

#[repr(C)]
struct MyIOPlug {
    io: snd_pcm_ioplug_t,
    callbacks: snd_pcm_ioplug_callback_t,
    self_info: DeviceInfo,
    ref_time: Instant,
    stream_info: Option<StreamInfo>,
    buffers_valid: Arc<RwLock<bool>>,
    media_clock: MediaClock,
    clock_receiver: Option<RealTimeClockReceiver>,
    start_time: Option<usize>,
    start_time_tx: Option<oneshot::Sender<usize>>,
    current_timestamp: Arc<AtomicUsize>,
    // TODO refactor multiple Options to single Option<struct>
}

unsafe fn get_private<'a>(io: *mut snd_pcm_ioplug_t) -> &'a mut MyIOPlug {
    &mut *((*io).private_data as *mut MyIOPlug)
}

unsafe extern "C" fn plugin_pointer(io: *mut snd_pcm_ioplug_t) -> snd_pcm_sframes_t {
    let this = get_private(io);
    let cur = this.current_timestamp.load(Ordering::SeqCst /*XXX*/);
    let ptr = if this.start_time.is_some() && (cur != usize::MAX) {
        //println!("using current_timestamp: {cur}");
        // It is important to use actual input/output clock here because sample precision is required.
        // Otherwise app may overwrite not-yet-sent samples or read not-yet-received ones.
        cur.wrapping_sub(this.start_time.unwrap()) as i64
    } else {
        //println!("using own clock");
        // ... but fall back to system clock when not transmitting/receiving
        if let Some(clock_receiver) = &mut this.clock_receiver {
            if clock_receiver.update() {
                if let Some(overlay) = clock_receiver.get() {
                    this.media_clock.update_overlay(*overlay);
                }
            }
        }
        let now_samples_opt = this.media_clock.now_in_timebase((*io).rate as u64);
        if now_samples_opt.is_some() && this.start_time.is_none() {
            this.start_time = now_samples_opt;
            this.start_time_tx.take().unwrap().send(now_samples_opt.unwrap()).expect("failed to send start timestamp");
        }
        now_samples_opt.map(|now_samples| now_samples.wrapping_sub(this.start_time.unwrap())).unwrap_or(0) as i64
    };
    // TODO may be non-monotonic in edge cases (switching between clocks)
    let buffered = match (*io).stream {
        SND_PCM_STREAM_CAPTURE => ptr.wrapping_sub((*io).appl_ptr as i64),
        SND_PCM_STREAM_PLAYBACK => ((*io).appl_ptr as i64).wrapping_sub(ptr),
        _ => 0
    };
    if buffered < 0 {
        println!("buffered: {buffered} samples");
    }
    ptr
}

fn get_app_name() -> Option<String> {
    Some(std::env::current_exe().ok()?.file_name()?.to_string_lossy().to_string())
}

unsafe extern "C" fn plugin_prepare(io: *mut snd_pcm_ioplug_t) -> c_int {
    println!("plugin_prepare called");
    let this = get_private(io);

    let channels_areas = snd_pcm_ioplug_mmap_areas(io);
    if channels_areas.is_null() {
        panic!("snd_pcm_ioplug_mmap_areas returned null, unable to get audio memory addresses");
    }

    let bits_per_sample = (8 * size_of::<Sample>()) as u32;
    let channels_areas = std::slice::from_raw_parts(channels_areas, (*io).channels as usize);
    for area in channels_areas {
        println!("got address {:x} with first {}b, step {}b, size {} samples * {} channels", area.addr as usize, area.first, area.step, (*io).buffer_size, channels_areas.len());
        if (area.first % 8) != 0 || (area.step % 8) != 0 {
            panic!("sample size is not measured in whole bytes, unsupported");
        }
        if (area.first % bits_per_sample) != 0 || (area.step % bits_per_sample) != 0 {
            panic!("samples not aligned, unsupported");
        }
    }
    println!("period size: {}", (*io).period_size);

    let channels_buffers = channels_areas.iter().map(|area| {
        ExternalBufferParameters::<Sample>::new(
            area.addr.byte_offset((area.first/8) as isize) as *const AtomicSample,
            ((*io).buffer_size as usize) * channels_areas.len() - ((area.first/bits_per_sample) as usize),
            (area.step/bits_per_sample) as usize,
            this.buffers_valid.clone()
        )
    }).collect();

    let mut swparams = std::ptr::null_mut::<snd_pcm_sw_params_t>();
    if snd_pcm_sw_params_malloc(&mut swparams) != 0 {
        panic!("snd_pcm_sw_params_malloc failed");
    }
    let boundary = if snd_pcm_sw_params_current((*io).pcm, swparams) == 0 {
        let mut value = 0;
        snd_pcm_sw_params_get_boundary(swparams, &mut value);
        value
    } else {
        panic!("snd_pcm_sw_params_current failed");
    };
    snd_pcm_sw_params_free(swparams);
    assert!(boundary != 0);
    println!("boundary: {boundary}");
    this.stream_info = Some(StreamInfo {
        boundary: boundary as usize // TODO use this
    });

    let (start_time_tx, start_time_rx) = oneshot::channel::<usize>();
    let (clock_tx, clock_rx) = oneshot::channel::<RealTimeClockReceiver>();

    init_common(this.self_info.clone());
    {
        let mut common_guard = inferno_global.lock();
        let common = common_guard.as_mut().unwrap().as_mut().unwrap();
        this.start_time_tx = None;
        this.current_timestamp.store(usize::MAX, Ordering::SeqCst);
        match (*io).stream {
            SND_PCM_STREAM_CAPTURE => {
                if common.capturing {
                    common.commands_sender.blocking_send(Command::StopReceiver).unwrap();
                }
                common.capturing = true;
                common.commands_sender.blocking_send(Command::Receive(channels_buffers, start_time_rx, clock_tx, this.current_timestamp.clone())).unwrap();
            },
            SND_PCM_STREAM_PLAYBACK => {
                if common.playing {
                    common.commands_sender.blocking_send(Command::StopTransmitter).unwrap();
                }
                common.playing = true;
                common.commands_sender.blocking_send(Command::Transmit(channels_buffers, start_time_rx, clock_tx, this.current_timestamp.clone())).unwrap();
            },
            _ => {
                panic!("unknown stream direction");
            }
        }
    }

    this.clock_receiver = Some(clock_rx.blocking_recv().expect("no clocks available"));
    this.start_time_tx = Some(start_time_tx);
    *this.buffers_valid.write().unwrap() = true;

    0
}

unsafe extern "C" fn plugin_start(io: *mut snd_pcm_ioplug_t) -> c_int {
    println!("plugin_start called");
    let this = get_private(io);
    if let Some(clock_receiver) = &mut this.clock_receiver {
        if clock_receiver.update() {
            if let Some(overlay) = clock_receiver.get() {
                this.media_clock.update_overlay(*overlay);
            }
        }
    }
    let now_samples_opt = this.media_clock.now_in_timebase((*io).rate as u64);
    if now_samples_opt.is_some() && this.start_time.is_none() {
        this.start_time = now_samples_opt;
        this.start_time_tx.take().unwrap().send(now_samples_opt.unwrap()).expect("failed to send start timestamp");
    }
    0
}

unsafe extern "C" fn plugin_stop(io: *mut snd_pcm_ioplug_t) -> c_int {
    println!("plugin_stop called");

    let this = get_private(io);
    *this.buffers_valid.write().unwrap() = false;
    
    let mut common_guard = inferno_global.lock();
    let common_opt = common_guard.as_mut().unwrap();
    // TODO blocking_send inside mutex, risk of deadlock?
    if let Some(common) = common_opt.as_mut() {
        match (*io).stream {
            SND_PCM_STREAM_CAPTURE => {
                if common.capturing {
                    common.commands_sender.blocking_send(Command::StopReceiver).unwrap();
                    common.capturing = false;
                } else {
                    println!("plugin_stop called more than once for capture stream");
                }
            },
            SND_PCM_STREAM_PLAYBACK => {
                if common.playing {
                    common.commands_sender.blocking_send(Command::StopTransmitter).unwrap();
                    common.playing = false;
                } else {
                    println!("plugin_stop called more than once for playback stream");
                }
            },
            _ => {
                panic!("unknown stream direction");
            }
        }
        if (!common.capturing) && (!common.playing) {
            //common.commands_sender.blocking_send(Command::Shutdown).unwrap();
            // don't do this, TODO think of something better
        }
    }

    0
}

unsafe extern "C" fn plugin_transfer(io: *mut snd_pcm_ioplug_t, areas: *const snd_pcm_channel_area_t, offset: snd_pcm_uframes_t, size: snd_pcm_uframes_t) -> snd_pcm_sframes_t {
    println!("plugin_transfer called, size: {:?}", size);
    size as snd_pcm_sframes_t
}

unsafe extern "C" fn plugin_close(io: *mut snd_pcm_ioplug_t) -> c_int {
    println!("plugin_close called");

    0
}


unsafe extern "C" fn plugin_define(pcmp: *mut *mut snd_pcm_t, name: *const c_char, root: *const snd_config_t, conf: *const snd_config_t, stream: snd_pcm_stream_t, mode: c_int) -> c_int {
    let app_name = get_app_name().unwrap_or(std::process::id().to_string());
    let logenv = env_logger::Env::default().default_filter_or("debug");
    env_logger::builder().parse_env(logenv).format_timestamp_micros().init();
    
    let myio = Box::into_raw(Box::new(MyIOPlug {
        io: zeroed(),
        callbacks: snd_pcm_ioplug_callback_t {
            prepare: Some(plugin_prepare),
            start: Some(plugin_start),
            stop: Some(plugin_stop),
            pointer: Some(plugin_pointer),
            transfer: Some(plugin_transfer),
            close: Some(plugin_close),
            ..zeroed()
        },
        self_info: DeviceInfo::new_self(&format!("{app_name} via Inferno-AoIP"), &app_name, None).make_rx_channels(*rx_channels_count).make_tx_channels(*tx_channels_count),
        ref_time: Instant::now(),
        stream_info: None,
        buffers_valid: Arc::new(RwLock::new(false)),
        media_clock: MediaClock::new(),
        clock_receiver: None,
        start_time: None,
        start_time_tx: None,
        current_timestamp: Arc::new(AtomicUsize::new(usize::MAX)),
    }));

    let io = &mut (*myio).io;
    io.version = (1<<16) | (0<<8) | 2;
    io.name = PLUGIN_NAME.as_ptr() as *const _;
    io.callback = &(*myio).callbacks;
    io.mmap_rw = 1;
    io.private_data = myio as *mut _;

    let r = snd_pcm_ioplug_create(io, name, stream, mode);
    if r < 0 {
        panic!("snd_pcm_ioplug_create returned {r}");
    }

    let r = snd_pcm_ioplug_set_param_list(io, SND_PCM_IOPLUG_HW_FORMAT as i32, SUPPORTED_HW_FORMATS.len() as u32, SUPPORTED_HW_FORMATS.as_ptr());
    if r < 0 {
        panic!("snd_pcm_ioplug_set_param_list SND_PCM_IOPLUG_HW_FORMAT returned {r}");
    }
    //snd_pcm_ioplug_set_param_list(io, SND_PCM_IOPLUG_HW_ACCESS as i32, 2, [SND_PCM_ACCESS_MMAP_INTERLEAVED as u32, SND_PCM_ACCESS_MMAP_NONINTERLEAVED as u32].as_ptr());

    let r = snd_pcm_ioplug_set_param_list(io, SND_PCM_IOPLUG_HW_RATE as i32, 1, [(*myio).self_info.sample_rate].as_ptr());
    if r < 0 {
        panic!("snd_pcm_ioplug_set_param_list SND_PCM_IOPLUG_HW_RATE returned {r}");
    }

    let num_channels = match (*io).stream {
        SND_PCM_STREAM_CAPTURE => *rx_channels_count,
        SND_PCM_STREAM_PLAYBACK => *tx_channels_count,
        _ => 0
    } as u32;

    let r = snd_pcm_ioplug_set_param_list(io, SND_PCM_IOPLUG_HW_CHANNELS as i32, 1, [num_channels].as_ptr());
    if r < 0 {
        panic!("snd_pcm_ioplug_set_param_list SND_PCM_IOPLUG_HW_CHANNELS returned {r}");
    }

    *pcmp = (*myio).io.pcm;

    println!("plugin_define end");
    0
}

#[no_mangle]
pub extern "C" fn _snd_pcm_inferno_open(pcmp: *mut *mut snd_pcm_t, name: *const c_char, root: *const snd_config_t, conf: *const snd_config_t, stream: snd_pcm_stream_t, mode: c_int) -> c_int {
    unsafe { plugin_define(pcmp, name, root, conf, stream, mode) }
}

#[no_mangle]
pub extern "C" fn __snd_pcm_inferno_open_dlsym_pcm_001() {
}


/* #[link(name = "asound")]
extern "C" {
    fn snd_pcm_ioplug_create(io: *mut snd_pcm_ioplug_t, name: *const c_char, stream: snd_pcm_stream_t, mode: c_int, flags: c_uint) -> c_int;
}
 */