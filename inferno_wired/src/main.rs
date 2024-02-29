// Based on pipewire-rs/pipewire/examples/{audio-capture.rs, tone.rs}
// Copyright The pipewire-rs Contributors.
// Original license: SPDX-License-Identifier: MIT

use clap::Parser;
use inferno_aoip::utils::{run_future_in_new_thread, set_current_thread_realtime, LogAndForget};
use inferno_aoip::{DeviceInfo, DeviceServer, MediaClock, SelfInfoBuilder};
use pipewire as pw;
use pw::loop_::TimerSource;
use pw::main_loop::MainLoop;
use pw::stream::{Stream, StreamListener, StreamRef};
use pw::{properties::properties, spa};
use spa::param::format::{MediaSubtype, MediaType};
use spa::param::format_utils;
use spa::pod::Pod;
#[cfg(feature = "v0_3_44")]
use spa::WritableDict;
use std::convert::TryInto;
use std::ops::Deref;
use std::thread::sleep;
use std::{mem, thread};
use std::sync::atomic::{AtomicIsize, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use futures_util::FutureExt;

struct UserData {
    format: spa::param::audio::AudioInfoRaw
}

struct StreamState {
    frames_per_callback: usize,
    next_ts: AtomicUsize,
    to_catchup: AtomicIsize,
    remaining_process: AtomicIsize
}

#[derive(Parser)]
#[clap(name = "inferno_wired", about = "Inferno Wired: unofficial implementation of Dante protocol - PipeWire virtual device")]
struct Opt {
    #[clap(short, long, help = "The target object id to connect to")]
    target: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Direction {
    FromNetwork,
    ToNetwork,
}

impl Direction {
    fn select<T>(&self, from_network: T, to_network: T) -> T {
        match self {
            Self::FromNetwork => from_network,
            Self::ToNetwork => to_network,
        }
    }
}

fn create_stream<F>(dir: Direction, core: &pw::core::Core, app_name: &str, sample_rate: u32, num_channels: u32, state: Arc<StreamState>, mut process_channel_cb: F) -> Result<(Stream, StreamListener<UserData>), pw::Error>
where F: FnMut(usize, usize, &mut [i32]) + 'static {
    let data = UserData {
        format: Default::default()
    };

    /* Create a simple stream, the simple stream manages the core and remote
     * objects for you if you don't need to deal with them.
     *
     * If you plan to autoconnect your stream, you need to provide at least
     * media, category and role properties.
     *
     * Pass your events and a user_data pointer as the last arguments. This
     * will inform you about the stream state. The most important event
     * you need to listen to is the process event where you need to produce
     * the data.
     */
    let props = properties! {
        "clock.name" => dir.select("network.inferno.from_network", "network.inferno.to_network"),
        "clock.id" => dir.select("network.inferno.from_network", "network.inferno.to_network"),
        *pw::keys::MEDIA_TYPE => "Audio",
        *pw::keys::MEDIA_CATEGORY => dir.select("Playback", "Capture"),
        *pw::keys::MEDIA_ROLE => "Production",
        *pw::keys::MEDIA_CLASS => dir.select("Audio/Source", "Audio/Sink"),
        *pw::keys::NODE_ALWAYS_PROCESS => "true",
        *pw::keys::NODE_NAME => format!("{} ({} network)", app_name, dir.select("from", "to")),
        *pw::keys::NODE_RATE => format!("1/{sample_rate}"),
        *pw::keys::NODE_FORCE_RATE => "0",
        *pw::keys::NODE_FORCE_QUANTUM => format!("{}", state.frames_per_callback),
        // TODO figure out how to set up resampling as PipeWire does with ALSA, instead of forcing to make us the driver
        // anyway, in most cases we SHOULD be a driver because Dante devices are always pro-audio so we don't want resampling
        *pw::keys::PRIORITY_DRIVER => dir.select("9000", "8000"),
        *pw::keys::PORT_PHYSICAL => "true", // dunno whether it is needed
        *pw::keys::PORT_TERMINAL => "true",
    };

    let stream = pw::stream::Stream::new(&core, dir.select("from_network", "to_network"), props)?;

    let inner_dir = dir;
    let listener = stream
        .add_local_listener_with_user_data(data)
        .param_changed(move |_, user_data, id, param| {
            log::trace!("param_changed");
            // NULL means to clear the format
            let Some(param) = param else {
                return;
            };
            if id != pw::spa::param::ParamType::Format.as_raw() {
                return;
            }
            log::trace!("format changed");

            let (media_type, media_subtype) = match format_utils::parse_format(param) {
                Ok(v) => v,
                Err(_) => return,
            };

            // only accept raw audio
            if media_type != MediaType::Audio || media_subtype != MediaSubtype::Raw {
                return;
            }

            // call a helper function to parse the format for us.
            user_data
                .format
                .parse(param)
                .expect("Failed to parse param changed to AudioInfoRaw");

            log::info!(
                "pipewire gives us {inner_dir:?} stream: rate:{} channels:{}",
                user_data.format.rate(),
                user_data.format.channels()
            );
        })
        .process(move |stream, user_data| {
            state.remaining_process.fetch_sub(1, Ordering::AcqRel);
            match stream.dequeue_buffer() {
                None => {
                    log::warn!("{dir:?} pipewire out of buffers (this is probably harmless)");
                },
                Some(mut buffer) => {
                    let datas = buffer.datas_mut();
                    if datas.is_empty() {
                        return;
                    }
                    let n_channels = user_data.format.channels();
                    
                    let n_samples_opt = match dir {
                        Direction::FromNetwork => datas.iter_mut().take(n_channels as usize).filter_map(|data|data.data()).map(|bytes|bytes.len()/mem::size_of::<i32>()).min(),
                        Direction::ToNetwork => datas.iter_mut().take(n_channels as usize).map(|data|data.chunk().size() as usize/mem::size_of::<i32>()).min()
                    };
                    let max_n_samples = n_samples_opt.unwrap_or(0);
                    let mut n_samples = match dir {
                        Direction::FromNetwork => max_n_samples.min(state.frames_per_callback),
                        Direction::ToNetwork => max_n_samples
                    };
                    if n_samples == 0 {
                        log::warn!("{dir:?} pipewire gave us buffer of 0 frames");
                        return;
                    } else if n_samples != state.frames_per_callback {
                        log::warn!("{dir:?} pipewire gave us buffer of {n_samples} frames but we want {}", state.frames_per_callback);
                    }
                    let to_catchup = state.to_catchup.load(Ordering::Acquire);
                    if to_catchup > 0 {
                        let add_samples = to_catchup.min(state.frames_per_callback as isize);
                        state.to_catchup.fetch_sub(add_samples, Ordering::AcqRel);
                        log::warn!("{dir:?} catching up: before add: {n_samples}");
                        n_samples = (n_samples + add_samples as usize).min(max_n_samples);
                        log::warn!("{dir:?} catching up: after add: {n_samples}");
                    }
                    
                    let start_ts = state.next_ts.fetch_add(n_samples, Ordering::AcqRel);
                    
                    for c in 0..n_channels {
                        let data = &mut datas[c as usize];
                        if let Some(bytes) = data.data() {
                            // safety: [u8] can be always losslessly reinterpreted as [i32]
                            let (left, samples, _right) = unsafe { bytes.align_to_mut::<i32>() };
                            assert_eq!(left.len(), 0, "{dir:?} got unaligned buffer from pipewire");
        
                            process_channel_cb(start_ts, c as usize, &mut samples[0..n_samples]);
                        }
                        if dir == Direction::FromNetwork {
                            let chunk = data.chunk_mut();
                            *chunk.offset_mut() = 0;
                            *chunk.stride_mut() = mem::size_of::<i32>() as _;
                            *chunk.size_mut() = (mem::size_of::<i32>() * n_samples) as _;
                        }
                    }
                }
            };
        })
        .register()?;
    
    /* Make one parameter with the supported formats. The SPA_PARAM_EnumFormat
     * id means that this is a format enumeration (of 1 value).
     * We leave the channels and rate empty to accept the native graph
     * rate and channels. */
    let mut audio_info = spa::param::audio::AudioInfoRaw::new();
    audio_info.set_format(spa::param::audio::AudioFormat::S32P);
    audio_info.set_channels(num_channels);
    let obj = pw::spa::pod::Object {
        type_: pw::spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
        id: pw::spa::param::ParamType::EnumFormat.as_raw(),
        properties: audio_info.into(),
    };
    let values: Vec<u8> = pw::spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &pw::spa::pod::Value::Object(obj),
    )
    .unwrap()
    .0
    .into_inner();

    let mut params = [Pod::from_bytes(&values).unwrap()];

    /* Now connect this stream. We ask that our process function is
     * called in a realtime thread. */
    stream.connect(
        dir.select(spa::utils::Direction::Output, spa::utils::Direction::Input),
        None,
            pw::stream::StreamFlags::MAP_BUFFERS | 
            pw::stream::StreamFlags::RT_PROCESS |
            //| pw::stream::StreamFlags::DRIVER,
            dir.select(
                pw::stream::StreamFlags::DRIVER /*| pw::stream::StreamFlags::TRIGGER*/,
                pw::stream::StreamFlags::DRIVER /*| pw::stream::StreamFlags::TRIGGER*/
            ),
        &mut params,
    )?;
    Ok((stream, listener))
}


