#!/bin/bash
basedir="$(dirname "$0")"

export ALSA_CONFIG_PATH=$basedir/asoundrc
export ALSA_PLUGIN_DIR=$basedir/../target/debug

sox -SV -t alsa inferno -t alsa inferno flanger echos 0.8 0.7 700 0.25 900 0.3 flanger
