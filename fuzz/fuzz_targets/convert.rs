//! Device formats and buffer sizes: any rate a device might report, any callback size.
#![no_main]
use afv_audio::{CaptureConverter, RenderConverter};
use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;

#[derive(Arbitrary, Debug)]
struct Input {
    rate: u32,
    channels: u8,
    render: bool,
    sizes: Vec<u16>,
}

fuzz_target!(|input: Input| {
    // Rates from real hardware: 8 kHz up to 384 kHz.
    let rate = 8_000 + input.rate % 376_001;
    let channels = 1 + (input.channels % 8) as u16;
    if input.render {
        let Ok(mut c) = RenderConverter::new(rate, channels) else { return };
        for &n in input.sizes.iter().take(64) {
            let mut out = vec![f32::NAN; n as usize % 8192];
            c.fill(&mut out, |left, right| {
                left.fill(0.25);
                if let Some(r) = right {
                    r.fill(-0.25);
                }
            });
            assert!(out.iter().all(|s| s.is_finite()));
        }
    } else {
        let Ok(mut c) = CaptureConverter::new(rate, channels) else { return };
        for &n in input.sizes.iter().take(64) {
            let samples = vec![0.5f32; n as usize % 8192];
            c.push(&samples, |f| assert!(f.iter().all(|s| s.is_finite())));
        }
    }
});
