//! Rewrite a container and compare it with the original, byte for byte.
//!
//! The unit tests prove the readers and writers agree with each other, which
//! is worth exactly as much as any pair of functions agreeing with each other.
//! This points them at files written by somebody else — ffmpeg, or the
//! decoder — where a wrong assumption has something to disagree with.

use hz_core::{Error, Result};
use hz_io::container::caf::{CafReader, CafWriter};
use hz_io::container::wav::{WavReader, WavWriter};
use std::fs;
use std::path::{Path, PathBuf};

/// Frames per block: large enough that per-call overhead vanishes, small
/// enough to stay cache-friendly.
const BLOCK_FRAMES: usize = 8192;

pub struct Options {
    pub files: Vec<PathBuf>,
}

pub fn run(options: Options) -> Result<()> {
    let work = Path::new(env!("CARGO_MANIFEST_DIR")).join("../target/roundtrip");
    fs::create_dir_all(&work).map_err(|e| Error::io(&work, e))?;

    let mut identical = 0;
    let mut differing = 0;
    let mut failed = 0;

    for file in &options.files {
        match rewrite(file, &work) {
            Ok(copy) => {
                let verdict = match first_difference(file, &copy) {
                    Ok(None) => {
                        identical += 1;
                        "identical".to_string()
                    }
                    Ok(Some(detail)) => {
                        differing += 1;
                        detail
                    }
                    Err(e) => {
                        failed += 1;
                        e.to_string()
                    }
                };
                println!("  {:<24} {verdict}", name(file));
                let _ = fs::remove_file(&copy);
            }
            Err(e) => {
                failed += 1;
                println!("  {:<24} {e}", name(file));
            }
        }
    }

    println!("\n{identical} identical, {differing} differing, {failed} failed");
    Ok(())
}

fn name(path: &Path) -> String {
    path.file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned()
}

fn rewrite(path: &Path, work: &Path) -> Result<PathBuf> {
    let copy = work.join(name(path));
    match path.extension().and_then(|e| e.to_str()) {
        Some("caf") | Some("audio") => rewrite_caf(path, &copy)?,
        _ => rewrite_wav(path, &copy)?,
    }
    Ok(copy)
}

fn rewrite_caf(path: &Path, copy: &Path) -> Result<()> {
    let mut reader = CafReader::open(path)?;
    let format = *reader.format();
    let extra = reader.extra_chunks().to_vec();
    let mut writer = CafWriter::create_with_chunks(copy, format, &extra)?;

    let mut block = vec![0i32; BLOCK_FRAMES * format.channels as usize];
    loop {
        let frames = reader.read_frames(&mut block)?;
        if frames == 0 {
            break;
        }
        writer.write_frames(&block[..frames * format.channels as usize])?;
    }
    writer.finish()
}

fn rewrite_wav(path: &Path, copy: &Path) -> Result<()> {
    let mut reader = WavReader::open(path)?;
    let kind = reader.kind();
    let format = *reader.format();
    let layout = reader.layout().to_vec();
    let mut writer = WavWriter::create_with_layout(copy, kind, format, &layout)?;

    let channels = format.pcm.channels as usize;
    let mut block = vec![0i32; BLOCK_FRAMES * channels];
    loop {
        let frames = reader.read_frames(&mut block)?;
        if frames == 0 {
            break;
        }
        writer.write_frames(&block[..frames * channels])?;
    }
    writer.finish()
}

/// Where two files first differ, in a form that can be acted on.
///
/// "They differ" is not a bug report. The offset and the two bytes are, and
/// with a container the offset alone usually names the field.
fn first_difference(a: &Path, b: &Path) -> Result<Option<String>> {
    let left = fs::read(a).map_err(|e| Error::io(a, e))?;
    let right = fs::read(b).map_err(|e| Error::io(b, e))?;

    for (offset, (x, y)) in left.iter().zip(&right).enumerate() {
        if x != y {
            return Ok(Some(format!(
                "differs at {offset:#x}: {x:#04x} vs {y:#04x}"
            )));
        }
    }
    if left.len() != right.len() {
        return Ok(Some(format!(
            "same for {} bytes, then {} vs {} long",
            left.len().min(right.len()),
            left.len(),
            right.len()
        )));
    }
    Ok(None)
}
