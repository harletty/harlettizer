//! Encode a master as an overlay, decode what came out, and check that the
//! arithmetic the mode promises is the arithmetic the stream carries.
//!
//! ```text
//! cargo xtask overlay-check a.atmos --sources 1
//! cargo xtask overlay-check a.atmos --sources 1 --source-pos 0,1,0
//! ```
//!
//! Needs `truehdd` on the path, and runs from the workspace.
//!
//! # What it checks, and why it needs the encoder's help
//!
//! Two claims:
//!
//! - an element **no source reached** carries the master's samples unchanged;
//! - one a source **did** reach carries those samples *plus* the sources,
//!   under the weights the fit chose, ramped across the block.
//!
//! The first can be checked from the master and the decoded stream alone. The
//! second cannot: the weights are what the fit decided, and a checker that
//! re-derived them would be checking the fit against itself. So the encoder
//! writes down what it did — `encode --overlay-report` — and this recomputes
//! the mix with [`hz_cluster::mix::mix`], the same function the encoder used,
//! and compares sample for sample.
//!
//! That account is also what says **which** elements were supposed to be
//! untouched. The first version of this decided it from the difference, so an
//! element the encoder had corrupted was counted as one that carried a source
//! and reported as a success: the check could not fail.
//! `a_changed_element_is_caught` is the test that it can.
//!
//! # The channel order
//!
//! A stream carries the low frequency channel first and the master's elements
//! after it, in the master's order — so element `e` of the output is **not**
//! channel `e` of the input, and comparing them that way silently checks four
//! channels against the wrong source on any master whose bed puts the low
//! frequency channel in the middle, which is where a 7.1 bed puts it. The
//! mapping comes from the account's header — one `element` line per element,
//! written before any block — which is the one place that knows it. It is
//! **not** recovered from a `copy` line: an element mixed on every block is
//! never copied, and a checker that learnt the mapping from copies skipped
//! exactly that element, silently, and called the result a success.
//! `an_element_mixed_throughout_is_not_skipped` is the test that it cannot.

use hz_core::{Error, Result};
use hz_io::container::caf::CafReader;
use hz_io::master::{Config, EventStream, MasterSet};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

/// What a sample is, as the codec's domain counts them.
const FULL_SCALE: f64 = 8_388_608.0;

pub struct Options {
    /// The master set to encode.
    pub input: PathBuf,
    /// How many of its trailing objects are sources — `--overlay K`.
    pub sources: usize,
    /// Round the mixed elements to this many bits. Twenty-four by default:
    /// the check is about what the fold did, not about its rounding.
    pub depth: u32,
    /// Put each trailing source at a fixed place before encoding, as
    /// `x,y,z[;x,y,z…]`, so that a sweep of positions is one command.
    pub source_pos: Option<String>,
    /// Extra arguments for `harlettizer encode`, as a caller would pass them.
    pub options: Vec<String>,
    /// Keep the encode and its account instead of deleting them.
    pub keep: bool,
}

