#!/bin/bash
basedir="$(dirname "$0")"

export ALSA_CONFIG_PATH=$basedir/asoundrc
export ALSA_PLUGIN_DIR=$basedir/../target/debug

aplay -vvv -D default -r 48000 -f s32 -c 2 music.wav
#sox music.wav -t alsa default