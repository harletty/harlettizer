//! Check that a stream is one every player can open.
//!
//! ```text
//! cargo xtask ffmpeg-check stream.thd [more.thd …]
//! ```
//!
//! FFmpeg's TrueHD decoder is the one inside VLC, Kodi, Plex and every player
//! that is not Dolby's own, so a stream it refuses is a stream nobody outside
//! Dolby's hardware can play — however exactly it round-trips through
//! `harletty` and the decoder in the test tree. That is not a hypothetical:
//! with folds, the encoder once wrote up to fourteen matrices into the 7.1
//! substream, the field can say fifteen, both of those decoders took it, and
//! FFmpeg refused unit after unit of it, with errors that cascaded into
//! filter orders, quantiser steps and substream lengths that were never
//! wrong — while exiting zero.
//!
//! Two checks, because either alone can miss it:
//!
//! - **The count.** Every substream's matrices, from our own reader, against
//!   what a decoder reads: [`MAX_MATRICES`] under restart sync words A and B,
//!   [`MAX_MATRICES_IMMERSIVE`] under C. Needs nothing installed, and names
//!   the substream.
//! - **FFmpeg itself**, decoding the whole stream to nowhere at `-v warning`.
//!   A healthy stream draws no line at all, so any line fails. Its exit status
//!   says nothing — it exits zero on a stream it refused every unit of — so
//!   what it prints is the verdict. Skipped, and said so, when it is not
//!   installed.
//!
//! The count reads the matrices a restart header's first block declares,
//! which is where this encoder declares them: it decides them per restart
//! interval.

use hz_core::{Error, Result};
use hz_mlp::format::RestartSync;
use hz_mlp::matrix::{MAX_MATRICES, MAX_MATRICES_IMMERSIVE};
use hz_mlp::reader;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

pub struct Options {
    pub inputs: Vec<PathBuf>,
}

pub fn run(options: Options) -> Result<()> {
    let mut failed = Vec::new();
    for input in &options.inputs {
        println!("{}", input.display());
        if !check(input)? {
            failed.push(input.display().to_string());
        }
    }
    if failed.is_empty() {
        Ok(())
    } else {
        Err(Error::malformed(
            Path::new(&failed[0]),
            format!(
                "{} of {} streams are not ones every player can open; see above",
                failed.len(),
                options.inputs.len()
            ),
        ))
    }
}

/// Both checks on one stream, printed; whether it passed.
pub fn check(path: &Path) -> Result<bool> {
    let counted = count_matrices(path)?;
    let decoded = decode_with_ffmpeg(path);
    Ok(counted && decoded)
}

/// The most matrices each substream declared, against what a decoder reads.
fn count_matrices(path: &Path) -> Result<bool> {
    let bytes = std::fs::read(path).map_err(|e| Error::io(path, e))?;

    // Per substream: the sync word it restarts under, the most matrices one
    // restart declared, and how many restarts declared more than a decoder
    // reads.
    let mut seen: BTreeMap<usize, (RestartSync, usize, usize)> = BTreeMap::new();
    let mut at = 0;
    let mut substreams = 0;
    let mut evolution = false;
    let mut units = 0usize;
    while at < bytes.len() {
        let unit = match reader::access_unit(&bytes[at..], substreams, evolution) {
            Ok(unit) => unit,
            Err(why) => {
                println!("  matrices     unreadable at access unit {units}, byte {at}: {why}");
                return Ok(false);
            }
        };
        substreams = unit.directory.len();
        if let Some(sync) = &unit.major_sync {
            evolution = sync.flags & (1 << 12) != 0;
        }
        for (which, substream) in unit.substreams.iter().enumerate() {
            let Some(restart) = &substream.restart else {
                continue;
            };
            let count = substream.matrices.len();
            let entry = seen.entry(which).or_insert((restart.sync, 0, 0));
            entry.0 = restart.sync;
            entry.1 = entry.1.max(count);
            if count > limit(restart.sync) {
                entry.2 += 1;
            }
        }
        at += unit.bytes;
        units += 1;
    }

    let most: Vec<String> = seen.values().map(|(_, most, _)| most.to_string()).collect();
    let limits: Vec<String> = seen
        .values()
        .map(|(sync, _, _)| limit(*sync).to_string())
        .collect();
    let over: Vec<(&usize, &(RestartSync, usize, usize))> =
        seen.iter().filter(|(_, (_, _, over))| *over > 0).collect();
    if over.is_empty() {
        println!(
            "  matrices     at most {} per substream, against {} a decoder reads",
            most.join("/"),
            limits.join("/")
        );
        return Ok(true);
    }
    for (which, (sync, most, restarts)) in over {
        println!(
            "  matrices     substream {which} declares up to {most} in {restarts} restarts, \
             and a decoder reads {} under sync {sync:?}",
            limit(*sync)
        );
    }
    Ok(false)
}

fn limit(sync: RestartSync) -> usize {
    match sync {
        RestartSync::A | RestartSync::B => MAX_MATRICES,
        RestartSync::C => MAX_MATRICES_IMMERSIVE,
    }
}

/// Decode the whole stream with FFmpeg and hold it to silence.
fn decode_with_ffmpeg(path: &Path) -> bool {
    let output = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-nostdin",
            "-v",
            "warning",
            "-f",
            "truehd",
            "-i",
        ])
        .arg(path)
        .args(["-f", "null", "-"])
        .output();
    let output = match output {
        Ok(output) => output,
        Err(_) => {
            println!("  ffmpeg       not installed; the decode was not checked");
            return true;
        }
    };

    let text = String::from_utf8_lossy(&output.stderr);
    let lines: Vec<&str> = text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    if output.status.success() && lines.is_empty() {
        println!("  ffmpeg       decodes it without a word");
        return true;
    }

    // The same complaint repeats unit after unit; say each once, with how
    // often, in the order they first came.
    let mut distinct: Vec<(String, usize)> = Vec::new();
    for line in &lines {
        let key = without_addresses(line);
        match distinct.iter_mut().find(|(seen, _)| *seen == key) {
            Some((_, count)) => *count += 1,
            None => distinct.push((key, 1)),
        }
    }
    println!(
        "  ffmpeg       {} complaints{}",
        lines.len(),
        if output.status.success() {
            String::new()
        } else {
            format!(", and it exited with {}", output.status)
        }
    );
    for (line, count) in distinct.iter().take(6) {
        println!("               {count:>5}× {line}");
    }
    false
}

/// A complaint with the pointers FFmpeg prints taken out, so that one said
/// by two instances of the decoder is counted as one.
fn without_addresses(line: &str) -> String {
    line.split_whitespace()
        .map(|word| {
            let bare = word.trim_end_matches(']');
            if bare.starts_with("0x") && bare.len() > 2 {
                if word.ends_with(']') {
                    "0x…]"
                } else {
                    "0x…"
                }
                .to_string()
            } else {
                word.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_complaint_from_two_decoder_instances_counts_once() {
        assert_eq!(
            without_addresses("[truehd @ 0x55d0a1] Number of primitive matrices"),
            without_addresses("[truehd @ 0x7f3b22] Number of primitive matrices"),
        );
        assert_eq!(
            without_addresses("[dec:truehd @ 0x561e53c5a340] Error"),
            "[dec:truehd @ 0x…] Error"
        );
    }

    #[test]
    fn a_decoder_reads_eight_before_the_immersive_substream_and_sixteen_in_it() {
        assert_eq!(limit(RestartSync::A), 8);
        assert_eq!(limit(RestartSync::B), 8);
        assert_eq!(limit(RestartSync::C), 16);
    }
}
