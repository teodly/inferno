extern crate alsa_sys_all;
extern crate libc;
use alsa_sys_all::*;

use futures_util::FutureExt;
use inferno_aoip::utils::{run_future_in_new_thread, LogAndForget};
use inferno_aoip::{AtomicSample, DeviceInfo, DeviceServer, ExternalBufferParameters, MediaClock, RealTimeClockReceiver, Sample, SelfInfoBuilder};
use lazy_static::lazy_static;
use libc::{c_void, c_int, c_uint, c_char, free, malloc};
use tokio::sync::{mpsc, oneshot};
use std::borrow::BorrowMut;
use std::collections::VecDeque;
use std::ptr::{null_mut, null};
use std::mem::zeroed;
use std::sync::{Arc, Mutex, RwLock};
use std::thread::JoinHandle;
use std::time::Instant;


enum Command {
    Receive(Vec<ExternalBufferParameters<Sample>>, tokio::sync::oneshot::Receiver<usize>),
    Transmit(Vec<ExternalBufferParameters<Sample>>, tokio::sync::oneshot::Receiver<usize>),
    Shutdown,
}

struct InfernoCommon {
    commands_sender: mpsc::Sender<Command>,
    thread: JoinHandle<()>,
    clock_receivers: Vec<RealTimeClockReceiver>,
    capturing: bool,
    playing: bool,
}

const SUPPORTED_HW_FORMATS: [u32; 1] = [SND_PCM_FORMAT_S32 as u32];
const PLUGIN_NAME: [u8; 23] = *b"Inferno virtual device\0";
lazy_static! {
    static ref inferno_global: Mutex<Option<InfernoCommon>> = Mutex::new(None);
}