pub fn run(options: Options) -> Result<()> {
    // Under `target/`, never the system temporary directory: this writes
    // programme audio, and the workspace keeps programme audio out of `/tmp`.
    let scratch = PathBuf::from("target/overlay-check");
    std::fs::create_dir_all(&scratch).map_err(|e| Error::io(&scratch, e))?;
    let stream = scratch.join("overlay.thd");
    let decoded = scratch.join("decoded");
    let account = scratch.join("report.txt");

    // The master to encode: the one given, or a copy with the sources pinned
    // where the caller asked.
    let master = match &options.source_pos {
        Some(spec) => pin_sources(&options.input, &scratch, options.sources, spec)?,
        None => options.input.clone(),
    };

    let cargo = std::env::var("CARGO").map_err(|_| {
        Error::unsupported(
            &options.input,
            "CARGO is not set, so the encoder cannot be built; run this as \
             `cargo xtask overlay-check`, from the workspace"
                .to_string(),
        )
    })?;
    let mut encode = Command::new(cargo);
    encode
        .args(["run", "--release", "--quiet", "-p", "hz-cli", "--"])
        .arg("encode")
        .arg(&master)
        .arg("--overlay")
        .arg(options.sources.to_string())
        .arg("--fold-depth")
        .arg(options.depth.to_string())
        .arg("--overlay-report")
        .arg(&account)
        .args(&options.options)
        .arg("--out")
        .arg(&stream);
    let encoded = encode.output().map_err(|e| {
        Error::unsupported(
            &options.input,
            format!("the encoder could not be run ({e}); this runs from the workspace"),
        )
    })?;
    println!("{}", String::from_utf8_lossy(&encoded.stdout).trim_end());
    let complaints = String::from_utf8_lossy(&encoded.stderr).into_owned();
    for line in complaints.lines().filter(|l| l.starts_with("error:")) {
        println!("{line}");
    }
    if !stream.is_file() {
        return Err(Error::unsupported(
            &options.input,
            format!(
                "the encode wrote no stream: {}",
                complaints.lines().last().unwrap_or("no reason given")
            ),
        ));
    }
    // A refusal is not a reason to stop looking: the guards are as much what
    // is being checked as the samples are, and a refused stream is left on
    // disk precisely so it can be measured.
    let refused = !encoded.status.success();
    // Before truehdd, which reads the widest substream and would take a
    // stream no player outside Dolby's can open.
    let playable = super::ffmpeg_check::check(&stream)?;

    let decode = Command::new("truehdd")
        .arg("decode")
        .arg(&stream)
        .arg("--output-path")
        .arg(&decoded)
        .arg("--presentation")
        .arg("3")
        .output()
        .map_err(|e| {
            Error::unsupported(
                &stream,
                format!("truehdd could not be run ({e}); it has to be on the path"),
            )
        })?;
    if !decode.status.success() {
        return Err(Error::unsupported(
            &stream,
            format!(
                "truehdd refused the stream: {}",
                String::from_utf8_lossy(&decode.stderr).trim()
            ),
        ));
    }

    let said = read_report(&account)?;
    let (input, input_channels) = channels_of(&master)?;
    let (output, output_channels) = channels_of(&with_suffix(&decoded))?;
    let report = compare(
        &said,
        &Signals {
            samples: &input,
            channels: input_channels,
        },
        &Signals {
            samples: &output,
            channels: output_channels,
        },
        options.sources,
        options.depth,
    )?;
    println!();
    println!("{}", report.say());
    println!(
        "{}",
        compare_metadata(&master, &with_suffix(&decoded), &said)?
    );

    if options.keep {
        println!("  kept in     {}", scratch.display());
    } else {
        // Everything this wrote, the decoded set and the pinned copy of the
        // master included: programme audio does not stay behind under
        // `target/` any more than it would under `/tmp`.
        let _ = std::fs::remove_file(&stream);
        let _ = std::fs::remove_file(&account);
        for suffix in [".atmos", ".atmos.metadata", ".atmos.audio"] {
            let mut name = decoded.as_os_str().to_os_string();
            name.push(suffix);
            let _ = std::fs::remove_file(PathBuf::from(name));
            if master != options.input {
                let mut name = master.with_extension("").into_os_string();
                name.push(suffix);
                let _ = std::fs::remove_file(PathBuf::from(name));
            }
        }
    }

    if report.failures > 0 {
        return Err(Error::unsupported(
            &options.input,
            format!(
                "{} elements are not what the overlay promised",
                report.failures
            ),
        ));
    }
    if !playable {
        return Err(Error::malformed(
            &stream,
            "FFmpeg, the decoder in every player that is not Dolby's, refuses the stream; \
             see above",
        ));
    }
    if refused {
        println!("  note        the encode refused on its own guards; the samples are still right");
    }
    Ok(())
}

/// `truehdd decode --output-path x` writes the set as `x.atmos`.
fn with_suffix(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(".atmos");
    PathBuf::from(name)
}

/// The encoder's account of itself — see `--overlay-report`.
#[derive(Default, Clone)]
pub struct Account {
    /// Per element, the master channel its own audio comes from — the
    /// header's `element` lines. `None` for an element the stream pads with.
    pub channels: Vec<Option<usize>>,
    pub blocks: Vec<BlockReport>,
}

impl Account {
    /// How many elements the account speaks of.
    pub fn elements(&self) -> usize {
        self.channels.len().max(
            self.blocks
                .iter()
                .map(|block| block.copy_from.len())
                .max()
                .unwrap_or(0),
        )
    }
}

