//! Development harness for harlettizer.
//!
//! Run with `cargo xtask <command>` (see `.cargo/config.toml` for the alias).
//! Everything here is a development tool: it is never shipped, and it is the
//! only place in the workspace allowed to know about local test material.

mod adm;
mod bench;
mod cluster;
mod container;
mod coords;
mod corpus;
mod drc;
mod ffmpeg_check;
mod loudness;
mod make_master;
mod mlp;
mod mounts;
mod overlay_check;
mod roundtrip;
mod speech;
mod thd;

use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Parser)]
#[command(name = "xtask", about = "harlettizer development harness")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Survey a directory of master sets and report what they can be used to
    /// test.
    Corpus {
        /// Directory of master sets to survey.
        #[arg(long)]
        root: PathBuf,

        /// Write the full per-master detail as JSON.
        #[arg(long, value_name = "PATH")]
        json: Option<PathBuf>,

        /// List every master, not just the summary.
        #[arg(long)]
        list: bool,

        /// Stop after this many masters. Useful while iterating.
        #[arg(long, value_name = "N")]
        limit: Option<usize>,

        /// Also check whether each master's source is still reachable.
        ///
        /// Off by default: it stats paths on other volumes. Paths behind an
        /// automount trigger that has not fired are reported without being
        /// touched — see `mounts.rs` for why touching them is not an option.
        #[arg(long)]
        sources: bool,
    },

    /// Read every master set under a root, write it back, and read it again.
    ///
    /// The phase 1 acceptance test. Reads about two gigabytes of YAML, so it
    /// is a command rather than a unit test.
    Roundtrip {
        /// Directory of master sets to survey.
        #[arg(long)]
        root: PathBuf,

        /// Stop after this many masters.
        #[arg(long, value_name = "N")]
        limit: Option<usize>,

        /// Skip the event streams, which are where the two gigabytes are.
        #[arg(long)]
        configs_only: bool,

        /// Also rewrite and compare each audio component. Gigabytes per master.
        #[arg(long)]
        audio: bool,
    },

    /// Rewrite audio containers and compare them with the originals.
    ///
    /// Points the readers and writers at files written by other tools, where
    /// a wrong assumption has something to disagree with.
    Container {
        /// The files to rewrite. `.caf` and `.audio` are read as CAF, the
        /// rest as RIFF/RF64/BW64.
        #[arg(required = true)]
        files: Vec<PathBuf>,
    },

    /// Print the polar/Cartesian conversion over a grid, as CSV.
    Coords {
        /// Degrees between sampled angles.
        #[arg(long, default_value_t = 9)]
        step: usize,
    },

    /// Measure loudness and true peak, to hold next to another meter.
    Loudness {
        #[arg(required = true)]
        files: Vec<PathBuf>,

        /// Index of the LFE channel, which BS.1770 excludes.
        #[arg(long)]
        lfe: Option<usize>,

        /// Indices of surround channels, weighted 1.41.
        #[arg(long, value_delimiter = ',')]
        surround: Vec<usize>,
    },

    /// Report on the speech gate, and calibrate its thresholds.
    ///
    /// Files given as `--speech` should be speech and `--other` should not;
    /// the harness prints what the gate made of each, and with `--features`
    /// the distributions the thresholds were read off.
    Speech {
        /// Material that is speech.
        #[arg(long, value_name = "PATH")]
        speech: Vec<PathBuf>,

        /// Material that is not.
        #[arg(long, value_name = "PATH")]
        other: Vec<PathBuf>,

        /// Channel to listen to. Defaults to the centre of a 5.1 or 7.1 file.
        #[arg(long)]
        channel: Option<usize>,

        /// Print the per-class feature distributions.
        #[arg(long)]
        features: bool,

        /// Lay the first speech file against the first other file at known
        /// loudnesses, and report what the gate recovers.
        #[arg(long)]
        mix: bool,

        /// Loudness to place the speech at, LUFS.
        #[arg(long, default_value_t = -27.0)]
        speech_lufs: f64,

        /// Loudness to place the other material at, LUFS.
        #[arg(long, default_value_t = -15.0)]
        other_lufs: f64,

        /// Write the mixture, so another meter can read it too.
        #[arg(long, value_name = "PATH")]
        out: Option<PathBuf>,

        /// Override the cepstral peak prominence threshold, dB.
        #[arg(long)]
        cpp: Option<f64>,

        /// Override the speech band ratio threshold.
        #[arg(long)]
        band_ratio: Option<f64>,

        /// Override the spectral flux threshold.
        #[arg(long)]
        flux: Option<f64>,

        /// Override the 4 Hz modulation threshold, dB. `-inf` disables it.
        #[arg(long)]
        modulation: Option<f64>,

        /// Override how far a frame must stand above the noise floor, dB.
        #[arg(long)]
        snr: Option<f64>,

        /// Override the fraction of a block that must be voice-like.
        #[arg(long)]
        block_voice: Option<f64>,

        /// Override the hangover, in 10 ms frames.
        #[arg(long)]
        hangover: Option<usize>,
    },

    /// Encode a file losslessly and check a decoder gets it back exactly.
    Mlp {
        input: PathBuf,

        /// Where to write the stream. Defaults to the input with `.thd`.
        #[arg(long, value_name = "PATH")]
        out: Option<PathBuf>,

        /// Stop after this many frames.
        #[arg(long, value_name = "N")]
        frames: Option<u64>,

        /// Ask every presentation for this much dynamic range gain, in
        /// decibels, in the substream directory. Metadata only: a decoder
        /// asked for full range ignores it and the samples are unchanged.
        #[arg(long, value_name = "DB", allow_negative_numbers = true)]
        drc: Option<f64>,

        /// How long a dynamic range value stands, as a power of two access
        /// units. Zero restates it in every unit, seven every 128.
        #[arg(long, value_name = "N", default_value_t = 4)]
        drc_refresh: u32,

        /// Carry object audio metadata in the access units' extra data, and
        /// check a decoder gives back the objects it describes. Sixteen
        /// channels only, which is what declares an object programme.
        #[arg(long)]
        objects: bool,

        /// Write the stream without decoding it back.
        #[arg(long)]
        no_verify: bool,

        /// State a two-channel presentation on the first substream, and check
        /// the full decode is still bit exact.
        #[arg(long)]
        presentation: bool,

        /// Search less hard: one second-filter design, decided at every
        /// other restart. A tenth of a per cent of the stream for half the
        /// time.
        #[arg(long)]
        fast: bool,
    },

    /// Render a master and each rule's fold of it on 7.1.4 as blinded
    /// stimuli for a listening session, or score a filled-in sheet.
    Bench {
        input: PathBuf,

        /// Where the stimuli, the key and the sheet go.
        #[arg(long, value_name = "DIR")]
        out: PathBuf,

        /// Elements the bitstream carries, which is what `harlettizer encode
        /// --cluster` means — the low frequency channel is one of them.
        ///
        /// It takes an element and is not clustered: it is not a direction,
        /// and nothing here has any say about where it goes. So a master with
        /// one folds into `N - 1` clustered elements, and a report from this
        /// harness is a report about the stream the encoder writes. Before
        /// September 2026 this counted the clustered elements only, so every
        /// table in `docs/clustering.md` written then names one fewer element
        /// than the stream it describes.
        #[arg(long, value_name = "N", default_value_t = 12)]
        elements: usize,

        /// Samples a block: the encoder's.
        #[arg(long, value_name = "SAMPLES", default_value_t = 1280)]
        block: u64,

        /// How many seconds to render, from the start. The whole master
        /// when absent.
        #[arg(long, value_name = "S")]
        seconds: Option<f64>,

        /// A condition to render, `loudness/weighting`: `flat`, `kweighted`
        /// or `perceptual`, and `fitted`, `nearest` or `spread`. Repeatable.
        #[arg(long = "condition", value_name = "L/W")]
        conditions: Vec<String>,

        /// What the blinding is shuffled by.
        #[arg(long, value_name = "N", default_value_t = 1)]
        seed: u64,

        /// A filled-in sheet to score against the key in `--out`, instead
        /// of making stimuli.
        #[arg(long, value_name = "CSV")]
        score: Option<PathBuf>,
    },

    /// Fold a master set's objects into the elements a bitstream carries, and
    /// say what it cost.
    Cluster {
        input: PathBuf,

        /// Elements to fold into.
        #[arg(long, value_name = "N", default_value_t = 12)]
        elements: usize,

        /// Metadata blocks a second.
        #[arg(long, value_name = "N", default_value_t = 25)]
        rate: u32,

        /// Samples a block, in place of `--rate`.
        ///
        /// What the encoder's own block is, so that the two can be pointed at
        /// the same master and compared: `harlettizer encode` reconsiders the
        /// elements once every 32 access units, which is 1280 samples, and no
        /// whole number of blocks a second is that.
        #[arg(long, value_name = "SAMPLES")]
        block: Option<u64>,

        /// How to share an object's energy out between the elements.
        #[arg(long, value_name = "RULE", default_value = "fitted")]
        weighting: String,

        /// Passes of the search that places each element on the fold's own
        /// cost. Zero declines it.
        ///
        /// `harlettizer encode --fold-search` is the same knob, so that a
        /// report from here is a report about a fold the encoder would run.
        #[arg(long, value_name = "PASSES", default_value_t = hz_cluster::place::PASSES)]
        search: usize,

        /// How a block's power is weighed before it steers the fold: `flat`,
        /// `kweighted` or `perceptual`.
        ///
        /// Whatever it is, the fold is judged on the other ruler as well —
        /// the perceptual one for a flat fold and the flat one for a
        /// perceptual fold — so that what the rule gains can be told from
        /// what the ruler moves. See `hz_cluster::scene`.
        #[arg(long, value_name = "RULE", default_value = "flat")]
        loudness: String,

        /// Bands of the perceptual rule, on the ERB-rate scale.
        #[arg(long, value_name = "N", default_value_t = hz_cluster::scene::PERCEPTION.bands)]
        bands: usize,

        /// What masks an object under the perceptual rule: `global` or
        /// `local`.
        #[arg(long, value_name = "RULE", default_value = "local")]
        masker: String,

        /// How the energies the placement is steered by are read from the
        /// blocks around each one: `hold` or `mean`. See
        /// `hz_cluster::smooth`.
        #[arg(long, value_name = "RULE", default_value = "hold")]
        smooth: String,

        /// Blocks behind the one being placed that count.
        #[arg(long, value_name = "N", default_value_t = hz_cluster::smooth::SMOOTHING.behind)]
        behind: usize,

        /// Blocks ahead of the one being placed that count — each a block of
        /// latency in the energy.
        #[arg(long, value_name = "N", default_value_t = hz_cluster::smooth::SMOOTHING.ahead)]
        ahead: usize,

        /// How much louder a block ahead has to be before a hold counts it,
        /// as a factor on the energy. One counts every block ahead.
        #[arg(long, value_name = "FACTOR", default_value_t = hz_cluster::ARRIVING)]
        arriving: f64,

        /// Whether objects that render differently — snap, zones, elevation,
        /// screen — are kept in elements of their own: `on` or `off`. See
        /// `hz_cluster::class`.
        #[arg(long, value_name = "SWITCH", default_value = "on")]
        classes: String,

        /// How much more a class has to gain from an element than another
        /// loses before the element changes class between blocks.
        #[arg(long, value_name = "FRACTION", default_value_t = hz_cluster::class::WORTH)]
        class_worth: f64,

        /// What to do with a bed channel other than the LFE: `pinned` to its
        /// speaker's place with an element of its own, or `free` as an
        /// object of the bed class sharing the elements.
        #[arg(long, value_name = "RULE", default_value = "pinned")]
        beds: String,

        /// Where the elements start from on a cold block: `direction`, the
        /// loudest then the worst served; or `capture`, the object an element
        /// on which would serve the most not yet served, by the overlap of
        /// rendered gain vectors. See `hz_cluster::seed`.
        #[arg(long, value_name = "RULE", default_value = "direction")]
        seeding: String,

        /// How much better the best place has to be than the one an element
        /// was in last block before it goes there, as a fraction of what
        /// staying costs. Nought lets it move for any gain at all.
        ///
        /// The dead band of `hz_cluster::place::HOLDING`. It is not a cap on
        /// travel — that was built, measured and rejected — the element
        /// either stays exactly where it was or goes the whole way.
        #[arg(long, value_name = "FRACTION", default_value_t = hz_cluster::place::HOLDING)]
        holding: f64,

        /// Whether the placement search offers each element the position of
        /// its dominant member as a candidate, beside the six steps: `on` or
        /// `off`. See `hz_cluster::place::DOMINANT`.
        #[arg(long, value_name = "SWITCH", default_value = "off")]
        dominant: String,

        /// The programme's dialnorm in decibels, which sets the absolute
        /// floor under what an object has to carry to be heard at the
        /// playback level; or `relative`, the floor as it was, forty decibels
        /// under the block's loudest and nothing else. See
        /// `hz_cluster::floor`.
        #[arg(
            long,
            value_name = "DIALNORM",
            default_value = "-31",
            allow_hyphen_values = true
        )]
        floor: String,

        /// Whether the fit bounds an element's coherent peak to the codec's
        /// domain: `on` or `off`. See `hz_cluster::headroom`.
        #[arg(long, value_name = "SWITCH", default_value = "off")]
        headroom: String,

        /// Report every block rather than the summary.
        #[arg(long)]
        all: bool,
    },

    /// What gain a shipped stream states, and when it changes it.
    Drc {
        input: PathBuf,

        /// One line per word rather than the summary.
        #[arg(long)]
        each: bool,

        /// Stop after this many access units.
        #[arg(long, value_name = "N")]
        units: Option<usize>,

        /// The decoded presentation, raw 24-bit little-endian PCM, to join the
        /// gains to the level they answer.
        #[arg(long, value_name = "FILE")]
        audio: Option<PathBuf>,

        /// How many channels that presentation carries.
        #[arg(long, value_name = "N", default_value_t = 2)]
        channels: usize,

        /// Which substream's words to join.
        #[arg(long, value_name = "N", default_value_t = 0)]
        substream: usize,

        /// The level detector's time constant, in milliseconds.
        #[arg(long, value_name = "MS", default_value_t = 700.0)]
        tau: f64,

        /// Hold each word to a measured curve at the decoded level — `wide`
        /// or `stereo` — and find the offset in units at which they agree
        /// best. Needs `--audio`.
        #[arg(long, value_name = "CURVE")]
        against: Option<String>,
    },

    /// Read a TrueHD stream's structure and check its integrity.
    Thd {
        input: PathBuf,

        /// Stop after this many access units.
        #[arg(long, value_name = "N")]
        units: Option<usize>,

        /// Print every unit, not just the first.
        #[arg(long)]
        all: bool,

        /// The settings file holding the key to check each Evolution frame's
        /// protection field against. Defaults to the command line's own
        /// (`$HARLETTIZER_CONFIG`, `~/.config/harlettizer/config.yaml`);
        /// without a key the fields are counted and not checked.
        #[arg(long, value_name = "PATH")]
        config: Option<PathBuf>,
    },

    /// Check that streams are ones every player can open: count each
    /// substream's matrices against what a decoder reads, and decode the
    /// whole stream with FFmpeg, failing on any line it prints.
    FfmpegCheck {
        #[arg(required = true)]
        inputs: Vec<PathBuf>,
    },

    /// Write a small synthetic master file set to convert against.
    MakeMaster {
        /// Path of the `.atmos` config; the other components sit beside it.
        output: PathBuf,

        #[arg(long, default_value_t = 4)]
        objects: u32,

        #[arg(long, default_value_t = 2)]
        seconds: u32,

        /// Write only the low frequency channel as a bed, which is the shape
        /// `harlettizer encode` carried before beds were folded. The same as
        /// `--bed lfe`.
        #[arg(long)]
        lfe_only: bool,

        /// Which bed to write: `left`, a left channel and the LFE; `lfe`, the
        /// LFE alone; or `7.1.2`, the bed an authored master has.
        #[arg(long, value_name = "KIND", default_value = "left")]
        bed: String,

        /// What the objects carry: `tones`, a different pure tone each;
        /// `broadband`, a voice, a rumble and effects made of shaped noise,
        /// some of them wide; or `bursts`, the same with the effects switching
        /// on and off.
        #[arg(long, value_name = "KIND", default_value = "tones")]
        scene: String,

        /// Snap every Nth object to the nearest speaker, which makes it a
        /// class of its own. Nought snaps none.
        #[arg(long, value_name = "N", default_value_t = 0)]
        snap_every: u32,

        /// Confine every Nth object to the front of the room (`no back`),
        /// which makes it a class of its own. Nought confines none.
        #[arg(long, value_name = "N", default_value_t = 0)]
        zone_every: u32,

        /// Give the first N objects one and the same tone, loud, so that an
        /// element carrying two of them passes full scale: the scene a fold
        /// has to keep inside the codec's domain. Nought gives none.
        #[arg(long, value_name = "N", default_value_t = 0)]
        coherent: u32,
    },

    /// Encode a master as an overlay, decode it with truehdd, and check that
    /// the kept elements really were kept.
    ///
    /// The invariant check the overlay mode rests on, made against somebody
    /// else's decoder rather than against our own reader. Needs `truehdd` on
    /// the path.
    OverlayCheck {
        /// The master set's `.atmos` config.
        input: PathBuf,

        /// How many of its trailing objects are sources.
        #[arg(long, value_name = "K", default_value_t = 1)]
        sources: usize,

        /// Round the mixed elements to this many bits.
        #[arg(long, value_name = "BITS", default_value_t = 24)]
        depth: u32,

        /// Put each trailing source at a fixed place before encoding, as
        /// `x,y,z` or `x,y,z;x,y,z…`, so that a sweep of positions is one
        /// command rather than a hand edit of an event stream.
        #[arg(long, value_name = "POS")]
        source_pos: Option<String>,

        /// Keep the encode and its account under `target/overlay-check/`.
        #[arg(long)]
        keep: bool,

        /// Anything else to hand `harlettizer encode`, as a caller would.
        #[arg(last = true)]
        options: Vec<String>,
    },

    /// Read the ADM out of BW64 files and report what was not understood.
    Adm {
        #[arg(required = true)]
        files: Vec<PathBuf>,

        /// Print every block of every channel format.
        #[arg(long)]
        blocks: bool,

        /// Also rewrite the ADM through this crate's model and compare.
        #[arg(long)]
        check: bool,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.command {
        Command::Corpus {
            root,
            json,
            list,
            limit,
            sources,
        } => corpus::run(corpus::Options {
            root,
            json,
            list,
            limit,
            sources,
        }),
        Command::Roundtrip {
            root,
            limit,
            configs_only,
            audio,
        } => roundtrip::run(roundtrip::Options {
            root,
            limit,
            configs_only,
            audio,
        }),
        Command::Container { files } => container::run(container::Options { files }),
        Command::Coords { step } => coords::run(coords::Options { step }),
        Command::Loudness {
            files,
            lfe,
            surround,
        } => loudness::run(loudness::Options {
            files,
            lfe,
            surround,
        }),
        Command::Speech {
            speech,
            other,
            channel,
            features,
            mix,
            speech_lufs,
            other_lufs,
            out,
            cpp,
            band_ratio,
            flux,
            modulation,
            snr,
            block_voice,
            hangover,
        } => speech::run(speech::Options {
            speech,
            other,
            channel,
            features,
            mix,
            speech_lufs,
            other_lufs,
            out,
            cpp,
            band_ratio,
            flux,
            modulation,
            snr,
            block_voice,
            hangover,
        }),
        Command::Mlp {
            input,
            out,
            frames,
            drc,
            drc_refresh,
            objects,
            no_verify,
            presentation,
            fast,
        } => mlp::run(mlp::Options {
            input,
            out,
            frames,
            drc,
            drc_refresh,
            objects,
            no_verify,
            presentation,
            fast,
        }),
        Command::Bench {
            input,
            out,
            elements,
            block,
            seconds,
            conditions,
            seed,
            score,
        } => bench::run(bench::Options {
            input,
            out,
            elements,
            block,
            seconds,
            conditions,
            seed,
            score,
        }),
        Command::Cluster {
            input,
            elements,
            rate,
            block,
            weighting,
            search,
            loudness,
            bands,
            masker,
            smooth,
            behind,
            ahead,
            arriving,
            classes,
            class_worth,
            beds,
            seeding,
            holding,
            dominant,
            floor,
            headroom,
            all,
        } => cluster::run(cluster::Options {
            input,
            elements,
            rate,
            block,
            weighting,
            search,
            loudness,
            bands,
            masker,
            smooth,
            behind,
            ahead,
            arriving,
            classes,
            class_worth,
            beds,
            seeding,
            holding,
            dominant,
            floor,
            headroom,
            all,
        }),
        Command::Drc {
            input,
            each,
            units,
            audio,
            channels,
            substream,
            tau,
            against,
        } => drc::run(drc::Options {
            input,
            each,
            units,
            audio,
            channels,
            substream,
            tau,
            against,
        }),
        Command::Thd {
            input,
            units,
            all,
            config,
        } => thd::run(thd::Options {
            input,
            units,
            all,
            config,
        }),
        Command::FfmpegCheck { inputs } => ffmpeg_check::run(ffmpeg_check::Options { inputs }),
        Command::MakeMaster {
            output,
            objects,
            seconds,
            lfe_only,
            bed,
            scene,
            snap_every,
            zone_every,
            coherent,
        } => make_master::run(make_master::Options {
            output,
            objects,
            seconds,
            bed: if lfe_only { "lfe".to_string() } else { bed },
            scene,
            snap_every,
            zone_every,
            coherent,
        }),
        Command::OverlayCheck {
            input,
            sources,
            depth,
            source_pos,
            keep,
            options,
        } => overlay_check::run(overlay_check::Options {
            input,
            sources,
            depth,
            source_pos,
            keep,
            options,
        }),
        Command::Adm {
            files,
            blocks,
            check,
        } => {
            let checked = if check { files.clone() } else { Vec::new() };
            adm::run(adm::Options { files, blocks }).and_then(|()| {
                for file in &checked {
                    adm::rewrite_check(file)?;
                }
                Ok(())
            })
        }
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("xtask: {e}");
            ExitCode::FAILURE
        }
    }
}
