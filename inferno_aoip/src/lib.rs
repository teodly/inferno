//! Inferno - unofficial implemention of the Dante protocol (Audio over IP)
//! 
//! Currently this library emulates a Dante device with receive channels,
//! which can be connected to transmitters using Dante Controller (official,
//! proprietary closed source) or
//! `[network-audio-controller](https://github.com/chris-ritsen/network-audio-controller)`
//! (unofficial, open source)
//! 
//! For example, this will show peak levels of incoming audio samples:
//! ```
//! use inferno_aoip::{Sample, DeviceInfo, DeviceServer, SelfInfoBuilder};
//! 
//! fn audio_callback(samples_count: usize, channels: &Vec<Vec<Sample>>) {
//!   let line = channels.iter().map(|ch| {
//!     let peak = (ch.iter().take(samples_count).map(|samp|
//!       samp.saturating_abs()).max().unwrap_or(0) as f32
//!     ) / (Sample::MAX as f32);
//!     if peak > 0.0 {
//!       let db = 20.0 * peak.log10();
//!       format!("{db:>+6.1} ")
//!     } else {
//!       format!("------ ")
//!     }
//!   }).collect::<String>();
//!   println!("{line}");
//! }
//! 
//! #[tokio::main(flavor = "current_thread")]
//! async fn main() {
//!   let self_info = DeviceInfo::new_self("My Inferno device", "MyInferno", None).make_rx_channels(16);
//!   let server = DeviceServer::start(self_info, Box::new(audio_callback)).await;
//!   let _ = tokio::signal::ctrl_c().await;
//!   server.shutdown().await;
//! }
//! ```
//! 


mod arc_server;
mod byte_utils;
mod channels_subscriber;
mod cmc_server;
mod common;
mod device_info;
mod device_server;
mod flows_rx;
mod info_mcast_server;
mod mdns_client;
mod mdns_server;
mod net_utils;
mod protocol;
mod samples_collector;
mod state_storage;

pub use common::Sample;
pub use device_info::DeviceInfo;
pub use device_server::{DeviceServer, SelfInfoBuilder};