/// One block of the encoder's account of itself — see `--overlay-report`.
#[derive(Default, Clone)]
pub struct BlockReport {
    pub at: u64,
    pub frames: usize,
    /// Whether the limiter acted, in which case the samples are not a plain
    /// sum and the mixed elements are not compared.
    pub limited: bool,
    /// Per element: the master channel it was copied from, or `None` when the
    /// block mixed it.
    pub copy_from: Vec<Option<usize>>,
    /// `(source, element, from, to)` — the weight, already divided by the
    /// element's stated gain, ramped across the block.
    pub weights: Vec<(usize, usize, f64, f64)>,
    /// The source's own gain, applied before the weight.
    pub gains: BTreeMap<usize, f64>,
}

fn read_report(path: &Path) -> Result<Account> {
    let text = std::fs::read_to_string(path).map_err(|e| Error::io(path, e))?;
    let mut channels: Vec<Option<usize>> = Vec::new();
    let mut out: Vec<BlockReport> = Vec::new();
    let number = |what: &str| -> Result<f64> {
        what.parse::<f64>()
            .map_err(|_| Error::malformed(path, format!("`{what}` is not a number")))
    };
    let place = |block: &mut BlockReport, element: usize, channel: Option<usize>| {
        if block.copy_from.len() <= element {
            block.copy_from.resize(element + 1, None);
        }
        block.copy_from[element] = channel;
    };
    for line in text.lines() {
        let word: Vec<&str> = line.split_whitespace().collect();
        match word.as_slice() {
            ["element", element, channel] => {
                let element = number(element)? as usize;
                if channels.len() <= element {
                    channels.resize(element + 1, None);
                }
                channels[element] = Some(number(channel)? as usize);
            }
            ["block", _, at, frames, limited] => out.push(BlockReport {
                at: number(at)? as u64,
                frames: number(frames)? as usize,
                limited: *limited != "0",
                ..BlockReport::default()
            }),
            ["copy", element, channel] => {
                let element = number(element)? as usize;
                let channel = number(channel)? as usize;
                let block = out
                    .last_mut()
                    .ok_or_else(|| Error::malformed(path, "an element before any block"))?;
                place(block, element, Some(channel));
            }
            ["mix", element] => {
                let element = number(element)? as usize;
                let block = out
                    .last_mut()
                    .ok_or_else(|| Error::malformed(path, "an element before any block"))?;
                place(block, element, None);
            }
            ["w", source, element, from, to] => {
                let one = (
                    number(source)? as usize,
                    number(element)? as usize,
                    number(from)?,
                    number(to)?,
                );
                out.last_mut()
                    .ok_or_else(|| Error::malformed(path, "a weight before any block"))?
                    .weights
                    .push(one);
            }
            ["g", source, gain] => {
                let (source, gain) = (number(source)? as usize, number(gain)?);
                out.last_mut()
                    .ok_or_else(|| Error::malformed(path, "a gain before any block"))?
                    .gains
                    .insert(source, gain);
            }
            [] => {}
            _ => return Err(Error::malformed(path, format!("cannot read `{line}`"))),
        }
    }
    if channels.is_empty() {
        return Err(Error::malformed(
            path,
            "no `element` lines: the account does not say which master channel each \
             element came from, so nothing mixed can be checked",
        ));
    }
    Ok(Account {
        channels,
        blocks: out,
    })
}

/// Interleaved samples and how wide they are.
pub struct Signals<'a> {
    pub samples: &'a [i32],
    pub channels: usize,
}

impl Signals<'_> {
    fn frames(&self) -> usize {
        self.samples.len() / self.channels.max(1)
    }
    fn at(&self, frame: usize, channel: usize) -> i32 {
        self.samples[frame * self.channels + channel]
    }
}

pub struct Report {
    pub elements: usize,
    pub untouched: usize,
    pub carried: usize,
    pub failures: usize,
    /// The worst a copied element differed from the master's own samples,
    /// which must be nothing at all.
    pub worst_copied: i64,
    /// And the worst a mixed one differed from the sum it was told to be, in
    /// steps of the fold's own depth.
    pub worst_mixed: f64,
    pub skipped: usize,
}

impl Report {
    pub fn say(&self) -> String {
        let mut out = format!(
            "  elements    {}, of which {} were never touched and {} carried a source\n",
            self.elements, self.untouched, self.carried
        );
        out.push_str(&format!(
            "  copied      worst difference {} sample steps from the master's own (must be 0)\n",
            self.worst_copied
        ));
        out.push_str(&format!(
            "  mixed       worst difference {:.2} steps of the fold's depth from what the \
             weights ask for\n",
            self.worst_mixed
        ));
        if self.skipped > 0 {
            out.push_str(&format!(
                "  skipped     {} blocks the limiter acted on, which are not a plain sum\n",
                self.skipped
            ));
        }
        out.push_str(&format!(
            "  verdict     {}",
            if self.failures == 0 {
                "every element is what the overlay promised"
            } else {
                "SOME ELEMENTS ARE NOT WHAT THE OVERLAY PROMISED"
            }
        ));
        out
    }
}

