//! Score the object fold against the encoder it was read out of.
//!
//! `tests/data/fold_samples.csv` is 600 measurements drawn uniformly from
//! 186 922: an object's position, and the gains a reference encoder actually
//! gave it, recovered by decoding a shipped stream's object elements and its
//! own 7.1 presentation and solving for the matrix between them. The sampling
//! is uniform on purpose — a stratified draw over-weights the sparse corners
//! of the room and reports a worse score than the law deserves.
//!
//! So this is not a golden file. The numbers on the right are somebody else's
//! encoder, with the measurement noise that comes of reading it through a
//! least-squares fit, and the thresholds below are the agreement the law
//! reaches on the whole population. A change that improves the fold should
//! move them up; a change that breaks it will move them a long way down.

use hz_render::{FoldMode, Layout, ObjectFold, elevation_scale};

struct Sample {
    position: [f64; 3],
    gains: Vec<f64>,
}

fn samples(csv: &str, channels: usize) -> Vec<Sample> {
    csv.lines()
        .filter(|line| !line.starts_with('#') && !line.trim().is_empty())
        .map(|line| {
            let v: Vec<f64> = line
                .split(',')
                .map(|f| f.trim().parse().expect("fixture holds numbers"))
                .collect();
            assert_eq!(v.len(), 3 + channels, "a position and {channels} gains");
            Sample {
                position: [v[0], v[1], v[2]],
                gains: v[3..].to_vec(),
            }
        })
        .collect()
}

/// Score one presentation against its measurements.
///
/// Returns the error as a share of signal and the shares landing within 0.05
/// and 0.02 on every channel.
fn score(layout: &Layout, mode: FoldMode, csv: &str) -> (f64, f64, f64) {
    let fold = ObjectFold::with_mode(layout, mode).expect("a measured presentation folds");
    let samples = samples(csv, layout.channels());
    assert_eq!(samples.len(), 600);
    let mut gains = vec![0.0; layout.channels()];
    let (mut error, mut signal) = (0.0f64, 0.0f64);
    let (mut within_05, mut within_02) = (0usize, 0usize);
    for sample in &samples {
        fold.gains(sample.position, &mut gains);
        let mut worst = 0.0f64;
        for (got, want) in gains.iter().zip(&sample.gains) {
            let d = got - want;
            error += d * d;
            signal += want * want;
            worst = worst.max(d.abs());
        }
        within_05 += usize::from(worst < 0.05);
        within_02 += usize::from(worst < 0.02);
    }
    let n = samples.len() as f64;
    (
        (error / signal).sqrt(),
        within_05 as f64 / n,
        within_02 as f64 / n,
    )
}

#[test]
fn the_fold_reproduces_the_encoder_it_was_measured_from() {
    for (layout, mode, csv, max_error, least_05, least_02) in [
        (
            Layout::surround_7_1(),
            FoldMode::Spread,
            include_str!("data/fold_samples.csv"),
            0.060,
            0.85,
            0.60,
        ),
        (
            Layout::surround_5_1(),
            FoldMode::Spread,
            include_str!("data/fold_samples_51.csv"),
            0.060,
            0.85,
            0.60,
        ),
        (
            Layout::stereo(),
            FoldMode::Spread,
            include_str!("data/fold_samples_20.csv"),
            0.060,
            0.90,
            0.70,
        ),
        // A stream of the other kind, whose rears stay on their own side.
        // It fits far more tightly, because the 7.1 render it is built on
        // fits that stream to 0.013 rather than 0.055.
        (
            Layout::surround_5_1(),
            FoldMode::SameSide,
            include_str!("data/fold_samples_51_sameside.csv"),
            0.020,
            0.99,
            0.95,
        ),
        (
            Layout::stereo(),
            FoldMode::SameSide,
            include_str!("data/fold_samples_20_sameside.csv"),
            0.030,
            0.99,
            0.95,
        ),
    ] {
        let (relative, share_05, share_02) = score(&layout, mode, csv);
        assert!(
            relative < max_error,
            "{} {mode:?}: error against the reference is {relative:.4} of signal",
            layout.name
        );
        assert!(
            share_05 > least_05,
            "{} {mode:?}: only {:.1}% of objects land within 0.05 on every channel",
            layout.name,
            100.0 * share_05
        );
        assert!(
            share_02 > least_02,
            "{} {mode:?}: only {:.1}% of objects land within 0.02 on every channel",
            layout.name,
            100.0 * share_02
        );
    }

    // The two modes are not interchangeable: each is wrong on the other's
    // stream by far more than either is on its own.
    let (crossed, _, _) = score(
        &Layout::surround_5_1(),
        FoldMode::Spread,
        include_str!("data/fold_samples_51_sameside.csv"),
    );
    assert!(
        crossed > 0.2,
        "the modes were interchangeable: {crossed:.4}"
    );
}

