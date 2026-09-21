//! Read the ADM out of a BW64 file and say what was in it — and, more to the
//! point, what was not understood.
//!
//! This is the tool that closes the verification gap recorded in docs/adm.md.
//! Pointed at a file written by another implementation, the `ignored` tally is
//! the whole report: every element it lists is one this crate would drop on a
//! conversion, named rather than discovered later.

use hz_core::{Error, Result};
use hz_io::adm::bw64;
use hz_io::adm::model::{AudioFormatExtended, Position};
use hz_io::container::wav::WavReader;
use std::path::PathBuf;

pub struct Options {
    pub files: Vec<PathBuf>,
    /// Print every block of every channel format, not just a count.
    pub blocks: bool,
}

pub fn run(options: Options) -> Result<()> {
    for file in &options.files {
        println!("{}", file.display());
        let reader = WavReader::open(file)?;
        println!(
            "  container    {:?}, {} ch, {} bit, {} frames",
            reader.kind(),
            reader.format().pcm.channels,
            reader.format().pcm.bits_per_channel,
            reader.frames(),
        );

        let Some(description) = bw64::read(file, &reader)? else {
            println!("  no ADM\n");
            continue;
        };

        println!(
            "  chna         {} tracks, {} entries",
            description.chna.track_count,
            description.chna.ids.len()
        );
        summarise(description.adm(), options.blocks);

        let ignored = &description.parsed.ignored;
        if ignored.is_empty() {
            println!("  understood   everything");
        } else {
            println!("  NOT MODELLED");
            for (name, count) in &ignored.elements {
                println!("    {name:<28} {count:>5}");
            }
        }
        println!();
    }
    Ok(())
}

fn summarise(adm: &AudioFormatExtended, blocks: bool) {
    println!(
        "  axml         {} programmes, {} contents, {} objects, {} packs, \
         {} channels, {} streams, {} track formats, {} track UIDs",
        adm.programmes.len(),
        adm.contents.len(),
        adm.objects.len(),
        adm.pack_formats.len(),
        adm.channel_formats.len(),
        adm.stream_formats.len(),
        adm.track_formats.len(),
        adm.track_uids.len(),
    );

    for channel in &adm.channel_formats {
        println!(
            "    {:<16} {:<16} {} blocks",
            channel.id,
            channel.type_definition.to_string(),
            channel.blocks.len()
        );
        if !blocks {
            continue;
        }
        for block in &channel.blocks {
            let position = match block.position {
                Some(Position::Cartesian { x, y, z }) => format!("xyz [{x}, {y}, {z}]"),
                Some(Position::Polar {
                    azimuth,
                    elevation,
                    distance,
                }) => format!("polar [{azimuth}, {elevation}, {distance}]"),
                None => match &block.speaker_label {
                    Some(label) => format!("speaker {label}"),
                    None => "no position".to_string(),
                },
            };
            let rtime = block
                .rtime
                .map(|t| t.to_string())
                .unwrap_or_else(|| "-".into());
            let duration = block
                .duration
                .map(|t| t.to_string())
                .unwrap_or_else(|| "-".into());
            println!("      {rtime:>18} +{duration:<18} {position}");
        }
    }

    // A track that resolves to nothing is the failure this is most likely to
    // find, and the one hardest to notice any other way.
    let unresolved: Vec<&str> = adm
        .track_uids
        .iter()
        .filter(|t| adm.channel_format_for_track(&t.uid).is_none())
        .map(|t| t.uid.as_str())
        .collect();
    if !unresolved.is_empty() {
        println!("  UNRESOLVED   {}", unresolved.join(", "));
    }
}

/// Rewrite the ADM of a file through this crate's model and report what
/// changed, so a difference is a finding rather than a surprise.
pub fn rewrite_check(file: &PathBuf) -> Result<()> {
    let reader = WavReader::open(file)?;
    let Some(description) = bw64::read(file, &reader)? else {
        return Err(Error::malformed(file, "no ADM to check"));
    };

    let chunks = bw64::chunks(&description.chna, description.adm());
    let text = std::str::from_utf8(&chunks[1].body)
        .map_err(|e| Error::malformed(file, format!("our own axml is not UTF-8: {e}")))?;
    let again = hz_io::adm::xml::parse(file, text)?;

    if again.adm == description.parsed.adm {
        println!("  ADM survives our own model");
        Ok(())
    } else {
        Err(Error::malformed(
            file,
            "the ADM does not survive our own model",
        ))
    }
}