/// Check the decoded stream against the master and the encoder's account.
pub fn compare(
    account: &Account,
    input: &Signals,
    output: &Signals,
    sources: usize,
    depth: u32,
) -> Result<Report> {
    let blocks = &account.blocks;
    let elements = account.elements();
    if output.channels != elements {
        return Err(Error::unsupported(
            Path::new("the decoded set"),
            format!(
                "{} decoded channels for {elements} elements",
                output.channels
            ),
        ));
    }
    if sources > input.channels {
        return Err(Error::unsupported(
            Path::new("the master"),
            format!("{sources} sources asked of {} channels", input.channels),
        ));
    }
    // One step of the fold's depth, in samples of the codec's domain.
    let step = f64::from(1u32 << (24 - depth.min(24)));
    let first_source = input.channels - sources;
    let in_frames = input.frames();
    let out_frames = output.frames();

    let mut report = Report {
        elements,
        untouched: 0,
        carried: 0,
        failures: 0,
        worst_copied: 0,
        worst_mixed: 0.0,
        skipped: 0,
    };
    let mut ever_mixed = vec![false; elements];
    let mut bad = vec![false; elements];
    let mut unmapped = vec![false; elements];

    // Buffers for recomputing one element's mix, reused across blocks.
    let mut signals: Vec<Vec<f32>> = Vec::new();
    let mut gains: Vec<f64> = Vec::new();
    let mut from: Vec<Vec<f64>> = Vec::new();
    let mut to: Vec<Vec<f64>> = Vec::new();
    let mut mixed: Vec<Vec<f32>> = vec![Vec::new()];

    for block in blocks {
        let start = block.at as usize;
        let frames = block
            .frames
            .min(in_frames.saturating_sub(start))
            .min(out_frames.saturating_sub(start));
        if frames == 0 {
            continue;
        }
        if block.limited {
            report.skipped += 1;
        }
        for element in 0..elements {
            match block.copy_from.get(element).copied().flatten() {
                // Copied: the master's own samples, unchanged. No tolerance —
                // that is the whole claim.
                Some(channel) => {
                    for frame in 0..frames {
                        let at = start + frame;
                        let want = i64::from(input.at(at, channel));
                        let got = i64::from(output.at(at, element));
                        let apart = (got - want).abs();
                        report.worst_copied = report.worst_copied.max(apart);
                        if apart != 0 {
                            bad[element] = true;
                        }
                    }
                }
                // Mixed: the element's own samples plus the sources, under the
                // weights the encoder says it used, ramped across the block by
                // the same function the encoder ramped them with.
                None => {
                    ever_mixed[element] = true;
                    if block.limited {
                        continue;
                    }
                    // Which master channel the element's own audio is: the
                    // account's header says, and an element it does not
                    // speak of cannot be checked — which is a failure, not
                    // a pass. See the module note on the channel order.
                    let Some(own) = account.channels.get(element).copied().flatten() else {
                        unmapped[element] = true;
                        bad[element] = true;
                        continue;
                    };
                    let rows = 1 + sources;
                    signals.resize_with(rows, Vec::new);
                    gains.clear();
                    from.clear();
                    to.clear();
                    for (row, signal) in signals.iter_mut().enumerate() {
                        let channel = if row == 0 {
                            own
                        } else {
                            first_source + row - 1
                        };
                        signal.clear();
                        signal.extend((0..frames).map(|frame| {
                            (f64::from(input.at(start + frame, channel)) / FULL_SCALE) as f32
                        }));
                        // The element carries itself at unity and never ramps;
                        // a source carries its own gain.
                        gains.push(if row == 0 {
                            1.0
                        } else {
                            block.gains.get(&(row - 1)).copied().unwrap_or(1.0)
                        });
                        let (was, now) = if row == 0 {
                            (1.0, 1.0)
                        } else {
                            weight_of(block, row - 1, element)
                        };
                        from.push(vec![was]);
                        to.push(vec![now]);
                    }
                    hz_cluster::mix::mix(&signals, &gains, &from, &to, frames, &mut mixed);
                    for frame in 0..frames {
                        let at = start + frame;
                        let want = f64::from(mixed[0][frame]) * FULL_SCALE;
                        let got = f64::from(output.at(at, element));
                        let apart = (got - want).abs() / step;
                        if apart > report.worst_mixed {
                            report.worst_mixed = apart;
                        }
                        // One step of the fold's depth for the rounding into
                        // it, and half a step either side of that for the
                        // single-precision the mix is carried in.
                        if apart > 1.5 {
                            bad[element] = true;
                        }
                    }
                }
            }
        }
    }

    for element in 0..elements {
        if ever_mixed[element] {
            report.carried += 1;
        } else {
            report.untouched += 1;
        }
        if unmapped[element] {
            report.failures += 1;
            println!(
                "  FAIL        element {element} was mixed but the account never said which \
                 master channel it came from, so it could not be checked"
            );
        } else if bad[element] {
            report.failures += 1;
            println!("  FAIL        element {element} is not what the encoder said it wrote");
        }
    }
    Ok(report)
}

