#!/bin/bash
basedir="$(dirname "$0")"

export ALSA_CONFIG_PATH=$basedir/asoundrc
export ALSA_PLUGIN_DIR=$basedir/../target/debug

arecord -vvv -D default -r 48000 -f s32 -c 2 test.wav
