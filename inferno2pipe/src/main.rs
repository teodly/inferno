use std::io::Write;
use std::{env, fs::File, mem::size_of};
use log::{info, error};
use clap::Parser;

use inferno_aoip::{Sample, DeviceInfo, DeviceServer, SelfInfoBuilder};

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
  #[arg(long, short)]
  channels_count: usize,
  #[arg(long, short)]
  output: String,
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
  let logenv = env_logger::Env::default().default_filter_or("debug");
  env_logger::init_from_env(logenv);

  let args = Args::parse();

  let self_info = DeviceInfo::new_self("Inferno2pipe", "Inferno2pipe", None).make_rx_channels(args.channels_count);

  let mut output_file = File::create(args.output).unwrap();
  let mut buffer: Vec<u8> =
    vec![0; self_info.rx_channels.len() * (self_info.sample_rate as usize) * size_of::<Sample>() / 10];
  let write_callback = move |samples_count, channels: &Vec<Vec<Sample>>| {
    let stride = channels.len() * size_of::<Sample>();
    let len = stride * samples_count;
    if len > buffer.len() {
      buffer.resize(len, 0);
      info!("enlarging write buffer to {len}");
    }
    for (chi, ch) in channels.iter().enumerate() {
      let mut bi = chi * size_of::<Sample>();
      for si in 0..samples_count {
        buffer[bi..bi + size_of::<Sample>()].copy_from_slice(&ch[si].to_ne_bytes());
        bi += stride;
      }
    }
    output_file.write_all(&buffer[..len]).unwrap_or_else(|e| error!("error writing output: {e:?}"));
  };

  let server = DeviceServer::start(self_info, Box::new(write_callback)).await;
  let _ = tokio::signal::ctrl_c().await;
  server.shutdown().await;

}
