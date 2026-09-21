//! Measure files, so the numbers can be held next to somebody else's.
//!
//! The meter's own tests check it against closed forms, which catches an
//! implementation that disagrees with arithmetic. They cannot catch one that
//! agrees with arithmetic and disagrees with every other meter in the world —
//! about where a window starts, say, or what a gate does at the edges. That
//! takes a second implementation reading the same file, which is what this is
//! for:
//!
//! ```text
//! cargo xtask loudness file.wav
//! ffmpeg -i file.wav -af ebur128=peak=true -f null -
//! ```

use hz_analysis::{ChannelWeight, Meter, TruePeak};
use hz_core::{Error, Result};
use hz_io::container::caf::CafReader;
use hz_io::container::wav::WavReader;
use std::path::{Path, PathBuf};

const BLOCK_FRAMES: usize = 8192;

pub struct Options {
    pub files: Vec<PathBuf>,
    /// Treat channel `n` as an LFE, which BS.1770 excludes entirely.
    pub lfe: Option<usize>,
    /// Treat these channels as surrounds, weighted 1.41.
    pub surround: Vec<usize>,
}

pub fn run(options: Options) -> Result<()> {
    for file in &options.files {
        let measured = measure(file, &options)?;
        println!("{}", file.display());
        println!("  channels     {}", measured.channels);
        println!("  frames       {}", measured.frames);
        println!("  I            {:>8.1} LUFS", measured.integrated);
        println!("  LRA          {:>8.1} LU", measured.range);
        println!("  true peak    {:>8.1} dBTP", measured.true_peak);
        println!();
    }
    Ok(())
}

struct Measured {
    channels: u32,
    frames: u64,
    integrated: f64,
    range: f64,
    true_peak: f64,
}

fn measure(path: &Path, options: &Options) -> Result<Measured> {
    let is_caf = matches!(
        path.extension().and_then(|e| e.to_str()),
        Some("caf") | Some("audio")
    );

    let (rate, channels, frames, blocks) = if is_caf {
        let mut reader = CafReader::open(path)?;
        let format = *reader.format();
        let frames = reader.frames();
        let mut blocks: Vec<Vec<i32>> = Vec::new();
        let mut block = vec![0i32; BLOCK_FRAMES * format.channels as usize];
        loop {
            let read = reader.read_frames(&mut block)?;
            if read == 0 {
                break;
            }
            blocks.push(block[..read * format.channels as usize].to_vec());
        }
        (format, format.channels, frames, blocks)
    } else {
        let mut reader = WavReader::open(path)?;
        let format = reader.format().pcm;
        let frames = reader.frames();
        let mut blocks: Vec<Vec<i32>> = Vec::new();
        let mut block = vec![0i32; BLOCK_FRAMES * format.channels as usize];
        loop {
            let read = reader.read_frames(&mut block)?;
            if read == 0 {
                break;
            }
            blocks.push(block[..read * format.channels as usize].to_vec());
        }
        (format, format.channels, frames, blocks)
    };

    if channels == 0 {
        return Err(Error::malformed(path, "no channels to measure"));
    }

    let weights: Vec<ChannelWeight> = (0..channels as usize)
        .map(|index| {
            if Some(index) == options.lfe {
                ChannelWeight::Excluded
            } else if options.surround.contains(&index) {
                ChannelWeight::Surround
            } else {
                ChannelWeight::Unity
            }
        })
        .collect();

    let mut meter = Meter::new(rate.sample_rate_hz(), &weights);
    let mut peak = TruePeak::new(channels as usize);
    let scale = rate.full_scale();
    let mut floats: Vec<f32> = Vec::new();

    for block in blocks {
        floats.clear();
        floats.extend(block.iter().map(|&s| s as f32 / scale));
        meter.push(&floats);
        peak.push(&floats);
    }
    meter.flush();

    Ok(Measured {
        channels,
        frames,
        integrated: meter.integrated(),
        range: meter.loudness_range(),
        true_peak: peak.peak_db(),
    })
}