fn init_common(sample_rate: u32) {
    let mut common_opt = inferno_global.lock().unwrap();
    if common_opt.is_none() {
        let app_name = get_app_name().unwrap_or(std::process::id().to_string());
        let logenv = env_logger::Env::default().default_filter_or("debug");
        env_logger::builder().parse_env(logenv).format_timestamp_micros().init();

        let self_info = DeviceInfo::new_self(&format!("{app_name} via Inferno-AoIP"), &app_name, None).make_rx_channels(2).make_tx_channels(2);
        // TODO make tx & rx channels based on (*io).channels   
        // this requires a complicated refactor to allow adding channels to the Dante network dynamically at any time, not just on DeviceServer start
        // because we don't know beforehand whether the app will be capture&playback, playback-only or capture-only
        // so we don't know whether we should wait for the second prepare call to gather all channels counts

        assert_eq!(self_info.sample_rate, sample_rate); // TODO set self_info.sample_rate based on (*io).rate

        let (commands_sender, mut commands_rx) = mpsc::channel(16);

        let (tx, rx) = oneshot::channel();
        let thread = run_future_in_new_thread("Inferno main", move || async move {
            let mut device_server = DeviceServer::start(self_info).await;
            let clocks = vec![device_server.get_realtime_clock_receiver(), device_server.get_realtime_clock_receiver()];
            tx.send(clocks);
            loop {
                match commands_rx.recv().await {
                    Some(command) => match command {
                        Command::Receive(channels, start_time_rx) => {
                            device_server.receive_to_external_buffer(channels, start_time_rx).await;
                            println!("started receiver");
                        },
                        Command::Transmit(channels, start_time_rx) => {
                            device_server.transmit_from_external_buffer(channels, start_time_rx).await;
                            println!("started transmitter");
                        },
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
        let clocks = rx.blocking_recv().unwrap();
        *common_opt = Some(InfernoCommon {
            commands_sender,
            thread,
            clock_receivers: clocks,
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
    ref_time: Instant,
    stream_info: Option<StreamInfo>,
    buffers_valid: Arc<RwLock<bool>>,
    media_clock: MediaClock,
    clock_receiver: Option<RealTimeClockReceiver>,
    start_time: Option<usize>,
    start_time_tx: Option<tokio::sync::oneshot::Sender<usize>>,
    // TODO refactor multiple Options to single Option<struct>
}

unsafe fn get_private<'a>(io: *mut snd_pcm_ioplug_t) -> &'a mut MyIOPlug {
    &mut *((*io).private_data as *mut MyIOPlug)
}

unsafe extern "C" fn plugin_pointer(io: *mut snd_pcm_ioplug_t) -> snd_pcm_sframes_t {
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
    now_samples_opt.map(|now_samples| now_samples.wrapping_sub(this.start_time.unwrap())).unwrap_or(0) as i64
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

    let (start_time_tx, start_time_rx) = tokio::sync::oneshot::channel::<usize>();

    init_common((*io).rate);
    let mut common_guard = inferno_global.lock();
    let common = common_guard.as_mut().unwrap().as_mut().unwrap();
    match (*io).stream {
        SND_PCM_STREAM_CAPTURE => {
            assert!(!common.capturing); // TODO return error
            common.capturing = true;
            common.commands_sender.blocking_send(Command::Receive(channels_buffers, start_time_rx)).unwrap();
        },
        SND_PCM_STREAM_PLAYBACK => {
            assert!(!common.playing); // TODO return error
            common.playing = true;
            common.commands_sender.blocking_send(Command::Transmit(channels_buffers, start_time_rx)).unwrap();
        },
        _ => {
            panic!("unknown stream direction");
        }
    }

    this.clock_receiver = Some(common.clock_receivers.pop().expect("ran out of clocks"));
    this.start_time_tx = Some(start_time_tx);
    *this.buffers_valid.write().unwrap() = true;

    0
}

unsafe extern "C" fn plugin_start(io: *mut snd_pcm_ioplug_t) -> c_int {
    println!("plugin_start called");
    0
}

unsafe extern "C" fn plugin_stop(io: *mut snd_pcm_ioplug_t) -> c_int {
    println!("plugin_stop called");
    0
}

unsafe extern "C" fn plugin_transfer(io: *mut snd_pcm_ioplug_t, areas: *const snd_pcm_channel_area_t, offset: snd_pcm_uframes_t, size: snd_pcm_uframes_t) -> snd_pcm_sframes_t {
    println!("plugin_transfer called, size: {:?}", size);
    size as snd_pcm_sframes_t
}

unsafe extern "C" fn plugin_close(io: *mut snd_pcm_ioplug_t) -> c_int {
    println!("plugin_close called");
    let this = get_private(io);
    *this.buffers_valid.write().unwrap() = false;
    
    let mut common_guard = inferno_global.lock();
    let common_opt = common_guard.as_mut().unwrap();
    if let Some(common) = common_opt.as_mut() {
        match (*io).stream {
            // FIXME if app opens the device multiple times, strange things can happen
            SND_PCM_STREAM_CAPTURE => {
                common.capturing = false;
            },
            SND_PCM_STREAM_PLAYBACK => {
                common.playing = false;
            },
            _ => {
                panic!("unknown stream direction");
            }
        }
        if (!common.capturing) && (!common.playing) {
            common.commands_sender.blocking_send(Command::Shutdown).unwrap();
        }
    }

    0
}



unsafe extern "C" fn plugin_define(pcmp: *mut *mut snd_pcm_t, name: *const c_char, root: *const snd_config_t, conf: *const snd_config_t, stream: snd_pcm_stream_t, mode: c_int) -> c_int {
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
        ref_time: Instant::now(),
        stream_info: None,
        buffers_valid: Arc::new(RwLock::new(false)),
        media_clock: MediaClock::new(),
        clock_receiver: None,
        start_time: None,
        start_time_tx: None,
    }));

    let io = &mut (*myio).io;
    io.version = (1<<16) | (0<<8) | 2;
    io.name = PLUGIN_NAME.as_ptr() as *const _;
    println!("name address: {}", io.name as usize);
    io.callback = &(*myio).callbacks;
    io.mmap_rw = 1;
    io.private_data = myio as *mut _;

    let r = snd_pcm_ioplug_create(io, name, stream, mode);
    if r < 0 {
        panic!("snd_pcm_ioplug_create returned {r}");
    }

    let r = snd_pcm_ioplug_set_param_list(io, SND_PCM_IOPLUG_HW_FORMAT as i32, SUPPORTED_HW_FORMATS.len() as u32, SUPPORTED_HW_FORMATS.as_ptr());
    if r < 0 {
        panic!("snd_pcm_ioplug_set_param_list returned {r}");
    }
    //snd_pcm_ioplug_set_param_list(io, SND_PCM_IOPLUG_HW_ACCESS as i32, 2, [SND_PCM_ACCESS_MMAP_INTERLEAVED as u32, SND_PCM_ACCESS_MMAP_NONINTERLEAVED as u32].as_ptr());

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