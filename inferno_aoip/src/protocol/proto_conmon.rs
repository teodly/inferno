//! Conmon (Connection & Monitoring) message identifiers.
//!
//! Sourced from the firmware string/rodata catalog (`REA/dante/DANTE_PROTOCOL_RE.md`
//! §4.2 and §16.10). Inferno does not yet implement the conmon plane; these
//! constants exist so the dispatch table and refusal paths can name the ids.

pub const PORT: u16 = 8800;

// conmon message ids (16-bit). Direction is a separate field in the header.
pub const AUDIO_INTERFACE_STATUS: u16 = 0x0040;
pub const NAME_ID_STATUS: u16 = 0x0062;
pub const NAME_ID_CONTROL: u16 = 0x0063;
pub const MASTER_STATUS: u16 = 0x0080;
pub const MASTER_QUERY: u16 = 0x0084;
pub const IDENTIFY_QUERY: u16 = 0x0086;
pub const SRATE_STATUS: u16 = 0x0050; // placeholder: exact id unconfirmed
pub const SRATE_PULLUP_STATUS: u16 = 0x0051; // placeholder
pub const ENC_STATUS: u16 = 0x0052; // placeholder
pub const RX_CHANNEL_CHANGE: u16 = 0x0053; // placeholder
pub const RX_CHANNEL_RX_ERROR: u16 = 0x0054; // placeholder
pub const IFSTATS_STATUS: u16 = 0x0060;
pub const IFSTATS_QUERY: u16 = 0x0061;
pub const IGMP_VERS_STATUS: u16 = 0x0070;
pub const IGMP_VERS_CONTROL: u16 = 0x0071;
pub const VERSIONS_STATUS: u16 = 0x0073;
pub const VERSIONS_QUERY: u16 = 0x0075;
pub const UPGRADE_V3_CONTROL: u16 = 0x0090;
pub const UPGRADE_STATUS: u16 = 0x00a0;
pub const CLEAR_CONFIG_STATUS: u16 = 0x00a1;
pub const CONFIG_STATUS: u16 = 0x00b0;
pub const CONFIG_CONTROL: u16 = 0x00b1;
pub const CLOCKING_STATUS: u16 = 0x0026;
pub const CLOCKING_CONTROL: u16 = 0x0027;
pub const UNICAST_CLOCKING_STATUS: u16 = 0x0022;
pub const UNICAST_CLOCKING_QUERY: u16 = 0x0023;
pub const SYS_RESET: u16 = 0x00c0;
pub const EDK_BOARD_STATUS: u16 = 0x0101;
pub const EDK_BOARD_CONTROL: u16 = 0x0102;
pub const ACCESS_STATUS: u16 = 0x0103;
pub const ACCESS_CONTROL: u16 = 0x0104;
pub const MANF_VERSIONS_STATUS: u16 = 0x0103;
pub const MANF_VERSIONS_QUERY: u16 = 0x0108;
pub const TX_CHANNEL_CHANGE: u16 = 0x0109;
pub const TX_LABEL_CHANGE: u16 = 0x0108;
pub const RX_FLOW_CHANGE: u16 = 0x0109;
pub const TX_FLOW_CHANGE: u16 = 0x010a;
pub const PROPERTY_CHANGE: u16 = 0x00e0;
pub const RX_FLOW_ERROR_CHANGE: u16 = 0x00f1;
pub const TX_SIGNAL_REFLEVEL_CHANGE: u16 = 0x0110;
pub const RX_SIGNAL_REFLEVEL_CHANGE: u16 = 0x0112;
pub const ROUTING_READY: u16 = 0x0120;
pub const RX_ERROR_THRES_STATUS: u16 = 0x00d0;
pub const RX_ERROR_THRES_CONTROL: u16 = 0x00d1;
pub const ROUTING_DEVICE_CHANGE: u16 = 0x00f0;
pub const METERING_STATUS: u16 = 0x0060;
pub const LED_STATUS: u16 = 0x1000;
pub const LED_QUERY: u16 = 0x1004;
pub const SERIAL_PORT_STATUS: u16 = 0x0130;
pub const SERIAL_PORT_CONTROL: u16 = 0x0131;
pub const HAREMOTE_STATUS: u16 = 0x0140;
pub const HAREMOTE_STATS_STATUS: u16 = 0x0141;
pub const DANTE_READY: u16 = 0x9000;
pub const DANTE_READY_QUERY: u16 = 0x9004;
pub const PTP_LOGGING_STATUS: u16 = 0x9008;
pub const SYS_RESET_STATUS: u16 = 0x00c1;
pub const GPIO_STATUS: u16 = 0x0142;
pub const AES67_STATUS: u16 = 0x0143;
pub const LOCK_STATUS: u16 = 0x0144;
pub const TX_CHANNEL_LABEL_CHANGE: u16 = 0x0108;
pub const TOPOLOGY_CHANGE: u16 = 0x0020;
pub const SWITCH_VLAN_STATUS: u16 = 0x0021;
pub const SWITCH_VLAN_CONTROL: u16 = 0x0024;
pub const INTERFACE_STATUS: u16 = 0x0025;
pub const PROPERTY_CHANGE_ALT: u16 = 0x00e0;
pub const METERING_CONTROL: u16 = 0x0061;
pub const IDENTIFY_STATUS: u16 = 0x0082;

// DDP error vocabulary (handoff W3 / DANTE_PROTOCOL_RE.md §4.1).
pub const NO_ERROR: u16 = 0;
pub const ERROR_OTHER: u16 = 1;
pub const ERROR_UNSUPPORTED_OPCODE: u16 = 2;
pub const ERROR_UNSUPPORTED_VERSION: u16 = 3;
pub const ERROR_INVALID_FORMAT: u16 = 4;
pub const ERROR_REBOOT_REQUIRED: u16 = 5;

// Direction field values.
pub const DIR_REQUEST: u8 = 0;
pub const DIR_RESPONSE: u8 = 1;
pub const DIR_EVENT: u8 = 2;

// Sample-rate / encoding update modes (handoff W6).
pub const SRATE_MODE_IMMEDIATE: u16 = 0;
pub const SRATE_MODE_REBOOT: u16 = 1;
pub const ENC_MODE_IMMEDIATE: u16 = 0;
pub const ENC_MODE_REBOOT: u16 = 1;

// Reset flags (handoff W7).
pub const RESET_FLAGS_SOFT: u16 = 0;
pub const RESET_FLAGS_FACTORY: u16 = 1;


// Encodings (handoff W6).
pub const ENC_PCM_16: u32 = 16;
pub const ENC_PCM_24: u32 = 24;
pub const ENC_PCM_32: u32 = 32;
pub const ENC_PRIVATE32_1: u32 = 0x8000_0001;
pub const ENC_PRIVATE32_2: u32 = 0x8000_0002;
pub const ENC_PRIVATE32_3: u32 = 0x8000_0003;


// Serial bridge (handoff W9).
pub const UART_MODE_NONE: u8 = 0;
pub const UART_MODE_CONSOLE: u8 = 1;
pub const UART_MODE_BRIDGE: u8 = 2;
pub const UART_MODE_METER: u8 = 3;

pub const SLOT_DB9: u8 = 0;
pub const SLOT_NETWORK: u8 = 1;
pub const SLOT_NETWORK_DB9: u8 = 2;
