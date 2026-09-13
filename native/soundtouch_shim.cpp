// SoundTouch (LGPL-2.1) C shim: exports plain C symbols for Rust FFI
// (avoids C++ name-mangling differences between MSVC and bindgen output).
#include "SoundTouch.h"

using soundtouch::SoundTouch;

extern "C" {
SoundTouch* st_new() { return new SoundTouch(); }
void st_free(SoundTouch* st) { delete st; }
void st_set_channels(SoundTouch* st, unsigned n) { st->setChannels(n); }
void st_set_sample_rate(SoundTouch* st, unsigned rate) { st->setSampleRate(rate); }
void st_set_pitch(SoundTouch* st, double pitch) { st->setPitch(pitch); }
void st_set_tempo(SoundTouch* st, double tempo) { st->setTempo(tempo); }
void st_put_samples(SoundTouch* st, const float* samples, unsigned n) {
    st->putSamples(samples, n);
}
unsigned st_receive_samples(SoundTouch* st, float* out, unsigned max) {
    return st->receiveSamples(out, max);
}
unsigned st_num_unprocessed_samples(SoundTouch* st) {
    return st->numUnprocessedSamples();
}
void st_flush(SoundTouch* st) { st->flush(); }
void st_clear(SoundTouch* st) { st->clear(); }
}