#[test]
fn an_object_between_two_speakers_of_a_row_is_shared_equally() {
    // Halfway along the side row, which spans two units and not one. Divide by
    // the wrong spacing and both speakers get zero here instead of 0.707.
    let fold = ObjectFold::for_layout(&Layout::surround_7_1()).expect("7.1 folds");
    let mut gains = vec![0.0; 8];
    fold.gains([0.0, 0.0, 0.0], &mut gains);
    let expected = hz_render::elevation_scale(0.0) * std::f64::consts::FRAC_1_SQRT_2;
    assert!((gains[4] - expected).abs() < 1e-12, "Ls was {}", gains[4]);
    assert!((gains[5] - expected).abs() < 1e-12, "Rs was {}", gains[5]);
    for (channel, gain) in gains.iter().enumerate() {
        if channel != 4 && channel != 5 {
            assert!(gain.abs() < 1e-12, "channel {channel} should be silent");
        }
    }
}

#[test]
fn an_object_on_a_speaker_reaches_only_that_speaker() {
    let fold = ObjectFold::for_layout(&Layout::surround_7_1()).expect("7.1 folds");
    let mut gains = vec![0.0; 8];
    for (position, channel) in [
        ([-1.0, 1.0, 0.0], 0),
        ([1.0, 1.0, 0.0], 1),
        ([0.0, 1.0, 0.0], 2),
        ([-1.0, 0.0, 0.0], 4),
        ([1.0, 0.0, 0.0], 5),
        ([-1.0, -1.0, 0.0], 6),
        ([1.0, -1.0, 0.0], 7),
    ] {
        fold.gains(position, &mut gains);
        assert!(
            (gains[channel] - hz_render::elevation_scale(0.0)).abs() < 1e-12,
            "{position:?} should be all of channel {channel}, got {gains:?}"
        );
    }
}

#[test]
fn the_lfe_is_never_panned_into() {
    let fold = ObjectFold::for_layout(&Layout::surround_7_1()).expect("7.1 folds");
    let mut gains = vec![0.0; 8];
    for x in [-1.0, -0.4, 0.0, 0.3, 1.0] {
        for y in [-1.0, -0.2, 0.5, 1.0] {
            for z in [0.0, 0.5, 1.0] {
                fold.gains([x, y, z], &mut gains);
                assert_eq!(gains[3], 0.0, "LFE took energy at {x} {y} {z}");
            }
        }
    }
}

#[test]
fn a_fold_carries_the_power_its_height_calls_for() {
    let fold = ObjectFold::for_layout(&Layout::surround_7_1()).expect("7.1 folds");
    let mut gains = vec![0.0; 8];
    for z in [0.0, 0.25, 0.5, 0.75, 1.0] {
        for (x, y) in [(-0.3, 0.7), (0.6, -0.2), (0.0, 0.0), (-1.0, -0.5)] {
            fold.gains([x, y, z], &mut gains);
            let norm = gains.iter().map(|g| g * g).sum::<f64>().sqrt();
            assert!(
                (norm - hz_render::elevation_scale(z)).abs() < 1e-12,
                "norm {norm} at {x} {y} {z}"
            );
        }
    }
}