fn weight_of(block: &BlockReport, source: usize, element: usize) -> (f64, f64) {
    block
        .weights
        .iter()
        .find(|(s, e, _, _)| *s == source && *e == element)
        .map_or((0.0, 0.0), |(_, _, from, to)| (*from, *to))
}

/// What the decoded stream states for each kept element, against what the
/// master states for the channel it came from.
///
/// The overlay's claim is that the elements' metadata is the *master's* — so
/// what it is compared against is the master, and not another encode of it.
///
/// # Two kinds of channel, and only one of them compares directly
///
/// A master's **object** channel carries a position, a gain, a size and the
/// rest, and a stream element carries the same fields: those compare move for
/// move, and that is the invariant.
///
/// A master's **bed** channel carries none of it. A bed is a speaker feed and
/// its place is the speaker's, so the master states no position at all —
/// while a stream has no beds beyond the low frequency channel and every
/// element must state where it is. Comparing the two raw says every bed
/// differs, which is true and means nothing. What is worth asking of a bed is
/// that the stream put it **at its speaker's place and left it there**, which
/// is what is checked instead.
fn compare_metadata(master: &Path, decoded: &Path, account: &Account) -> Result<String> {
    let theirs = MasterSet::open(master)?;
    let ours = MasterSet::open(decoded)?;
    let (theirs_events, ours_events) = (theirs.read_events(0)?, ours.read_events(0)?);
    let theirs_by_channel = ids_by_channel(&theirs.config);
    let ours_by_channel = ids_by_channel(&ours.config);
    let beds = bed_places(&theirs.config);
    let lfe = lfe_channel(&theirs.config);

    let (mut objects, mut beds_seen) = (0usize, 0usize);
    let mut differing = Vec::new();
    let mut adrift = Vec::new();
    let elements = account.elements();
    for element in 0..elements {
        let Some(channel) = account.channels.get(element).copied().flatten() else {
            continue;
        };
        let (Some(theirs_id), Some(ours_id)) = (
            theirs_by_channel.get(&channel),
            ours_by_channel.get(&element),
        ) else {
            continue;
        };
        match beds.get(&channel) {
            // A bed: it has to be at its speaker, and stay there.
            Some(place) => {
                beds_seen += 1;
                let coded = hz_meta::oamd::Position::from_master(*place);
                let stayed = ours_events
                    .events
                    .iter()
                    .filter(|event| event.id == Some(*ours_id))
                    .filter_map(|event| event.pos.as_ref())
                    .all(|pos| match pos.as_slice() {
                        [x, y, z] => hz_meta::oamd::Position::from_master([x.0, y.0, z.0]) == coded,
                        _ => false,
                    });
                if !stayed {
                    adrift.push(element);
                }
            }
            // An object: move for move.
            None => {
                // The low frequency channel has no position and is not
                // compared as one; it is a bed with nowhere to be.
                if Some(channel) == lfe {
                    continue;
                }
                objects += 1;
                if places_of(&theirs_events, *theirs_id) != places_of(&ours_events, *ours_id) {
                    differing.push(element);
                }
            }
        }
    }
    let mut out = String::new();
    out.push_str(&if differing.is_empty() {
        format!(
            "  metadata    {objects} moving elements are where the master put them, at the \
             sample it put them there"
        )
    } else {
        format!(
            "  metadata    {} of {objects} moving elements are not where or when the master \
             put them: {differing:?}",
            differing.len()
        )
    });
    if beds_seen > 0 {
        out.push('\n');
        out.push_str(&if adrift.is_empty() {
            format!(
                "              and {beds_seen} bed elements sit at their speakers' places \
                 throughout, which is all a bed states"
            )
        } else {
            format!(
                "              {} bed elements left their speakers: {adrift:?}",
                adrift.len()
            )
        });
    }
    Ok(out)
}

