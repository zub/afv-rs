// Runs afv-native's original DSP code on a fixed signal to produce reference outputs.
#include <compressor/compressor.h>
#include "afv-native/audio/RmsAgc.h"
#include <cmath>
#include <cstdio>
#include <string>
#include <vector>

using namespace afv_native;
using namespace afv_native::audio;

static const int N = 24000; // 25 frames

static std::vector<float> make_input() {
    std::vector<float> x(N);
    uint32_t lcg = 12345;
    auto noise = [&]() {
        lcg = lcg * 1664525u + 1013904223u;
        return (float)((double)lcg / 4294967296.0 * 2.0 - 1.0);
    };
    for (int i = 0; i < N; i++) {
        double t = (double)i / 48000.0;
        double v;
        if (i < 6000) {
            v = 0.1 * (1.0 + 0.5 * sin(2 * M_PI * 3 * t)) *
                (sin(2 * M_PI * 300 * t) + 0.5 * sin(2 * M_PI * 1200 * t) + 0.3 * sin(2 * M_PI * 2500 * t));
        } else if (i < 12000) {
            v = 0.6 * sin(2 * M_PI * 700 * t) + 0.3 * sin(2 * M_PI * 1900 * t) + 0.1 * noise();
        } else if (i < 18000) {
            double env = 0.5 * pow(0.02, (i - 12000) / 6000.0);
            v = env * noise();
        } else if (i < 21000) {
            v = 0.0;
        } else {
            v = 3.0 * sin(2 * M_PI * 440 * t);
            if (v > 1.0) v = 1.0;
            if (v < -1.0) v = -1.0;
        }
        x[i] = (float)v;
    }
    return x;
}

// afv-native's SimpleCompressorEffect (sndfilter's default compressor fed on the left channel),
// with one deliberate difference matching afv-audio: the detector starts settled (1.0) instead
// of fully compressed (0.0), which made a new compressor about 14 dB quieter for its first 0.3 s.
struct SettledCompressor {
    sf_compressor_state_st state;

    SettledCompressor() {
        sf_defaultcomp(&state, sampleRateHz);
        state.detectoravg = 1.0f;
    }

    void transformFrame(float *out, const float *in) {
        sf_sample_st input[frameSizeSamples] = {}, output[frameSizeSamples] = {};
        for (int i = 0; i < frameSizeSamples; i++) input[i].L = in[i];
        sf_compressor_process(&state, frameSizeSamples, input, output);
        for (int i = 0; i < frameSizeSamples; i++) out[i] = output[i].L;
    }
};

static void write(const std::string &dir, const char *name, const std::vector<float> &v) {
    std::string path = dir + "/" + name;
    FILE *f = fopen(path.c_str(), "wb");
    fwrite(v.data(), sizeof(float), v.size(), f);
    fclose(f);
}

template <class F> static std::vector<float> per_frame(const std::vector<float> &in, F f) {
    std::vector<float> out(N);
    for (int i = 0; i < N; i += frameSizeSamples) {
        f(out.data() + i, in.data() + i);
    }
    return out;
}

int main(int argc, char **argv) {
    std::string dir = argc > 1 ? argv[1] : ".";
    auto in = make_input();
    write(dir, "input.f32", in);
    SettledCompressor comp;
    write(dir, "compressor.f32", per_frame(in, [&](float *o, const float *i) { comp.transformFrame(o, i); }));
    RmsAgc agc;
    write(dir, "agc.f32", per_frame(in, [&](float *o, const float *i) { agc.transformFrame(o, i); }));
    return 0;
}
