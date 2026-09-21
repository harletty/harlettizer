//! Read a TrueHD stream's structure and say what is in it.
//!
//! ```text
//! cargo xtask thd stream.thd
//! ```
//!
//! Its purpose is comparison. Every field of this format this project got
//! right was got right by holding it next to a stream somebody else wrote, and
//! the ones it got wrong were wrong in ways that parsed cleanly and decoded to
//! the wrong samples. So this reads both — a reference and our own output —
//! and prints them in the same shape.

use hz_core::hmac::HmacSha256;
use hz_core::{Error, Result};
use hz_io::settings::Settings;
use hz_mlp::reader;
use std::path::PathBuf;

pub struct Options {
    pub input: PathBuf,
    /// Stop after this many access units.
    pub units: Option<usize>,
    /// Print every unit rather than the first and a summary.
    pub all: bool,
    /// The settings file holding the key the protection fields are checked
    /// against; the command line's own default locations otherwise.
    pub config: Option<PathBuf>,
}

pub fn run(options: Options) -> Result<()> {
    let bytes = std::fs::read(&options.input).map_err(|e| Error::io(&options.input, e))?;
    let limit = options.units.unwrap_or(usize::MAX);
    let settings = Settings::load(options.config.as_deref())?;
    let key = settings.evolution_key.as_deref().map(HmacSha256::new);

    let mut at = 0usize;
    let mut substreams = 0usize;
    let mut evolution = false;
    let mut count = 0usize;
    let mut restarts = 0usize;
    let mut bad = Vec::new();
    let mut extra_data_total = 0usize;
    let mut extra_data_units = 0usize;
    // Evolution frames carrying a protection field, and how many of them the
    // key reproduces.
    let mut protected = 0usize;
    let mut verified = 0usize;
    // What the substream directory's second word carries, per substream: how
    // many units said anything, and the range and last value of the gain.
    let mut drc: Vec<(usize, i32, i32, u32)> = Vec::new();
    // A dynamic range value stands for `2^refresh` access units and no
    // longer. A decoder counts the unit it reads the next word in, so the gap
    // between two words may be that many units and not one more — which is
    // exactly the off-by-one this catches.
    // Units since the last word, and how long *that* word said it would
    // stand — the bound in force is the previous value's, not the new one's.
    let mut drc_due: Vec<Option<(u64, u64)>> = Vec::new();

    while at < bytes.len() && count < limit {
        let unit = match reader::access_unit(&bytes[at..], substreams, evolution) {
            Ok(unit) => unit,
            Err(why) => {
                println!("access unit {count} at byte {at}: {why}");
                break;
            }
        };
        substreams = unit.directory.len();
        if let Some(sync) = &unit.major_sync {
            evolution = sync.flags & (1 << 12) != 0;
        }

        if count == 0 || options.all {
            print_unit(count, &unit);
        }
        if unit.major_sync.is_some() {
            restarts += 1;
        }
        if unit.extra_data > 0 {
            extra_data_total += unit.extra_data;
            extra_data_units += 1;
        }
        drc.resize(
            unit.directory.len().max(drc.len()),
            (0, i32::MAX, i32::MIN, 0),
        );
        drc_due.resize(unit.directory.len().max(drc_due.len()), None);
        for (which, entry) in unit.directory.iter().enumerate() {
            if let Some((since, _)) = &mut drc_due[which] {
                *since += 1;
            }
            if entry.extra_word {
                let seen = &mut drc[which];
                seen.0 += 1;
                seen.1 = seen.1.min(entry.drc_gain_update);
                seen.2 = seen.2.max(entry.drc_gain_update);
                seen.3 = entry.drc_time_update;
                if let Some((since, stood_for)) = drc_due[which]
                    && since > stood_for
                {
                    bad.push(format!(
                        "unit {count}: substream {which} restated its gain after {since} \
                         units, and the last one stood for {stood_for}"
                    ));
                }
                drc_due[which] = Some((0, 1u64 << entry.drc_time_update));
            }
        }
        if !unit.parity_nibble_ok {
            bad.push(format!("unit {count}: access unit parity"));
        }
        if let Some(sync) = &unit.major_sync
            && !sync.checksum_ok
        {
            bad.push(format!("unit {count}: major sync checksum"));
        }
        if let Some(extra) = &unit.evolution {
            if !extra.check_nibble_ok {
                bad.push(format!("unit {count}: extra data check nibble"));
            }
            if !extra.parity_ok {
                bad.push(format!("unit {count}: extra data parity"));
            }
            if !extra.frame_ok {
                bad.push(format!(
                    "unit {count}: extra data evolution frame unreadable"
                ));
            }
            if extra.frame_ok && !extra.protection_bytes.is_empty() {
                protected += 1;
            }
            if let Some(key) = &key {
                match unit.protection_verifies(key, &bytes[at..]) {
                    Some(true) => verified += 1,
                    Some(false) => bad.push(format!(
                        "unit {count}: the protection field is not the key's digest"
                    )),
                    None => {}
                }
            }
        }
        for (index, substream) in unit.substreams.iter().enumerate() {
            if !substream.parity_ok {
                bad.push(format!("unit {count}: substream {index} parity"));
            }
            if !substream.checksum_ok {
                bad.push(format!("unit {count}: substream {index} checksum"));
            }
            if let Some(restart) = &substream.restart
                && !restart.checksum_ok
            {
                bad.push(format!("unit {count}: substream {index} restart checksum"));
            }
        }

        at += unit.bytes;
        count += 1;
    }

    println!();
    println!("  units        {count}, {restarts} with a major sync");
    println!("  bytes        {at}");
    if extra_data_units > 0 {
        println!(
            "  extra data   {extra_data_total} bytes over {extra_data_units} units, {:.0} a unit",
            extra_data_total as f64 / extra_data_units as f64
        );
    } else {
        println!("  extra data   none");
    }
    if protected > 0 {
        match (&key, &settings.from) {
            (Some(_), Some(from)) => println!(
                "  protection   {verified} of {protected} fields are the digest of the key \
                 from {}",
                from.display()
            ),
            (Some(_), None) => {
                println!("  protection   {verified} of {protected} fields are the key's digest")
            }
            (None, _) => {
                println!("  protection   {protected} fields, unchecked: no key configured")
            }
        }
    }
    if drc.iter().any(|seen| seen.0 > 0) {
        for (which, (units, low, high, time)) in drc.iter().enumerate() {
            if *units == 0 {
                println!("  drc {which}        never");
                continue;
            }
            // The gain is a power of two in sixty-fourths.
            let db = |step: i32| f64::from(step) / 64.0 * 6.020_6;
            println!(
                "  drc {which}        {units} of {count} units, gain {low}..{high} \
                 ({:+.2}..{:+.2} dB), refresh every {} units",
                db(*low),
                db(*high),
                1u32 << time,
            );
        }
    }
    if bad.is_empty() {
        println!("  integrity    every checksum and parity verifies");
    } else {
        println!("  integrity    {} failures", bad.len());
        for line in bad.iter().take(8) {
            println!("               {line}");
        }
    }
    Ok(())
}