/// Which channel of the master is the low frequency one, which has no place.
fn lfe_channel(config: &Config) -> Option<usize> {
    let mut channel = 0usize;
    let presentation = config.presentations.first()?;
    for bed in &presentation.bed_instances {
        for one in &bed.channels {
            if hz_core::speakers::is_lfe_label(&one.channel) {
                return Some(channel);
            }
            channel += 1;
        }
    }
    None
}

/// Where each bed channel of the master sits, by channel index.
fn bed_places(config: &Config) -> BTreeMap<usize, [f64; 3]> {
    let mut out = BTreeMap::new();
    let mut channel = 0usize;
    if let Some(presentation) = config.presentations.first() {
        for bed in &presentation.bed_instances {
            for one in &bed.channels {
                if let Some(place) = hz_render::fold::bed_position(&one.channel) {
                    out.insert(channel, place);
                }
                channel += 1;
            }
        }
    }
    out
}

/// Each channel's object id, in the order the channels appear.
fn ids_by_channel(config: &Config) -> BTreeMap<usize, u32> {
    let mut out = BTreeMap::new();
    let mut channel = 0usize;
    if let Some(presentation) = config.presentations.first() {
        for bed in &presentation.bed_instances {
            for one in &bed.channels {
                out.insert(channel, one.id);
                channel += 1;
            }
        }
        for object in &presentation.objects {
            out.insert(channel, object.id);
            channel += 1;
        }
    }
    out
}

/// Where one object is, and when — as the wire would code it.
///
/// # Why only the place and the time
///
/// The obvious check is to compare every field of every event. It does not
/// work, and not because anything is wrong: a master set and a decoded stream
/// are two serialisations of overlapping information. The stream states every
/// field explicitly because the payload has a bit for each — `snap: false`,
/// `zones: all`, `trimBypass: false` — where the master omits whatever is
/// default, and a master emits an initialising event with no position at all
/// that a stream has no way to express. Comparing them raw says every object
/// differs, which is true and says nothing.
///
/// What **was** wrong, and is what this exists to catch, is the place and the
/// time: keyframes landing on block boundaries rather than where the master
/// put them, and keyframes sharing a block being dropped. Both are here, on
/// the wire's own grid, which is the resolution at which two positions are
/// the same position to any decoder.
fn places_of(events: &EventStream, id: u32) -> Vec<(u64, hz_meta::oamd::Position)> {
    events
        .events
        .iter()
        .filter(|event| event.id == Some(id))
        .filter_map(|event| {
            let pos = event.pos.as_ref()?;
            let [x, y, z] = pos.as_slice() else {
                return None;
            };
            Some((
                event.sample_pos.unwrap_or(0),
                hz_meta::oamd::Position::from_master([x.0, y.0, z.0]),
            ))
        })
        .collect()
}

