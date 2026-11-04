//! Compares the DSP ports against outputs of afv-native's original C/C++ code. (The hardware
//! filter is no longer a port: afv-rs keeps afv-native's EQ curves but not its dynamics.)
//!
//! `tests/data/*.f32` were produced by `tests/data/reference.cpp`, compiled against afv-native
//! (commit fc7d216) with `-ffp-contract=off`, from a fixed 0.5 s test signal (`input.f32`) mixing
//! quiet tones, loud tones with noise, decaying noise, silence and a clipped square-ish wave.
//!
//! The compressor reference includes afv-audio's one deliberate change: the detector starts
//! settled instead of fully compressed. `reference.cpp` makes the same change to sndfilter's state.

use afv_audio::dsp::{Compressor, RmsAgc};
use afv_audio::{FRAME_SAMPLES, Frame};

fn load(name: &str) -> Vec<f32> {
    let path = format!("{}/tests/data/{name}", env!("CARGO_MANIFEST_DIR"));
    let bytes = std::fs::read(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
    bytes.as_chunks::<4>().0.iter().map(|b| f32::from_le_bytes(*b)).collect()
}

fn per_frame(input: &[f32], mut f: impl FnMut(&mut Frame)) -> Vec<f32> {
    let mut out = input.to_vec();
    for chunk in out.as_chunks_mut::<FRAME_SAMPLES>().0 {
        f(chunk);
    }
    out
}

fn assert_matches(name: &str, actual: &[f32], tolerance: f32) {
    let expected = load(name);
    assert_eq!(actual.len(), expected.len());
    let (worst, at) = actual
        .iter()
        .zip(&expected)
        .map(|(a, e)| (a - e).abs())
        .enumerate()
        .fold((0f32, 0), |(m, mi), (i, d)| if d > m { (d, i) } else { (m, mi) });
    println!("{name}: max deviation {worst:e} at sample {at}");
    assert!(
        worst <= tolerance,
        "{name}: sample {at} differs by {worst} (actual {}, expected {})",
        actual[at],
        expected[at]
    );
    // Guard against a degenerate reference (e.g. all zeros) making the test meaningless.
    assert!(expected.iter().any(|v| v.abs() > 0.05), "{name}: reference is silent");
}

#[test]
fn compressor_matches_afv_native() {
    let input = load("input.f32");
    let mut comp = Compressor::new();
    let out = per_frame(&input, |f| comp.process(f));
    assert_matches("compressor.f32", &out, 1e-5);
}

#[test]
fn agc_matches_afv_native() {
    let input = load("input.f32");
    let mut agc = RmsAgc::default();
    let out = per_frame(&input, |f| agc.process(f));
    assert_matches("agc.f32", &out, 1e-6);
}
