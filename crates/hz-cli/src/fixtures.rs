//! What the tests encode: a small master written for them, and a place
//! under `target/` to write it and their outputs.

use std::path::{Path, PathBuf};

/// A master set in `dir`, `samples` long, of a 7.1 bed and two objects
/// circling the room, a tone on each channel: its config's path.
pub(crate) fn master(dir: &Path, samples: usize) -> PathBuf {
    master_with(dir, samples, 2)
}

/// [`master`] with `objects` objects circling the room, every other one
/// climbing to the ceiling, on the channels after the bed's: with two,
/// [`master`] itself, to the byte.
pub(crate) fn master_with(dir: &Path, samples: usize, objects: usize) -> PathBuf {
    const BED: [(&str, u32); 8] = [
        ("L", 0),
        ("R", 1),
        ("C", 2),
        ("LFE", 3),
        ("Lss", 4),
        ("Rss", 5),
        ("Lrs", 6),
        ("Rrs", 7),
    ];
    let ids = || (BED.len() as u32..).take(objects);
    let mut config = String::from(
        "language: eng\nversion: 0.5.1\npresentations:\n  - type: home\n    simplified: \
         false\n    metadata: m.atmos.metadata\n    audio: m.atmos.audio\n    offset: 0.0\n    \
         fps: 24\n    scBedConfiguration: [0, 1, 2, 3, 4, 5, 6, 7]\n    creationTool: test\n    \
         creationToolVersion: 0.0.0\n    bedInstances:\n      - channels:\n",
    );
    for (name, id) in BED {
        config.push_str(&format!(
            "          - channel: {name}\n            ID: {id}\n"
        ));
    }
    config.push_str("    objects:\n");
    for id in ids() {
        config.push_str(&format!("      - ID: {id}\n"));
    }
    let path = dir.join("m.atmos");
    std::fs::write(&path, config).unwrap();

    let mut events = String::from("sampleRate: 48000\nevents:\n");
    for (_, id) in BED {
        events.push_str(&format!(
            "  - ID: {id}\n    samplePos: 0\n    active: true\n    importance: 1.0\n    \
             gain: 0\n    rampLength: 0\n"
        ));
    }
    // Each object somewhere new every 1000 samples, ramping there, each
    // going round the other way from the one before, set apart from the
    // others, and every other one climbing to the ceiling.
    for (k, id) in ids().enumerate() {
        for step in 0..=samples / 1000 {
            let turn = (step as f64 / 6.0 + k as f64 / objects as f64) * std::f64::consts::TAU;
            let way = if k % 2 == 0 { 1.0 } else { -1.0 };
            let (x, y, z) = (
                way * turn.sin(),
                turn.cos(),
                (((k % 2) * step) as f64 / 20.0).min(1.0),
            );
            events.push_str(&format!(
                "  - ID: {id}\n    samplePos: {}\n    active: true\n    pos: [{x:.4}, \
                 {y:.4}, {z:.4}]\n    snap: false\n    elevation: true\n    zones: all\n    \
                 size: 0.0\n    importance: 1.0\n    gain: 0\n    rampLength: 512\n",
                step * 1000
            ));
        }
    }
    std::fs::write(dir.join("m.atmos.metadata"), events).unwrap();

    let width = BED.len() + objects;
    let mut audio = hz_io::container::caf::CafWriter::create(
        &dir.join("m.atmos.audio"),
        hz_io::container::pcm::PcmFormat::master(48_000.0, width as u32),
    )
    .unwrap();
    let pcm: Vec<i32> = (0..samples * width)
        .map(|i| {
            let (channel, n) = (i % width, i / width);
            let hz = 110.0 * (channel + 1) as f64 * 1.37;
            let phase = std::f64::consts::TAU * hz * n as f64 / 48_000.0;
            (phase.sin() * f64::from(1 << 20)).round() as i32
        })
        .collect();
    audio.write_frames(&pcm).unwrap();
    audio.finish().unwrap();
    path
}

/// Fixtures go under `target/`, never the system temp directory.
pub(crate) fn scratch(area: &str, name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/test-fixtures/hz-cli")
        .join(area)
        .join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}