/// Write a copy of the master with its trailing sources pinned at fixed
/// places, so that a sweep of positions is one command rather than a hand
/// edit of an event stream.
fn pin_sources(master: &Path, into: &Path, sources: usize, spec: &str) -> Result<PathBuf> {
    let set = MasterSet::open(master)?;
    let mut events = set.read_events(0)?;
    let ids = ids_by_channel(&set.config);
    let channels = ids.len();
    let places: Vec<[f64; 3]> = spec
        .split(';')
        .map(|one| {
            let axis: Vec<f64> = one
                .split(',')
                .filter_map(|value| value.trim().parse().ok())
                .collect();
            match axis.as_slice() {
                [x, y, z] => Ok([*x, *y, *z]),
                _ => Err(Error::unsupported(
                    master,
                    format!("`{one}` is not an x,y,z"),
                )),
            }
        })
        .collect::<Result<_>>()?;
    if places.is_empty() {
        return Err(Error::unsupported(master, "no positions given".to_string()));
    }

    // The sources keep only their earliest event — whatever sample it was at,
    // since a source whose first event is not at nought would otherwise lose
    // every event it has — moved to the start and pinned: a static source is
    // what a dub's voices are, and what the sweep is about.
    if sources > channels {
        return Err(Error::unsupported(
            master,
            format!("{sources} sources asked of {channels} channels"),
        ));
    }
    let pinned: Vec<u32> = (channels - sources..channels)
        .filter_map(|channel| ids.get(&channel).copied())
        .collect();
    for (index, id) in pinned.iter().enumerate() {
        let place = places[index.min(places.len() - 1)];
        let earliest = events
            .events
            .iter()
            .enumerate()
            .filter(|(_, event)| event.id == Some(*id))
            .min_by_key(|(_, event)| event.sample_pos.unwrap_or(0))
            .map(|(index, _)| index);
        let Some(keep) = earliest else {
            continue;
        };
        let keep_rank = events.events[..=keep]
            .iter()
            .filter(|event| event.id == Some(*id))
            .count();
        let kept = &mut events.events[keep];
        kept.sample_pos = Some(0);
        kept.pos = Some(place.iter().map(|v| hz_io::Num::from(*v)).collect());
        let mut seen = 0usize;
        events.events.retain(|event| {
            let own = event.id == Some(*id);
            seen += usize::from(own);
            !own || seen == keep_rank
        });
    }

    // The set is copied whole — the audio is the master's, only the events
    // change — so that the encoder reads one consistent thing.
    let stem = "pinned";
    let config_path = into.join(format!("{stem}.atmos"));
    let mut config = set.config.clone();
    if let Some(presentation) = config.presentations.first_mut() {
        presentation.metadata = format!("{stem}.atmos.metadata");
        presentation.audio = format!("{stem}.atmos.audio");
    }
    config.write(&config_path)?;
    events.write(&into.join(format!("{stem}.atmos.metadata")))?;
    let audio = set.components[0]
        .audio
        .path()
        .ok_or_else(|| Error::unsupported(master, "no audio component".to_string()))?;
    std::fs::copy(audio, into.join(format!("{stem}.atmos.audio")))
        .map_err(|e| Error::io(audio, e))?;
    Ok(config_path)
}

