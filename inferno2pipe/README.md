Dante to raw audio pipe adapter.

# Quick start - how to record audio?
1. [install Rust](https://rustup.rs/) and [FFmpeg](http://ffmpeg.org/)
2. clone this repo with `--recursive` option (some dependencies are in submodules)
3. `cd inferno2pipe`
4. `./save_to_file 4` where `4` is number of channels
   * if your Dante network operates with sample rate other that 48000Hz, prefix the command with e.g. `sample_rate=44100`
5. make connections using [network-audio-controller](https://github.com/chris-ritsen/network-audio-controller) or Dante Controller
