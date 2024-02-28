// Inferno-AoIP
// Copyright (C) 2023 Teodor Woźniak
// 
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
// 
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
// 
// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <http://www.gnu.org/licenses/>.


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
mod flows_control_server;
mod flows_rx;
mod flows_tx;
mod info_mcast_server;
mod mdns_client;
mod mdns_server;
mod media_clock;
mod net_utils;
mod os_utils;
mod protocol;
mod real_time_box_channel;
mod samples_collector;
mod samples_utils;
mod state_storage;
mod thread_utils;

pub use common::Sample;
pub use device_info::DeviceInfo;
pub use device_server::{DeviceServer, SelfInfoBuilder};
pub use media_clock::MediaClock;

pub mod utils {
  pub use crate::os_utils::set_current_thread_realtime;
  pub use crate::thread_utils::run_future_in_new_thread;
  pub use crate::common::LogAndForget;
}
