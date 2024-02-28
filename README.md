# Inferno - unofficial implementation of Dante protocol

Highly experimental for now. I don't recommend using it for serious purposes.

However, chances that it'll break already working Dante network are low.

# Features
* receiving audio from Dante devices
* connections can be made using Dante Controller or [network-audio-controller](https://github.com/chris-ritsen/network-audio-controller) (`netaudio` command line tool)


## Quirks, read it before using:
* Dante protocol is undocumented. Everything was reverse-engineered or based on other reverse-engineering projects. Some things in implementation were guessed. So while it works with my setup, it may not work with yours.
* channel names can't be changed. If you try to change them, Dante Controller may get confused
* receiving from multicast flows isn't supported yet, unicast connection will be made
* clocked by incoming media flows. When nothing is connected, "time will stop" (i.e. recording will pause) until something is connected again - silence won't be generated unless at least one channel is connected.
* it will not start if there is no default route in OS routing table

Disclaimer: Dante uses technology patented by Audinate. This source code may use these patents too. Consult a lawyer if you want to:
* make money of it
* distribute binaries in (or from) a region where software patents apply

This project makes no claim to be either authorized or approved by Audinate.


# Quick start - how to record audio?
1. [install Rust](https://rustup.rs/) and [FFmpeg](http://ffmpeg.org/)
2. clone this repo with `--recursive` option (some dependencies are in submodules)
3. `cd inferno2pipe`
4. `./save_to_file 4` where `4` is number of channels
   * if your Dante network operates with sample rate other that 48000Hz, prefix the command with e.g. `sample_rate=44100`
5. make connections using [network-audio-controller](https://github.com/chris-ritsen/network-audio-controller) or Dante Controller


# Tested with
## Dante devices
* Audinate AVIO AES3
* Ben & Fellows 523019 4x4 balanced analog I/O module (based on Dante UltimoX4)
* Klark Teknik DN32-DANTE (based on Dante Brooklyn II)
* Dante Via @ OS X
* Dante Virtual Soundcard @ Windows 10

## Control software
* Dante Controller @ Windows 11
* network-audio-controller

## Host
* x86_64 Linux
  * Arch
  * Ubuntu


# Anatomy of the repository
* `inferno_aoip` - main library crate for emulating a Dante audio over IP device. In the future controller functionality will also be implemented. **Start here if you want to develop your app based on Inferno**.
* `inferno2pipe` - capture audio, writing interleaved 32-bit integer samples into an Unix named pipe (or a raw file). Helper script for recording to more convenient format is also provided. **Start here if you want to use Inferno for capturing audio**
* `searchfire` - fork of [Searchlight](https://github.com/WilliamVenner/searchlight) mDNS crate, modified for compatibility with Dante's mDNS
* `cirb` - Clock-Indexed Ring-Buffer - fork of [`rt-history`](https://github.com/HadrienG2/rt-history) crate with emphasis on allowing reordered incoming packets and clock synchronization


# Environment variables
* `INFERNO_BIND_IP` - which local IP to bind to. Specifying it may be necessary if you have multiple network interfaces
* `INFERNO_DEVICE_ID` - 16 hexadecimal digits (8 bytes) used as a device ID. Dante devices usually use MAC address padded with zeros. Inferno uses `0000<IP address>0000` by default. Device ID is the storage key when saving state.
* `INFERNO_NAME` - name of advertised device. If unspecified, name based on app name and IP address will be generated.
* `INFERNO_SAMPLE_RATE` - sample rate this device will operate on


# Contributing
Issue reports and pull requests are welcome. However I'm currently taking a break from this project and will be back in June or July 2023.

By submitting any contribution, you agree that it will be distributed according to the comment found at the top of `inferno_aoip/src/lib.rs` file - under the terms of GNU GPL v3 or any later version.

Please use editor respecting `.editorconfig` (for example, VSCode needs an extension: [EditorConfig for VS Code](https://open-vsx.org/extension/EditorConfig/EditorConfig)) or configure it approprietly manually.


# Changelog
## 0.2.0
* audio transmitter
* alpha version of Inferno Wired - virtual audio source & sink for PipeWire
* receiving clock from [Statime](https://github.com/teowoz/statime) modified for PTPv1 and virtual clock support - Linux-only for now (because CLOCK_TAI is Linux-only)
* increased receive thread priority to reduce chance of OS UDP input queue overflow


## 0.1.0

initial release


# To do
likely in order they'll be implementated

* use multicast flows when available
* ability to change channel names and settings in Dante Controller

At this point, Inferno will roughly become alternative to Dante Virtual Soundcard.

* improve performance of receiver. Currently it's implemented with coroutines which is not very optimal for processing thousands of packets per second.
* integration with JACK
* send statistics (clock, latency, signal levels)
* ability to work as a clock source (PTP leader)
* ability to use non-default network ports to allow running multiple instances on a single IP address
* automated integration test that will launch several instances, stream audio data between them and check for its correctness
* API: number of channels changeable without device server restart (useful for Dante Via-like operation where transmitters & receivers can be added and removed dynamically)
* AES67
* primary & secondary network support, for dual-NIC computers
* `grep -r TODO inferno_aoip/src`


# Design
* 99.9% safe Rust (unsafe is required only because PipeWire Rust bindings return raw buffers)
* no external libraries needed, the only dependencies are Rust crates


# Motivation
I've been using free as in freedom, open source software for many years now. I'm also fascinated by connections between music and technology. One day my sound engineer collegue showed me how Dante works, how easy to use and (most of the time) stable it is. The problem was that it's not an open standard, didn't have open source implementation and I couldn't use it on my favourite operating system - Linux. Now I can.


# Other open source projects related to Dante
* [network-audio-controller](https://github.com/chris-ritsen/network-audio-controller) - command line connection and device controller, alternative to Dante Controller
* [dante-aes67-relay.js](https://gist.github.com/philhartung/87d336a3c432e2ce5452befcad1b945f) - Relay a Dante multicast stream to AES67
* [wycliffe](https://github.com/jsharkey/wycliffe), receiver implementation contained in a video control software
* [List of AES67 audio resources](https://aes67.app/resources) at [AES67 Stream Monitor](https://aes67.app/) website (Dante is AES67-compatible but not on all devices and requires manual configuration)
