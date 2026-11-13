// Regression test for teodly/inferno#8: ALSA can call the plugin's pointer
// callback before the stream is prepared (snd_pcm_status/avail/hwsync right
// after open, as JACK does). That used to panic inside an extern "C"
// callback, which aborts the host process. Exits 0 when every call returns.
#include <alsa/asoundlib.h>
#include <stdio.h>

int main(int argc, char **argv) {
  const char *dev = argc > 1 ? argv[1] : "inferno_status_test";
  snd_pcm_stream_t dirs[] = {SND_PCM_STREAM_CAPTURE, SND_PCM_STREAM_PLAYBACK};
  for (int i = 0; i < 2; i++) {
    snd_pcm_t *pcm;
    int rc = snd_pcm_open(&pcm, dev, dirs[i], 0);
    if (rc < 0) {
      fprintf(stderr, "open %s: %s\n", dev, snd_strerror(rc));
      return 2;
    }
    snd_pcm_status_t *st;
    snd_pcm_status_alloca(&st);
    snd_pcm_status(pcm, st);
    snd_pcm_avail(pcm);
    snd_pcm_hwsync(pcm);
    snd_pcm_close(pcm);
    printf("%s: status/avail/hwsync before prepare returned\n", i == 0 ? "capture" : "playback");
  }
  return 0;
}
