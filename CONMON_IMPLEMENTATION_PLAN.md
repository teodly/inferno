# Detailed implementation plan: closing the conmon / multi-plane gap

Source of truth: `REA/dante/INFERNO_HANDOFF_REPORT.md` (Rev. 2) + `REA/dante/DANTE_PROTOCOL_RE.md`.
Current Inferno: ARC (4440), flows-control/DBCP (4455), info-multicast (8700/8702/8708),
CMC advertisement (8800 `0x1001`), mDNS (5353). Conmon plane absent.

Status legend: ✅ done · 🟡 in progress · ⬜ not started

---

## Phase 0 — Refusal scaffolding ✅
Files: `arc_server.rs`, `cmc_server.rs`, `flows_control_server.rs`
- Replaced silent unknown arms with `warn!` + `respond_with_code(0x30, &[])`.
Tests: 161 unit tests green.

## Phase 1 — Plane-aware dispatch + legacy `0x1200@8800` 🟡
Files: `proto_cmc.rs`, `cmc_server.rs`
Steps:
1. ✅ Add `LEGACY_START_CODE = 0x1200` constant.
2. ✅ Branch on `request.start_code() == LEGACY_START_CODE` before opcode2 check.
3. ✅ Log as known variant; reply `0x30`.
4. ✅ Parse trailing 16 bytes into two `(ipv4, port, reserved)` records; send to embedded addresses.
5. ⬜ Fallback to socket peer when self-record absent (done: `respond_with_code`).
6. ⬜ Golden fixture: replay decoded Yamaha probe; assert no `unknown opcode` log and replies to 8706 on both NICs.
7. ⬜ Extend to reuse paged `get_receive_channels` writer in info-plane dialect for a complete probe response.
Tests: add `legacy_plane_parses_embedded_addresses`, `legacy_plane_falls_back`.

## Phase 2 — Conmon protocol module skeleton 🟡
Files: `src/protocol/proto_conmon.rs`, `src/protocol/mod.rs`
Steps:
1. ✅ Module registered.
2. ✅ Message-id constants (full catalog §16.10).
3. ⬜ Define `ConmonHeader { start_code, total_length, seqnum, opcode, direction, content }`.
4. ⬜ Direction enum: Request/Response/Event.
5. ⬜ Extension TLV enum: FROM, RETURN_FROM, RETURNED_VIA, CUSTOM, SOCKADDR, DDP_ADDR.
6. ⬜ Error vocabulary constants: NO_ERROR, ERROR_OTHER, ERROR_UNSUPPORTED_OPCODE, ERROR_UNSUPPORTED_VERSION, ERROR_INVALID_FORMAT, ERROR_REBOOT_REQUIRED + dapi errors.
7. ⬜ Serde round-trip tests for header + each extension.

## Phase 3 — Conmon server task (refusal-first) ⬜
Files: new `src/device_server/conmon_server.rs`, `mod.rs`
Steps:
1. Create `run_server(self_info, shutdown)` binding UDP 8800 (or a dedicated conmon port) + multicast 224.0.0.232.
2. `req_resp::Connection`-style recv loop; dispatch on `(start_code, opcode)`.
3. Default arm: `warn!` + `respond_with_code(ERROR_UNSUPPORTED_OPCODE)`.
4. Per-instance seqnum tracking + drop log `[drop packet] inst=%s, seqnum=%u`.
5. Opcode-after-seqnum mismatch detection (`Response seqnum=… has opcode …, expected …`).
6. Supervise via `mod.rs::supervise`.
7. Wire into `mod.rs` server list (feature-gated?).
Tests: `conmon_unknown_opcode_replies_unsupported`, `conmon_drop_packet_logged`, `conmon_seqnum_mismatch`.

## Phase 4 — Capability word from runtime feature table (W4) ⬜
Files: `info_mcast_server.rs`
Steps:
1. Replace `BOARD_PRIMARY_CAPABILITIES` const with a function `primary_capabilities(features: &FeatureTable) -> u32`.
2. Define `FeatureTable { metering, serial_slots, rate_writable, encoding_writable, identify, lock, leds, gpio, static_ip, aes67, has_ddp, … }`.
3. Split into `primary / readonly / domain / inferred` words.
4. Unit tests: feature combinations produce expected bits; honesty test (no `HAS_LEDS` unless implemented).
Tests: `capability_word_only_advertises_implemented`.

## Phase 5 — Metering re-framing (W5) ⬜
Files: `peaks.rs`, `info_mcast_server.rs`, `proto_conmon.rs`
Steps:
1. Per-channel level as `%4.1f` (f32), CLIP/MUTE/SOM bitfield.
2. Configurable `update_rate`, `peak_holdoff`, `peak_decay` floats (from settings).
3. Emit conmon `METERING_STATUS` instead of info-plane blob.
4. Settings keys: `METERING_UPDATE_RATE`, `METERING_PEAK_HOLDOFF`, `METERING_PEAK_DECAY`.
Tests: `metering_status_bit_positions`, `metering_config_roundtrip`.