/// A corner of the cube is a speaker, and the fold has to put it there.
///
/// The object frame a master carries is a **cube**, not a sphere: `[-1, 1, 0]`
/// is the front left corner, which is where L sits, and `[0.707, 0.707, 0]` —
/// the same *direction* on the unit sphere — is not a corner and is not a
/// speaker. Anything that hands the fold a normalised direction is asking it
/// the wrong question, and the answer it gets back is plausible and wrong.
///
/// The x axis runs to the right and the master's left is negative, which is
/// the one sign in this frame worth stating out loud.
#[test]
fn the_corners_of_the_cube_are_the_speakers() {
    let layout = Layout::surround_7_1();
    let fold = ObjectFold::for_layout(&layout).expect("7.1 folds to itself");
    let corners = [
        ([-1.0, 1.0, 0.0], "M+030"),
        ([1.0, 1.0, 0.0], "M-030"),
        ([-1.0, -1.0, 0.0], "M+135"),
        ([1.0, -1.0, 0.0], "M-135"),
    ];
    let mut gains = vec![0.0f64; layout.channels()];
    for (position, label) in corners {
        fold.gains(position, &mut gains);
        let wanted = layout
            .index_of(label)
            .unwrap_or_else(|| panic!("{label} is in a 7.1"));
        let loudest = gains
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.partial_cmp(b.1).expect("gains are finite"))
            .expect("a 7.1 has channels")
            .0;
        assert_eq!(
            loudest, wanted,
            "{position:?} should be {label}, and went to channel {loudest} instead: {gains:?}"
        );
        // The whole of it, to within the elevation scale the reference
        // applies at every height including none — 0.9487 at the listener's
        // plane, which is a measured property of the fold and not a leak.
        let scale = elevation_scale(0.0);
        assert!(
            (gains[wanted] - scale).abs() < 1e-9,
            "{label} got {} of the corner, against the elevation scale {scale}",
            gains[wanted]
        );
        let elsewhere: f64 = gains
            .iter()
            .enumerate()
            .filter(|(channel, _)| *channel != wanted)
            .map(|(_, gain)| gain * gain)
            .sum::<f64>()
            .sqrt();
        assert!(
            elsewhere < 1e-9,
            "{label} leaked {elsewhere} of the corner elsewhere: {gains:?}"
        );
    }
}

/// And the same corner through a narrower presentation lands where that
/// presentation's matrix puts it, which for the front pair is the same
/// speaker.
#[test]
fn a_front_corner_survives_the_narrowing_matrices() {
    for layout in [Layout::surround_5_1(), Layout::stereo()] {
        let fold = ObjectFold::for_layout(&layout).expect("a presentation the fold knows");
        let mut gains = vec![0.0f64; layout.channels()];
        fold.gains([-1.0, 1.0, 0.0], &mut gains);
        let left = layout
            .index_of("M+030")
            .expect("every presentation has a left");
        let loudest = gains
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.partial_cmp(b.1).expect("gains are finite"))
            .expect("channels")
            .0;
        assert_eq!(
            loudest, left,
            "the front left corner of a {} went to channel {loudest}: {gains:?}",
            layout.name
        );
    }
}

/// A bed channel answers by either of its names.
///
/// A master set writes `L` and `Uls`; an ADM file writes `M+030` and `U+110`.
/// A caller walking a master should not have to translate first — that is the
/// bug this pins: every bed channel of every real master came back `None`.
#[test]
fn a_bed_channel_answers_to_either_of_its_names() {
    use hz_render::fold::bed_position;
    for (master, label) in [
        ("L", "M+030"),
        ("R", "M-030"),
        ("C", "M+000"),
        ("Lss", "M+090"),
        ("Lrs", "M+135"),
        ("Lts", "U+090"),
    ] {
        let by_master = bed_position(master);
        let by_label = bed_position(label);
        assert!(by_master.is_some(), "no position for `{master}`");
        assert_eq!(by_master, by_label, "`{master}` against `{label}`");
    }
    // Front left is the front-left corner of the room, on the floor.
    assert_eq!(bed_position("L"), Some([-1.0, 1.0, 0.0]));
    // And a top speaker is the same place at the ceiling.
    assert_eq!(bed_position("Lts"), Some([-1.0, 0.0, 1.0]));
    // The low frequency channel has no position by either name.
    assert_eq!(bed_position("LFE"), None);
    assert_eq!(bed_position("LFE1"), None);
}