pub fn main() -> Result<(), pw::Error> {
    let logenv = env_logger::Env::default().default_filter_or("debug");
    env_logger::builder().parse_env(logenv).format_timestamp_micros().init();

    let self_info = DeviceInfo::new_self("Inferno Wired", "InfernoWired", None).make_rx_channels(4).make_tx_channels(32);
    let tx_channels = self_info.tx_channels.len();
    let rx_channels = self_info.rx_channels.len();
    let sample_rate = self_info.sample_rate;

    let (tx, rx) = std::sync::mpsc::channel();

    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();

    let inferno_thread = run_future_in_new_thread("Inferno main", move || async move {
        let (mut device_server, samples_receiver, clock_receiver) = DeviceServer::start_with_realtime_receiver(self_info).await;
        let tx_inputs = device_server.take_tx_inputs();
        let self_info = device_server.self_info.clone();
        tx.send((self_info, samples_receiver, clock_receiver, tx_inputs)).log_and_forget();
        stop_rx.await;
        device_server.shutdown().await;
    }.boxed_local());

    let (self_info, mut samples_receiver, mut clock_receiver, mut tx_inputs) = rx.recv().unwrap();
    
    pw::init();

    let props = properties! {
        *pw::keys::CONFIG_NAME => "client-rt.conf",
    };
    let mainloop = pw::main_loop::MainLoop::new(None)?;
    let context = pw::context::Context::with_properties(&mainloop, props)?;
    let core = context.connect(None)?;

    let frames_per_callback = 256;
    let next_ts = loop {
        if let Some(now) = samples_receiver.clock().now_in_timebase(sample_rate as u64) {
            break now;
        } else {
            log::warn!("waiting for clock");
            std::thread::sleep(Duration::from_secs(1));
        }
    };

    let from_network_state = Arc::new(StreamState {
        next_ts: next_ts.into(),
        to_catchup: 0.into(),
        remaining_process: 0.into(),
        frames_per_callback
    });

    let from_network_stream = match create_stream(Direction::FromNetwork, &core, &self_info.friendly_hostname, sample_rate, rx_channels as u32, from_network_state.clone(), move |start_ts, channel_index, mut samples| {
        samples_receiver.get_samples(start_ts, channel_index, &mut samples);
    }) {
        Ok((stream, stream_listener)) => {
            Box::leak(Box::new(stream_listener));
            stream
        },
        Err(e) => {
            log::error!("failed to create playback stream (from network): {e:?}");
            return Err(e);
        }
    };

    let to_network_state = Arc::new(StreamState {
        next_ts: next_ts.into(),
        to_catchup: 0.into(),
        remaining_process: 0.into(),
        frames_per_callback
    });
    let to_network_stream = match create_stream(Direction::ToNetwork, &core, &self_info.friendly_hostname, sample_rate, tx_channels as u32, to_network_state.clone(), move |start_ts, channel_index, samples| {
        tx_inputs[channel_index].write_from_at(start_ts, samples[0..].into_iter().map(|e|*e));
    }) {
        Ok((stream, stream_listener)) => {
            Box::leak(Box::new(stream_listener));
            stream
        },
        Err(e) => {
            log::error!("failed to create playback stream (from network): {e:?}");
            return Err(e);
        }
    };
    
    //let (trig_sender, trig_recv) = pw::channel::channel();
    
    /* let _receiver = trig_recv.attach(mainloop.loop_(), {
        move |(from_network, to_network)| {
            if from_network {
                from_network_stream.trigger_process();
            }
            if to_network {
                to_network_stream.trigger_process();
            }
        }
    }); */

    thread::scope(|s| {
        let from_network_stream = from_network_stream.deref();
        let to_network_stream = to_network_stream.deref();
        
        std::thread::Builder::new().name("pw trigger".to_owned()).spawn_scoped(s, move || {
            #[inline(always)]
            fn handle_state(dir: Direction, now: usize, state: &StreamState) -> isize {
                let remaining = state.remaining_process.load(Ordering::Acquire); // FIXME it doesn't work
                /*if remaining < 0 {
                    log::error!("BUG: pipewire process called more times than trigger_process: {remaining}");
                }*/ // FIXME dunno why it happens but it spams the log
                //log::debug!("remaining {remaining}");
                let waiting = false; //remaining > 0;
                let start_ts = state.next_ts.load(Ordering::Acquire);
                let drift = now.wrapping_sub(start_ts) as isize;
                let mut trig_count = 0;
                /*if drift.abs() > state.frames_per_callback as isize {
                    if !waiting {
                        log::warn!("pipewire is calling us with drift {drift} samples ({})", if drift<0 {"too early"} else {"too late"});
                    }
                }*/
                let too_late = match dir {
                    Direction::FromNetwork => drift > (state.frames_per_callback as isize)*2,
                    Direction::ToNetwork => drift > (state.frames_per_callback as isize)
                };
                if drift.abs() > 48000/4 {
                    log::error!("{dir:?} large clock drift detected, dropping some samples!");
                    state.next_ts.store(now, Ordering::Release);
                } else if (!waiting) && too_late {
                    log::warn!("{dir:?} catching up");
                    //state.to_catchup.fetch_add(((drift as usize / state.frames_per_callback - 1) * state.frames_per_callback) as isize, Ordering::AcqRel);
                    //state.to_catchup.fetch_add((drift as usize - state.frames_per_callback) as isize, Ordering::AcqRel);
                    trig_count += 1;
                }
                let too_early = (!too_late) && match dir {
                    Direction::FromNetwork => drift < 0 /* (state.frames_per_callback as isize) */,
                    Direction::ToNetwork => drift < -(state.frames_per_callback as isize)
                };
                if !too_early {
                    // do not call if realtime thread is too early - we may try to dequeue not yet ready samples then
                    trig_count += 1;
                }
                state.remaining_process.fetch_add(trig_count, Ordering::AcqRel);
                return trig_count
            }
            let mut clock = MediaClock::new();
            while !clock_receiver.update() {
                sleep(Duration::from_secs(1));
                log::warn!("trigger thread: waiting for clock");
            }
            clock.update_overlay(clock_receiver.get().unwrap().clone());

            set_current_thread_realtime(88); // TODO dehardcode

            let mut next_wakeup = clock.now_in_timebase(sample_rate as u64).unwrap();
            
            loop {
                if clock_receiver.update() {
                    if let Some(ovl) = clock_receiver.get() {
                        clock.update_overlay(ovl.clone());  
                    }
                }
                if let Some(now) = clock.now_in_timebase(sample_rate as u64) {
                    let drift = (now as isize).wrapping_sub(next_wakeup as isize);
                    //log::debug!("now: {now}, next: {next_wakeup}, drift: {drift}");
                    if drift.abs() > sample_rate as isize / 2 {
                        log::error!("trigger thread: clock jumped {drift} samples");
                        next_wakeup = clock.now_in_timebase(sample_rate as u64).unwrap();
                    }

                    let from_network_count = handle_state(Direction::FromNetwork, now, &from_network_state);
                    let to_network_count = handle_state(Direction::ToNetwork, now, &to_network_state);

                    if from_network_count > 0 {
                        from_network_stream.trigger_process();
                    }
                    if to_network_count > 0 {
                        to_network_stream.trigger_process();
                    }
                    let accel = from_network_count > 1 || to_network_count > 1;

                    if accel {
                        let half_wakeup = next_wakeup.wrapping_add(frames_per_callback/2);
                        sleep(clock.system_clock_duration_until(half_wakeup, sample_rate as u64).unwrap());
                        if from_network_count > 1 {
                            from_network_stream.trigger_process();
                        }
                        if to_network_count > 1 {
                            to_network_stream.trigger_process();
                        }
                    }

                    next_wakeup = next_wakeup.wrapping_add(frames_per_callback);

                    let before_sleep = clock.now_in_timebase(sample_rate as u64).unwrap();
                    let sleep_duration = clock.system_clock_duration_until(next_wakeup, sample_rate as u64).unwrap();
                    sleep(sleep_duration);
                    let after_sleep = clock.now_in_timebase(sample_rate as u64).unwrap();
                    //log::debug!("slept {} samples, wanted {sleep_duration:?}", after_sleep-before_sleep);
                } else {
                    sleep(Duration::from_secs(1));
                }
            }
        }).unwrap();

        log::info!("starting main loop");
        mainloop.run();
    });

    stop_tx.send(()).unwrap();

    inferno_thread.join().unwrap();

    Ok(())
}
