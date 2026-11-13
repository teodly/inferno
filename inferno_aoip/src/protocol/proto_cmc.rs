use crate::device_info::DeviceId;

pub const PORT: u16 = 8800;

/// Legacy discovery plane start code observed in the field (Yamaha DSP probes).
pub const LEGACY_START_CODE: u16 = 0x1200;

#[derive(Debug, binary_serde::BinarySerde, Default)]
pub struct DeviceAdvertisement {
  pub process_id: u16,
  pub factory_device_id: DeviceId,
  pub unknown1_1: u16,
  pub unknown2_0: u16,
  pub ip_address: [u8; 4],
  pub info_request_port: u16,
  pub unknown3_0: u16,
}

pub const REQUEST_DEVICE_ADVERTISEMENT: u16 = 0x1001;

#[cfg(test)]
mod tests {
  use super::*;
  use binary_serde::recursive_array::RecursiveArray;
  use binary_serde::BinarySerde;

  #[test]
  fn device_advertisement_default_is_all_zeros() {
    let default = DeviceAdvertisement::default();
    assert_eq!(default.process_id, 0);
    assert_eq!(default.factory_device_id, [0; 8]);
    assert_eq!(default.unknown1_1, 0);
    assert_eq!(default.unknown2_0, 0);
    assert_eq!(default.ip_address, [0; 4]);
    assert_eq!(default.info_request_port, 0);
    assert_eq!(default.unknown3_0, 0);
  }

  #[test]
  fn device_advertisement_round_trip_default() {
    let original = DeviceAdvertisement::default();
    let bytes = original.binary_serialize_to_array(binary_serde::Endianness::Big);
    let deserialized =
      DeviceAdvertisement::binary_deserialize(bytes.as_slice(), binary_serde::Endianness::Big).unwrap();
    assert_eq!(original.process_id, deserialized.process_id);
    assert_eq!(original.factory_device_id, deserialized.factory_device_id);
    assert_eq!(original.unknown1_1, deserialized.unknown1_1);
    assert_eq!(original.unknown2_0, deserialized.unknown2_0);
    assert_eq!(original.ip_address, deserialized.ip_address);
    assert_eq!(original.info_request_port, deserialized.info_request_port);
    assert_eq!(original.unknown3_0, deserialized.unknown3_0);
  }

  #[test]
  fn device_advertisement_round_trip_non_default() {
    let original = DeviceAdvertisement {
      process_id: 42,
      factory_device_id: [1, 2, 3, 4, 5, 6, 7, 8],
      unknown1_1: 0,
      unknown2_0: 0,
      ip_address: [192, 168, 1, 10],
      info_request_port: 12345,
      unknown3_0: 0,
    };
    let bytes = original.binary_serialize_to_array(binary_serde::Endianness::Big);
    let deserialized =
      DeviceAdvertisement::binary_deserialize(bytes.as_slice(), binary_serde::Endianness::Big).unwrap();
    assert_eq!(original.process_id, deserialized.process_id);
    assert_eq!(original.factory_device_id, deserialized.factory_device_id);
    assert_eq!(original.unknown1_1, deserialized.unknown1_1);
    assert_eq!(original.unknown2_0, deserialized.unknown2_0);
    assert_eq!(original.ip_address, deserialized.ip_address);
    assert_eq!(original.info_request_port, deserialized.info_request_port);
    assert_eq!(original.unknown3_0, deserialized.unknown3_0);
  }
}
