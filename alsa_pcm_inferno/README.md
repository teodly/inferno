# alsa_pcm_inferno

[Inferno-AoIP](../README.md) ALSA PCM module (virtual soundcard)

# How to use

1. Follow common instructions in [Quick start](../README.md#quick-start) to build `alsa_pcm_inferno` and start clock synchronization daemon.
2. Copy (or symlink) `libasound_module_pcm_inferno.so` from `target/debug/` or `target/release/` directory to `/usr/lib64/alsa-lib/` (Fedora) or `/usr/lib/x86_64-linux-gnu/alsa-lib/` (Debian, Ubuntu). If using CPU architecture other than x86_64, the target directory will be different. `find /usr/lib* -type d -name alsa-lib` if unsure.
3. Add device with type `inferno` to your `.asoundrc` (example [`asoundrc`](asoundrc))
4. In the application there should be a place where usually device name is entered, e.g. `hw:1`. Enter the name of the Inferno `pcm.` device created in your `.asoundrc` there. If you've copied the example `asoundrc`, the name is just `inferno`.

Application must support 32-bit signed integer audio samples. (Audacity doesn't, but generally using Audacity directly with this plugin is not a good idea) `plug` plugin shipped with ALSA should work as automatic converter for apps that don't support that format (not tested yet).

If you don't mind replacing the whole system-wide ALSA configuration with Inferno-only setup, you can set ALSA environment variables to make libasound search for configuration file and modules in custom directories. Example is in `test_effect_processor.sh`.

## Recommended: audio server

This plugin is entirely user-space and contained in a library. It means that the Dante device is emulated only when the ALSA device is in use in an application and certainly can't outlive the process. When the stream is stopped or the whole audio app is closed, device disappears from the network. Next time it is opened, audio flows have to be established again and it takes time during which silence is played or recorded (in other words, several seconds of sound will be lost).

So the Inferno ALSA PCM is intended to be constantly running. The easiest way of ensuring this is using an audio server, for example [JACK](https://jackaudio.org/) (not tested yet) or [PipeWire](https://www.pipewire.org/) (see script [`start_pipewire_sink`](start_pipewire_sink)), making sure that automatic suspending of audio device is disabled (or, to save energy, set to a timeout long enough that it won't be annoying). Some DAWs (e.g. [Ardour](https://ardour.org/), [BespokeSynth](https://www.bespokesynth.com/), NOT Audacity) also keep the audio interface running all time.