## Phase 6 — Generic serial bridge (W9) ⬜
Files: new `src/device_server/serial_bridge.rs`, `proto_conmon.rs`
Steps:
1. Host-provided TTY/FIFO path (like `LATENCY_REQUEST_PATH`): `SERIAL_PORT_PATH`.
2. Status message: mode, baud, bits, parity, stop_bits, configurable; counters recv/sent/checksum/timeout.
3. Control message: set mode/baud/etc; modes UNATTACHED/CONSOLE/BRIDGE/METERING.
4. Slot descriptors: `SLOT_DB9/NETWORK_SLOT/NETWORK_DB9`.
5. Checksum framing + per-message seqnum + drop tracking.
6. Advertise `HAS_SERIAL_PORTS` only after this lands.
Tests: `serial_status_roundtrip`, `serial_control_applies`, `serial_checksum_drop`.

## Phase 7 — Sample-rate & encoding write path (W6) ⬜
Files: `settings.rs`, `arc_server.rs`, `proto_arc.rs`
Steps:
1. Add `SRATE_MODE_IMMEDIATE/REBOOT`, `ENC_MODE_IMMEDIATE/REBOOT` constants.
2. Encodings: `PCM_16/24/32`, `ENC_PRIVATE32_1/2/3`.
3. Extend 0x1100–0x1102 property table with writable entries; honor current/on-reboot duality.
4. On rate mismatch use DBCP `SampleRateMismatch`.
Tests: `srate_write_immediate`, `srate_write_reboot`, `encoding_list`.

## Phase 8 — Identity honesty (W8) ⬜
Files: `arc_server.rs`, `info_mcast_server.rs`
Steps:
1. Reply to `IDENTIFY_QUERY` (event or no-op).
2. Do not advertise `HAS_LEDS/HAS_GPIO/HAS_SWITCH/…` until implemented.
3. LED model (`LED_STATUS/QUERY` 0x1000/0x1004) with type/colour/state triples only if `HAS_LEDS`.
Tests: `identify_query_reply`, `capability_honesty`.

## Phase 9 — Restart / factory / upgrade refusals (W7) ⬜
Files: `arc_server.rs`
Steps:
1. `SYS_RESET`/factory → `NOTSUPPORTED`-family or host-restarter mirroring `LATENCY_REQUEST_PATH`; modes `RESET_FLAGS_SOFT/FACTORY`, apply `Fixed/Reboot/Immediate`.
2. Upgrades → `reboot required before starting upgrade` class codes.
3. Refuse `UPGRADE_V3_CONTROL` with appropriate code.
Tests: `sys_reset_refusal`, `upgrade_refusal`.

## Phase 10 — DDP vocabulary alignment (W3) ⬜
Files: `proto_arc.rs`, `req_resp.rs`, all servers
Steps:
1. Replace ad-hoc `0x30`/`0xFFFF` with `ERROR_*` constants.
2. Align dapi error codes.
3. Access-control gating by privilege level.
Tests: `refusal_uses_error_vocabulary`.

## Phase 11 — ARCP / flows extra fields (§16.3) ⬜
Files: `proto_arc.rs`, `flows_control.rs`
Steps:
1. Add RX-flow record fields: transport proto, flowsub, AES67 clock offset, AES67 SDP id, actual FPP, interface status (AES67 fields reserved/excluded).
2. APCP bundle handling (optional).

## Phase 12 — APEC3 / clocking / discovery / endpoints (§16.4–16.8) ⬜
Steps:
1. Pull-ups + IMMEDIATE/REBOOT for srate/enc (Phase 7).
2. Additional mDNS service types.
3. Multicast groups 224.0.0.230/232, 239.255.255.255, 239.69.0.0.
4. PTP state reporting surface (delegated to statime; document only).

## Phase 13 — Golden fixtures & CI (§14) ⬜
Steps:
1. Save decoded field logs as fixtures in `test/`.
2. Replay harness against servers; assert response shapes.
3. CI gate: `cargo test -p inferno_aoip --lib` + fixture replay.

## Cross-cutting
- Every phase keeps `cargo test -p inferno_aoip --lib` green.
- New code paths get unit tests for edge cases before merge.
- Update `INFERNO_HANDOFF_REPORT.md` §16 as items land (move to "implemented").

## Effort (from handoff §13)
S ≈ one session, M ≈ a few. Phases 0–2/4/8/9 are S; 1/5/6/7/10 are M; 3/11/12/13 are M–L.