/// What the object audio metadata says, in the master format's own frame so
/// it can be held next to a decoder's own metadata output.
fn print_objects(bytes: &[u8]) {
    let oamd = match hz_meta::ObjectAudioMetadata::read(bytes) {
        Ok(oamd) => oamd,
        Err(why) => {
            println!("               {why}");
            return;
        }
    };
    println!(
        "               {} elements{}, {} block{}",
        oamd.objects(),
        if oamd.lfe { " (the first an LFE)" } else { "" },
        oamd.blocks.len(),
        if oamd.blocks.len() == 1 { "" } else { "s" },
    );
    for block in &oamd.blocks {
        print!(
            "               at {:>3}, ramp {:?}:",
            block.offset * 32,
            block.ramp
        );
        for object in &block.objects {
            match &object.render {
                Some(render) => {
                    let [x, y, z] = render.position.to_master();
                    print!(" [{x:.2} {y:.2} {z:.2}]");
                }
                None => print!(" bed"),
            }
        }
        println!();
    }
}

fn print_unit(index: usize, unit: &reader::AccessUnit) {
    println!(
        "access unit {index}: {} bytes, timing {}",
        unit.bytes, unit.input_timing
    );
    if let Some(sync) = &unit.major_sync {
        println!(
            "  major sync   {} bytes, {} substreams, peak {} ({} kbit/s at 48 kHz)",
            sync.size,
            sync.substreams,
            sync.peak_bitrate,
            sync.peak_bitrate * 48_000 / 16 / 1000
        );
        println!(
            "               arrangements 6ch={:#06x} 8ch={:#06x}, flags={:#06x}",
            sync.arrangement_6ch, sync.arrangement_8ch, sync.flags
        );
        println!(
            "               substream_info={:#04x} extended={:#x}{}",
            sync.substream_info,
            sync.extended_substream_info,
            match &sync.extra_channel_meaning {
                Some(extra) => format!(", extra channel meaning {} bytes", extra.len()),
                None => String::new(),
            }
        );
        if let Some(immersive) = &sync.immersive {
            println!(
                "               16 elements: {} objects{}, dialnorm -{} dB, mix level {} dB",
                immersive.objects,
                if immersive.lfe { " and an LFE" } else { "" },
                immersive.dialogue_norm,
                immersive.mix_level + 70,
            );
        }
    }
    for (which, entry) in unit.directory.iter().enumerate() {
        let substream = &unit.substreams[which];
        print!(
            "  substream {which}  {} bytes, end {} words",
            substream.bytes, entry.end_words
        );
        if entry.extra_word {
            print!(
                ", drc gain {} time {}",
                entry.drc_gain_update, entry.drc_time_update
            );
        }
        println!();
        if let Some(restart) = &substream.restart {
            println!(
                "               sync {:#06x}, channels {}..{}, matrix span {}, timing bit {}, \
                 assignment {:?}",
                restart.sync.word(),
                restart.min_channel,
                restart.max_channel,
                restart.max_matrix_channel,
                u8::from(restart.hires_output_timing),
                restart.channel_assignment
            );
            if !substream.output_shift.is_empty() {
                println!("               output shift {:?}", substream.output_shift);
            }
            for matrix in &substream.matrices {
                println!(
                    "               matrix -> {:>2} at {} bits, shift {}, {} bypassed: {}{}",
                    matrix.dest,
                    matrix.frac_bits,
                    matrix.shift,
                    matrix.bypassed,
                    matrix
                        .coefficients
                        .iter()
                        .enumerate()
                        .map(|(source, _)| format!("{:+.3}", matrix.gain(source)))
                        .collect::<Vec<_>>()
                        .join(" "),
                    if matrix.delta.iter().any(|step| *step != 0) {
                        format!(
                            " moving by {:?} a unit",
                            matrix.delta.iter().collect::<Vec<_>>()
                        )
                    } else {
                        String::new()
                    }
                );
            }
        }
    }
    if unit.extra_data > 0 {
        print!("  extra data   {} bytes", unit.extra_data);
        match &unit.evolution {
            Some(extra) => {
                println!(
                    ", evolution v{} key {}, {} declared, protection {}+{} bits{}{}{}",
                    extra.version,
                    extra.key,
                    extra.declared_bytes,
                    extra.protection.0,
                    extra.protection.1,
                    if extra.protection_bytes.is_empty() {
                        String::new()
                    } else {
                        format!(
                            " [{}]",
                            extra
                                .protection_bytes
                                .iter()
                                .map(|byte| format!("{byte:02x}"))
                                .collect::<String>()
                        )
                    },
                    if extra.check_nibble_ok {
                        ""
                    } else {
                        ", BAD NIBBLE"
                    },
                    if extra.parity_ok { "" } else { ", BAD PARITY" },
                );
                for payload in &extra.payloads {
                    println!(
                        "               payload {} ({}), {} bytes{}",
                        payload.id,
                        match payload.id {
                            11 => "object audio metadata",
                            _ => "not named here",
                        },
                        payload.bytes.len(),
                        match payload.sample_offset {
                            Some(at) => format!(", at sample {at}"),
                            None => String::new(),
                        }
                    );
                    if payload.id == 11 {
                        print_objects(&payload.bytes);
                    }
                }
            }
            None => println!(", opaque"),
        }
    }
}
