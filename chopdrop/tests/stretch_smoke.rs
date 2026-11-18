//! Smoke test for the time-stretch dependency: the C++ library builds, links
//! and stretches. The real wrapper and its onset tests live in `stretch.rs`.

use signalsmith_stretch::Stretch;

#[test]
fn exact_stretches_one_second_sine_to_two_seconds() {
    const RATE: u32 = 48_000;
    const OUT_FRAMES: usize = 96_000;
    let input: Vec<f32> = (0..RATE as usize)
        .map(|i| (std::f32::consts::TAU * 440.0 * i as f32 / RATE as f32).sin() * 0.5)
        .collect();
    let mut output = vec![0.0f32; OUT_FRAMES];

    let mut stretch = Stretch::preset_default(1, RATE);
    assert!(stretch.exact(&input, &mut output), "exact() reported failure");

    assert_eq!(output.len(), OUT_FRAMES);
    assert!(output.iter().all(|s| s.is_finite()), "non-finite sample in output");
    // The middle of the stretched sine still carries signal at about its level.
    let mid = &output[OUT_FRAMES / 4..OUT_FRAMES * 3 / 4];
    let rms = (mid.iter().map(|s| s * s).sum::<f32>() / mid.len() as f32).sqrt();
    assert!(rms > 0.2 && rms < 0.6, "unexpected rms {rms}");
}
