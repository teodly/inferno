# Inferno - unofficial implementation of Dante protocol

[GitLab](https://gitlab.com/lumifaza/inferno) | [GitHub](https://github.com/teodly/inferno) | [Principal author's website](https://info.lumifaza.org/)

This project is named after [a place](https://en.wikipedia.org/wiki/Inferno_(Dante)) destined for those who create undocumented network protocols.

Highly experimental for now. I don't recommend using it for serious purposes.

However, chances that it'll break already working Dante network are low.

If you know what you're doing and have basic Linux command line experience, it is perfectly usable for non-critical tasks, e.g. listening to music, playing multitracks through mixing console for mixing practice, recording rehearsals.

Big thanks to [Project Pendulum](https://github.com/pendulum-project) (by [Trifecta Tech Foundation](https://trifectatech.org/)) for creating and maintaining [Statime](https://github.com/pendulum-project/statime) and collaboration on features needed for audiovisual networks functionality! Audio transmission would be much more difficult to implement without it.


# Features
* receiving audio from and sending audio to Dante devices and virtual devices
* works with most features of Dante Controller and [network-audio-controller](https://github.com/chris-ritsen/network-audio-controller) (`netaudio` command line tool)

## Comparison with other AoIP virtual soundcards

|   | **Inferno** | [DVS](https://www.getdante.com/products/software-essentials/dante-virtual-soundcard) | [AES67 Linux daemon](https://github.com/bondagit/aes67-linux-daemon) |
|---|---|---|---|
| Maturity | ⏳ Alpha but used in production | ✅ Production-ready | ✅ Probably stable |
| Platforms | Linux  | Mac, Windows  | Linux |
| Supported protocols | Dante | Dante | AES67 |
| Directly supported audio backends | ALSA | CoreAudio, ASIO, WDM | ALSA |
| Works with DAWs | 💣 experimental | ✅ Yes | ✅ Yes |
| Route audio using Dante Controller patchbay | ✅ Yes! | ✅ Yes | 🚫 AES67->Dante only |
| Configurable using Dante Controller | ⏳ A bit (channel names, TX multicasts) | ✅ Yes | 🚫 No |
| Compatible with Dante Domain Manager | 🚫 No | ✅ Yes | 🚫 No (but AES67 integration possible) |
| Supported clock protocols | [PTPv1](https://github.com/teodly/statime/tree/inferno-dev) ☑️, PTPv2 ☑️ | PTPv1 ✅ | PTPv2 ✅ |
| Clock leader | PTPv2 ☑️ via [Statime](https://github.com/pendulum-project/statime) | 🚫 No (but possible in Dante Via) | ☑️ via external daemon |
| Stream audio from/to modern Dante hardware | ✅ Yes | ✅ Yes | ✅ Yes |
| Stream audio from/to DVS, Dante Via & old Dante hardware | ✅ Yes | ✅ Yes | 🚫 No  |
| Stream audio from/to AES67           | 🚫 No  | 🚫 No  | ✅ Yes |
| Minimum latency | as low as your kernel gets | 4ms | ... |
| Sends & receives multicasts | ✅ Yes | ✅ Yes | ✅ Yes |
| OS integration | Entirely user-space | Kernel driver & user-space services | Kernel driver & user-space helper |
| Lightweight recording app | ✅ Yes (Inferno2pipe) | 🚫 No | ☑️ FFmpeg with RTP input does the trick |
| Disk space & RAM usage | 🌱 Low (~12MB RAM) | 🔥 High | 🌱 Low |
| Written in | Rust | C++, Java | C++, C |
| License | 🥰 FOSS, copyleft | 🔒 Closed source | 🥰 FOSS, copyleft |
| [DRM](https://drm.info/what-is-drm.en.html) | 😊 No | 🔒 Activation required, virtual machines banned | 😊 No |
| Price   | Free of charge | 🤑 50-80 USD ... *for a **device driver*** | Free of charge |
| Privacy | 😊 No tracking | 😡 Registration required, telemetry enabled by default | 😊 No tracking |

* ✅ - usable
* 💣 - experimental
* ☑️ - not a part of this software but integration is easily possible
* ⏳ - will be implemented soon (until 2025-06 probably)
* 🚫 - unimplemented and not planned for the near future

## Quirks, read it before using:
* Dante protocol is undocumented. Everything was reverse-engineered or based on other reverse-engineering projects. Some things in implementation were guessed. So while it works with my setup, it may not work with yours.
* Collisions with NTP clock synchronization have been noticed. The built-in clock filter is insufficient. [Fixing it properly is complicated.](https://github.com/pendulum-project/statime/issues/389#issuecomment-2214559362) If your network interface supports hardware timestamping, you may be able to workaround it using `/dev/ptp0` as [`CLOCK_PATH`](#configuration) and a [PTP daemon that does not use global system clock](https://gitlab.freedesktop.org/pipewire/pipewire/-/wikis/AES67#setting-up-ptp-time-sync), but you need to enable PTPv2 (AES67) in one of Dante devices then because ptp4l is not compatible with PTPv1.
  * to fix this in Statime, [PHC-only mode](https://github.com/pendulum-project/statime/issues/517) needs to be implemented for hardware timestamps, and [CLOCK_MONOTONIC-based virtual clock with arbitrary timescale](https://github.com/pendulum-project/statime/issues/389) for software timestamps
* Inferno2pipe is clocked by incoming media flows. When nothing is connected, "time will stop" (i.e. recording will pause) until something is connected again - silence won't be generated unless at least one channel is connected.


# Quick start
1. [Install Rust](https://rustup.rs/)
2. If using a firewall, open UDP ports: 4455, 8700, 4400, 8800 (or others if [`INFERNO_ALT_PORT`](#environment-variables) is specified), 5353. Also, allow incoming UDP traffic from possible transmitters (port numbers are allocated by the OS so can't be known beforehand)
3. Ensure that seccomp, SELinux or other kernel-level security mechanisms are not blocking clock-related syscalls. For example, if you want to use `alsa_pcm_inferno` together with PipeWire, and PipeWire service is managed by systemd, copy [`os_integration/systemd_allow_clock.conf`](os_integration/systemd_allow_clock.conf) to `$HOME/.config/systemd/user/pipewire.service.d/override.conf` (or, if already exists, append it)
4. <s>If wanting to use anything other than Inferno2pipe,</s> clock synchronization daemon is needed. Inferno is compatible with modified [Statime](https://github.com/pendulum-project/statime):
   * currently, Statime is always needed, even for just capturing audio, but it is not by design and will be fixed
   * `git clone --recurse-submodules -b inferno-dev https://github.com/teodly/statime`
   * `cd statime && cargo build`
   * adjust network interface in `inferno-ptpv1.toml`
   * `sudo target/debug/statime -c inferno-ptpv1.toml`
   * Disable global system time synchronization while Inferno is in use (`systemctl stop chronyd.service`)
     * Experimental clock guard has been added to our fork of Inferno, so if you're brave, you can skip it.
5. Clone this repo with `--recursive` option (some dependencies are in submodules)
6. `cd` to the desired program/library directory
   * simple command line audio recorder: [`Inferno2pipe`](inferno2pipe/README.md)
   * virtual soundcard for ALSA: [`alsa_pcm_inferno`](alsa_pcm_inferno/README.md) - also works with PipeWire, should work with JACK (not tested yet)
7. If using `alsa_pcm_inferno`, install alsa dev libraries.
   * `sudo apt install libasound2-dev` on Debian/Ubuntu/Mint
   * `pacman -S alsa-lib` on Arch
   * `dnf install alsa-lib-devel` on Fedora/Centos
8. `cargo build`
9. Follow the instructions in README of the specific program/library

## Cross compiling
If you want to use Inferno on Raspberry Pi or other single-board computer, it is usually faster to compile on your desktop/server/laptop (*host*) and copy resulting binaries to the *target*. Also, you will not run out of SD card space or RAM.

Fortunately, there is **actually zero-setup** tool for this: [`cross`](https://crates.io/crates/cross). Install it and instead of executing `cargo build`, do:

```
cross build --release --target=aarch64-unknown-linux-gnu
```

and you'll find compiled binaries in `target/aarch64-unknown-linux-gnu/release`.

Currently the only dependency needed on the target system is the ALSA library, if using `alsa_pcm_inferno`. Everything else is stored in the binary.

Installing dependencies on your host system is not needed - `cross` does it inside its container according to instructions from `Cargo.toml`.

If you need to compile for a different architecture, add it to `Cargo.toml` (copy-paste the `workspace.metadata.cross.target` section) before running `cross`. It is needed only for `alsa_pcm_inferno`. Statime does not have any shared library dependencies.


## Do you prefer Docker?
This won't work with usual desktop system usage, but if you are packaging an audio application using ALSA, into a Docker container, we've got you covered! See the directory `test/containerized_trx` for inspiration.

Remember that:
* Clock synchronization is still your responsibility. I didn't want to complicate the base image with service management so PTP daemon is not included. `usrvclock-rs` requires the sockets path (including client sockets) to be the same for all containers so a shared volume for `/tmp` is required. Future versions may include Statime as a separate container.
* Dante protocol requires good timing. Allow realtime priorities.
* Network routing (incl. multicast) may be tricky to configure correctly but `--network=host` is your friend. Or bridge the container with your AoIP network. NAT is your enemy.
* To have state persistence, mount `/root/.local/state/inferno_aoip` as a volume (or different user's if you application is not running as root)


# Legal and moral stuff
Disclaimer: Dante uses technology patented by Audinate. This source code may use these patents too. Consult a lawyer if you want to:
* make money of it
* distribute binaries in (or from) a region where software patents apply

This project makes no claim to be either authorized or approved by Audinate.

Please do not use this project to make counterfeit Dante devices/software, it is both immoral and illegal (while Audinate's approach is only immoral). Always specify that the implementation is unofficial and not endorsed by Audinate. Probably you can legally say that it is compatible with a subset of Dante protocol, but IANAL.

## License
This project is dual licensed under the GPLv3-or-later and AGPLv3-or-later. You may choose which license to use, or retain both. For example:

* if you want to integrate it into a project already licensed under the GPL, you have the rights to do so.
* if you want to fork it into something working in cloud (or public Internet in general), it will be beneficial to the Free Software Community to use the AGPL **and remove** GPL when forking.


# Tested with
## Dante devices
* Audinate AVIO
  * AVIO-AES3
  * AVIO-DAI2
  * AVIO-DIOUSBC
* Ben & Fellows 523019 4x4 balanced analog I/O module (based on Dante UltimoX4)
* MUSIC Group
  * Klark Teknik DN32-DANTE (in Behringer X32) (based on Dante Brooklyn II)
  * Behringer Aoip-Dante (in Behringer Wing-Rack) (based on Dante Brooklyn III)
* Orban Optimod 5750 (based on Dante Broadway)
* Soundcraft
  * Vi2000
  * Vi3000
* Allen&Heath
  * SQ-5
  * SQ-6
  * Qu-5D
* Yamaha
  * Dante-MY16-AUD (in Yamaha LS9) (also tested with old 3.x Dante firmware version)
  * RIVAGE PM3
  * DSP-RX
* ESI planet 22c
* Dante Via @ OS X and Windows 11
* Dante Virtual Soundcard @ Windows 10

## Control software
* Dante Controller @ Windows 10, 11
* network-audio-controller

## Host
* x86_64 Linux
  * Arch
  * Ubuntu
  * Fedora
* aarch64 (ARM 64-bit) Linux
  * Raspberry Pi 5 - Raspberry Pi OS & Armbian Bookworm
  * Raspberry Pi 4 (no hardware PTP) - Raspberry Pi OS Lite (64bit)
  * Raspberry Pi Zero 2 W (with USB Ethernet) - Raspberry Pi OS Lite (64bit) - CAUTION build times are long



# Anatomy of the repository
* `inferno_aoip` - main library crate for emulating a Dante audio over IP device. In the future controller functionality will also be implemented. **Start here if you want to develop your app based on Inferno**.
* `inferno2pipe` - capture audio, writing interleaved 32-bit integer samples into an Unix named pipe (or a raw file). Helper script for recording to more convenient format is also provided. **Start here if you want to use Inferno for capturing audio without setting up whole audio stack**
* `alsa_pcm_inferno` - virtual soundcard for ALSA. **Start here if you've ever dreamed of Dante Virtual Soundcard for Linux**
* `searchfire` - fork of [Searchlight](https://github.com/WilliamVenner/searchlight) mDNS crate, modified for compatibility with Dante's mDNS


# Clocking options

## Software timestamping
It is the default option, compatible with all NICs (network cards) and Dante devices. It requires [our fork or the Statime daemon](https://github.com/teodly/statime/tree/inferno-dev).

Change the network interface in `inferno-ptpv1.toml` configuration file and run the daemon using:

```
sudo target/debug/statime -c inferno-ptpv1.toml
```

If you want extra stability, disable time synchronization, usually one of the following commands will suffice:
```
sudo systemctl stop chronyd.service
sudo systemctl stop systemd-timesyncd.service
sudo systemctl stop ntpd.service
```
This should no longer be needed if `virtual-system-clock-base` is set to `monotonic` or anything that starts with `monotonic`, but this fix/workaround is very young so take care.

If you have high enough `TX_LATENCY_NS` and `RX_LATENCY_NS` configured (TODO: how high is enough?), you can try to use `monotonic_coarse` which should reduce CPU load.

You can change the protocol from PTPv1 to PTPv2 in Statime configuration - this allows master operation (currently implemented only for PTPv2) so Inferno can be used even when no physical Dante device is in the network. But then at least one Dante device with AES67 enabled must be present in the network to make Inferno and Dante devices interoperate.

## Hardware timestamping
If your network card supports hardware timestamping (check with `ethtool -T`), you can use its clock directly without relying on system clock. It is more accurate than software timestamping, meaning that you can potentially use lower audio latencies.

Set the Inferno's configuration option `CLOCK_PATH` to PTP clock device path, usually `/dev/ptp0`.

If you require PTPv1 (Dante without AES67), you need to use our fork of Statime because there is no other open source PTPv1 daemon. In `inferno-ptpv1.toml`, set the `hardware-clock` option to `auto` and start the daemon:
```
sudo target/debug/statime -c inferno-ptpv1.toml
```

If you can live with PTPv2, you can use different PTP implementation, e.g. [linuxptp](https://www.linuxptp.org/):
```
sudo ptp4l -i enp0s31f6 -p /dev/ptp0 -l6 -E -H -s -m -4 --priority1 255 --priority2 255 --domainNumber 0 --freq_est_interval=7 --delay_filter_length=240 --dscp_event 46
```

or [unpatched (upstream) Statime](https://github.com/pendulum-project/statime).


Audio packets latency jitter is similar when using Statime and ptp4l, however clock histogram in DC shows that ptp4l is doing more abrupt changes in the long run and Statime is unstable for a few seconds during startup.


Be aware that Inferno queries the clock pretty often, which may put more load on the CPU than using system clock. If you want to avoid it but still have some benefits of hardware timestamping, specify `hardware-clock` in Statime config, but leave Inferno's `CLOCK_PATH` unset. Also read the [Software timestamping](#software-timestamping) section then.

# Configuration
Configuration can be set via:

* environment variables - add `INFERNO_` prefix to the setting name
* ALSA plugin configuration - it is recommended to specify them in your `asoundrc` because too long ALSA device string may be truncated (happens with PipeWire, not sure about other apps)

## Settings
All settings have default values. In a simple setup you should be able to start without specifying any parameters.
* `BIND_IP` - which local IP to bind to. Specifying it may be necessary if you have multiple network interfaces. Alternatively, network interface name may be specified, in that case the first address belonging to the interface will be used.
* `DEVICE_ID` - 16 hexadecimal digits (8 bytes) used as a device ID. Dante devices usually use MAC address padded with zeros. Inferno uses `0000<IP address>0000` by default. Device ID is the storage key when saving state.
* `NAME` - name of advertised device. If unspecified, name based on app name and IP address will be generated. May be removed in future versions when it becomes settable from DC and so stored in a configuration file.
* `SAMPLE_RATE` - sample rate this device will operate on
* `PROCESS_ID` - integer number between 0 and 65535. Must be provided and unique when starting multiple instances on a single IP address. Specifying different `DEVICE_ID`s is not sufficient.
* `ALT_PORT` - start of the range of UDP ports used by socket listeners. If not specified, standard Dante ports as seen in hardware devices will be used. Must be provided when starting multiple instances on a single IP address. Currently 4 ports are used (`ALT_PORT` to `ALT_PORT+3`) but it may change in the future, so better separate different instances by at least 10 ports.
* `RX_CHANNELS` - number of receive channels, defaults to 2, will be overwritten by the application if it supports changing channels count.
* `TX_CHANNELS` - number of transmit channels, defaults to 2, will be overwritten by the application if it supports changing channels count.
* `RX_LATENCY_NS` - receive latency in nanoseconds, i.e. how much time to wait for media packets, relatively to PTP media clock. Equivalent to latency setting in Dante Controller. Defaults to 10ms. May be removed in future versions when it becomes settable from DC and so stored in a configuration file.
* `TX_LATENCY_NS` - transmit latency in nanoseconds, i.e. receive latency that this device will demand from devices receiving from us. Equivalent to latency setting in Dante Virtual Soundcard. Defaults to 10ms.
* `CLOCK_PATH` - path to either [usrvclock](https://gitlab.com/lumifaza/usrvclock) socket, or PTP device. If the latter, make sure [you are allowed](https://gitlab.freedesktop.org/pipewire/pipewire/-/blob/78642cc53bd84c2ad529f2175cc50a658d1e52c0/src/daemon/90-pipewire-aes67-ptp.rules) to read it. Also, write permissions are required if you want to see actual frequency offset in DC. (the clock is never adjusted from within Inferno, but the syscall to read the frequency requires write access)


# Audio stability tips

## Set appropriate latency

Disable Inferno. Run cyclictest for at least 20 minutes, while doing things that you normally do with the computer:

    cyclictest --mlockall --smp --priority=80 --interval=5000 --distance=0

You can also add some artificial system load:

    stress -m `nproc` -c `nproc`

In cyclictest output, the last column is the worst-case latency in microseconds. Multiply it by 1000, add some margin and time for network transfer and you get the lowest safe value for `TX_LATENCY_NS` and `RX_LATENCY_NS`. If the values are higher than your acceptable latency, you will have hard time using Inferno (and computer audio in general). Also keep in mind that the maximum latency that Dante officially allows is 40ms. You can try to follow tutorials to make latency more predictable:

* [Professional audio @ Arch wiki](https://wiki.archlinux.org/title/Professional_audio) (not just for Arch)
* [Performance tuning @ PipeWire wiki](https://wiki.archlinux.org/title/Professional_audio)
* [Fedora Pipewire Low Latency Audio Configuration Reference Guide](https://linuxmusicians.com/viewtopic.php?t=27121) (Fedora-specific)
* isolate CPU cores using `isolcpus` [or `cset`](https://unix.stackexchange.com/a/692558) and pin audio processes and network card IRQ to the isolated cores
* if nothing else helps, try `PREEMPT_RT` kernel


# Contributing
Issue reports and pull requests are welcome.

By submitting any contribution, you agree that it will be distributed according to the comment found at the top of `inferno_aoip/src/lib.rs` file - under the terms of GNU GPL v3 or any later version, or GNU AGPL v3 or any later version.

If you want to fork this project into something working in cloud (or public Internet in general), you may consider removing the GPL license and retaining AGPL.

Please use editor respecting `.editorconfig` (for example, VSCode needs an extension: [EditorConfig for VS Code](https://open-vsx.org/extension/EditorConfig/EditorConfig)) or configure it approprietly manually.

## Dependencies
This project is dependent on Statime ([upstream](https://github.com/pendulum-project/statime), [our fork](https://github.com/teodly/statime/tree/inferno-dev)), so if you want to help, look at its TODO list, too! The most important are:
* [PTPv1 support](https://github.com/pendulum-project/statime/pull/602) - slave already working but not upstreamed yet
* [PHC-only mode](https://github.com/pendulum-project/statime/issues/517) (for hardware timestamping)
* [Independent clock for arbitrary timescale](https://github.com/pendulum-project/statime/issues/389) (for software timestamping) - already sort of working, but unstable due to Linux using CLOCK_REALTIME for software timestamps, and not upstreamed completely

## Non-code contributions
If you want to contribute but can't code or don't know where to start because this project is complex, you can:
* report issues
* request new features
* discuss new features, often a decision needs to be made and having multiple voices helps
* test with more Linux software! `grep -r 'not tested yet'` for a list
* try to achieve sub-millisecond latency with PREEMPT_RT kernel
* test with more [Dante devices](#dante-devices), preferably very new or very old ones without firmware updates


# Changelog

## 0.5.0
* works with JACK, should work with any app that uses the soundcard as an interrupt source
* introduced automated integration test that launches several instances, streams audio data between them and checks for its correctness (including audio comparison). Currently tests JACK, PipeWire, aplay, arecord with alsa_pcm_inferno. SoX and Inferno2pipe are planned.
* Dockerfile
* fix logic error in CPU lockup prevention code

## 0.4.3
* support monotonic clock as base with jump-preventing guard (Statime)
* reduced snowball effect when CPU is overloaded, transmitter is lagging and tries to transmit multiple packets at once: max lag is set to `TX_LATENCY_NS`, and if transmit thread is locked for more than 5ms, 2ms sleep will be forced

## 0.4.2
* optimization: remove modulo division from ringbuffer index calculations

## 0.4.1
* disable SafeClock for now, doesn't help much and harms PHC
* fix panic on devices that can tx high number of channels per flow (e.g. Behringer Wing)

## 0.4.0
* refactor - preparation to introduction of controller functionality
* multicast transmitter

## 0.3.3
* fix starting without default route in routing table (Searchfire)

## 0.3.2
* changeable RX & TX channel names
* RX & TX latency configurable via env var or ALSA plugin parameter
* relicense to GPL-or-AGPL to simplify forking to AGPL projects, as parts of this project may be useful in cloud applications

## 0.3.1
* read configuration from ALSA plugin parameters - useful for multiple sources & sinks in PipeWire
* send statistics (clock, latency, signal levels)
* changeable usrvclock path

## 0.3.0
* introduced ALSA PCM plugin - a virtual soundcard compatible with most Linux audio apps
* receive clock using a documented protocol: [usrvclock](https://gitlab.com/lumifaza/usrvclock)
* various internal changes primarily related to allowing the use of external buffers (needed for mmap mode in ALSA plugin)
* receive multicasts
* ability to use non-default network ports to allow running multiple instances on a single IP address
* removed Inferno Wired because the ALSA plugin works well with PipeWire. [This is the last version](https://gitlab.com/lumifaza/inferno/-/blob/3941765700696f545425a5479be25091fda514d4/inferno_wired/src/main.rs) for the curious.

## 0.2.0
* audio transmitter
* alpha version of Inferno Wired - virtual audio source & sink for PipeWire
* receiving clock from [Statime](https://github.com/teodly/statime) modified for PTPv1 and virtual clock support - Linux-only for now (because CLOCK_TAI is Linux-only)
* increased receive thread priority to reduce chance of OS UDP input queue overflow

## 0.1.0

initial release


# To do
likely in order they'll be implementated

* ability to change settings in Dante Controller

At this point, Inferno will roughly become alternative to Dante Virtual Soundcard.

* realtime thread priorities configurable
* report late packets in DC
* operation without PTP daemon with lower clock precision - useful for OSes other than Linux
* read configuration from text files
* ability to work as a clock source (PTPv1 leader) - Statime
  * it is already possible if you use PTPv2 - in theory you should be able to make Inferno-only AoIP network - not tested yet
* add more apps and Linux distributions to the integration test
* bit-perfect transmitter (32-bit integers are always used internally; TX dithering for 16-bit and 24-bit output is configurable via `TX_SOURCE_BIT_DEPTH`)
* command line helper / TUI / GUI
* installer script, with cross compilation support
* more refactoring of network packets serializers & deserializers to make more code reusable in upcoming controller app (because DC is bloated closed source and bad UX)
* AES67
* primary & secondary network support, for dual-NIC computers
* API: number of channels changeable without device server restart (useful for Dante Via-like operation where transmitters & receivers can be added and removed dynamically)
  * not really necessary to replicate functionality of Via, as now multiple Inferno instances on a single IP address are supported
* `grep -r TODO inferno_aoip/src`


# Design
* 99% safe Rust (unsafe is required only because ALSA plugin API doesn't have safe Rust bindings)
* no external libraries needed, the only dependencies are Rust crates


# Motivation
I've been using free as in freedom, open source software for many years now. I'm also fascinated by connections between music and technology. One day my sound engineer collegue showed me how Dante works, how easy to use and (most of the time) stable it is. The problem was that it's not an open standard, didn't have open source implementation and I couldn't use it on my favourite operating system - Linux. Now I can.

## Why not AES67?
* AES67 is only a standard of media transport, not control, so flows need to be established manually. NMOS could fix it but I doubt Audinate will implement it in reasonable future.

And the following are limitations of AES67 implementation in Dante, not AES67 in general:

* it is not supported in Dante Virtual Soundcard and Dante Via
* some older Dante devices (without firmware upgrades) don't support it either
* it is multicast-only
* sample rate is locked to 48kHz

# Other open source projects related to Dante
* [network-audio-controller](https://github.com/chris-ritsen/network-audio-controller) - command line connection and device controller, alternative to Dante Controller
* [companion-module-audinate-dantecontroller](https://github.com/bitfocus/companion-module-audinate-dantecontroller) - Bitfocus Companion module for controlling Dante devices
* [inferno_runners](https://github.com/maze42d/inferno_runners) - scripts for running Inferno with PipeWire-based inter-soundcard bridge and [USB audio gadget](https://www.diyaudio.com/community/threads/linux-usb-audio-gadget-rpi4-otg.342070/)
* [dante-aes67-relay.js](https://gist.github.com/philhartung/87d336a3c432e2ce5452befcad1b945f) - Relay a Dante multicast stream to AES67
* [wycliffe](https://github.com/jsharkey/wycliffe), receiver implementation contained in a video control software - the earliest public attempt of reverse engineering Dante
* [List of AES67 audio resources](https://aes67.app/resources) at [AES67 Stream Monitor](https://aes67.app/) website (Dante is AES67-compatible but not on all devices and requires manual configuration)

## Alternatives
To my knowledge, there are no other unofficial implementations of audio transmission compatible with Dante. However, if AES67 fits your use case, you may want to use:
* [AES67 Linux daemon](https://github.com/bondagit/aes67-linux-daemon) with kernel-level virtual soundcard
* [pipewire-aes67](https://gitlab.freedesktop.org/pipewire/pipewire/-/wikis/AES67)
* [aes67-recorder](https://github.com/voc/aes67-recorder) ([tutorial for usage with Dante](https://behringer.world/viewtopic.php?t=197))
* AES67 can be also received by any software that supports raw audio in RTP packets, e.g. FFmpeg or GStreamer.
  * ... Dante multicasts are equally simple... just cut off 9 bytes at the front of every UDP packet ;)