/// Every sample of a master set's audio component, interleaved, and how wide
/// it is.
fn channels_of(config: &Path) -> Result<(Vec<i32>, usize)> {
    let set = MasterSet::open(config)?;
    let audio = set
        .components
        .first()
        .and_then(|c| c.audio.path())
        .ok_or_else(|| Error::unsupported(config, "no audio component".to_string()))?;
    let mut reader = CafReader::open(audio)?;
    let channels = reader.format().channels as usize;
    let mut all = Vec::new();
    let mut block = vec![0i32; channels * 4096];
    loop {
        let frames = reader.read_frames(&mut block)?;
        if frames == 0 {
            break;
        }
        all.extend_from_slice(&block[..frames * channels]);
    }
    Ok((all, channels))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **A harness that has never failed has never been tested.** One kept
    /// element altered by a single sample step — the smallest change a stream
    /// can carry — and the check has to say so.
    ///
    /// The first version of this could not. It decided which elements were
    /// untouched *from the difference itself*, so a corrupted element was
    /// counted as one that had carried a source, and reported as a success.
    fn signals(samples: &[i32], channels: usize) -> Signals<'_> {
        Signals { samples, channels }
    }

    #[test]
    fn a_changed_element_is_caught() {
        // Two elements, one copied from channel 1 and one from channel 0 —
        // the permutation a real stream applies — and no source reaching
        // either.
        let account = Account {
            channels: vec![Some(1), Some(0)],
            blocks: vec![BlockReport {
                at: 0,
                frames: 4,
                limited: false,
                copy_from: vec![Some(1), Some(0)],
                weights: Vec::new(),
                gains: BTreeMap::new(),
            }],
        };
        let input: Vec<i32> = vec![10, 20, 11, 21, 12, 22, 13, 23];
        let whole: Vec<i32> = vec![20, 10, 21, 11, 22, 12, 23, 13];

        let clean =
            compare(&account, &signals(&input, 2), &signals(&whole, 2), 0, 24).expect("a report");
        assert_eq!(clean.failures, 0, "an untouched stream was called wrong");
        assert_eq!(clean.untouched, 2);

        let mut damaged = whole.clone();
        damaged[2] += 1; // one step, on the second frame of element 0
        let caught =
            compare(&account, &signals(&input, 2), &signals(&damaged, 2), 0, 24).expect("a report");
        assert_eq!(caught.failures, 1, "one sample step went unnoticed");
        assert_eq!(caught.worst_copied, 1);
    }

    /// And the mixed case: an element carrying a source at a stated weight is
    /// its own samples plus that source, and a stream that carries some other
    /// weight is caught. This is the half the old checker never attempted.
    #[test]
    fn an_element_that_carries_the_wrong_weight_is_caught() {
        // Channels: 0 is element 0's own, 1 is element 1's, 2 is the source.
        let account = Account {
            channels: vec![Some(0), Some(1)],
            blocks: vec![BlockReport {
                at: 0,
                frames: 4,
                limited: false,
                // Element 0 mixed, element 1 copied from channel 1.
                copy_from: vec![None, Some(1)],
                weights: vec![(0, 0, 0.5, 0.5)],
                gains: BTreeMap::from([(0, 1.0)]),
            }],
        };
        let mut input: Vec<i32> = Vec::new();
        for _ in 0..4 {
            input.extend_from_slice(&[100, 7, 1000]);
        }
        // Element 0 was told to carry itself plus half the source, at a
        // constant weight, so every sample is 100 + 500.
        let mut whole: Vec<i32> = Vec::new();
        for _ in 0..4 {
            whole.extend_from_slice(&[600, 7]);
        }

        let clean =
            compare(&account, &signals(&input, 3), &signals(&whole, 2), 1, 24).expect("a report");
        assert_eq!(clean.failures, 0, "a correct sum was called wrong");
        assert_eq!(clean.carried, 1, "the mixed element was not seen as one");

        let mut damaged = whole.clone();
        damaged[0] = 350; // as though the weight had been a quarter
        let caught =
            compare(&account, &signals(&input, 3), &signals(&damaged, 2), 1, 24).expect("a report");
        assert_eq!(caught.failures, 1, "a wrong weight went unnoticed");
    }

    /// An element mixed on every block is never copied, so no `copy` line
    /// names its channel. The previous checker recovered the mapping from
    /// copies, skipped such an element without a word, and called the result
    /// a success — on a static centre voice over a bed, which is the common
    /// case, the one element that mattered was the one never checked. Now
    /// the mapping is the header's, and an element the header does not name
    /// is a failure.
    #[test]
    fn an_element_mixed_throughout_is_not_skipped() {
        let block = BlockReport {
            at: 0,
            frames: 4,
            limited: false,
            copy_from: vec![None, Some(1)],
            weights: vec![(0, 0, 0.5, 0.5)],
            gains: BTreeMap::from([(0, 1.0)]),
        };
        let mut input: Vec<i32> = Vec::new();
        for _ in 0..4 {
            input.extend_from_slice(&[100, 7, 1000]);
        }
        let mut wrong: Vec<i32> = Vec::new();
        for _ in 0..4 {
            wrong.extend_from_slice(&[100, 7]); // the source never arrived
        }

        // With the header, the wrong sum is caught.
        let named = Account {
            channels: vec![Some(0), Some(1)],
            blocks: vec![block.clone()],
        };
        let caught =
            compare(&named, &signals(&input, 3), &signals(&wrong, 2), 1, 24).expect("a report");
        assert_eq!(
            caught.failures, 1,
            "a source that never arrived went unnoticed"
        );

        // Without it, the element cannot be checked, and that is a failure
        // too — not a pass.
        let unnamed = Account {
            channels: vec![None, Some(1)],
            blocks: vec![block],
        };
        let unchecked =
            compare(&unnamed, &signals(&input, 3), &signals(&wrong, 2), 1, 24).expect("a report");
        assert_eq!(unchecked.failures, 1, "an unmapped element passed");
    }

    /// The account's header is what carries the mapping; an account without
    /// one is refused at reading, not silently treated as all-copied.
    #[test]
    fn an_account_without_a_header_is_refused() {
        let dir = std::env::temp_dir().join(format!("overlay-check-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("a scratch directory");
        let path = dir.join("report.txt");
        std::fs::write(&path, "block 0 0 4 0\nmix 0\n").expect("written");
        assert!(read_report(&path).is_err(), "a headerless account was read");
        std::fs::write(&path, "element 0 2\nblock 0 0 4 0\nmix 0\n").expect("written");
        let account = read_report(&path).expect("a headed account");
        assert_eq!(account.channels, vec![Some(2)]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
