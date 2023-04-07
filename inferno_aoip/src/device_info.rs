use std::net::Ipv4Addr;

pub struct Channel {
  pub factory_name: String,
  pub friendly_name: String,
}

pub struct DeviceInfo {
  pub ip_address: Ipv4Addr,
  pub board_name: String,
  pub manufacturer: String,
  pub model_name: String,
  pub model_number: String, // _000000000000000b
  pub factory_device_id: [u8; 8],
  pub vendor_string: String,
  pub friendly_hostname: String,
  pub factory_hostname: String,
  pub rx_channels: Vec<Channel>,
  pub tx_channels: Vec<Channel>,
  pub pcm_type: u8, // usually 0xe, in older devices 4
  pub sample_rate: u32,
}
