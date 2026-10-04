//! The `harlettizer` command line.

mod convert;
mod encode;
mod iamf;
mod iamf_objects;

use clap::{Parser, Subcommand};
use hz_programme::mix::{self, Headroom, Mixing};
use hz_programme::overlay;
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Parser)]
#[command(
    name = "harlettizer",
    about = "An open encoding engine for immersive audio",
    version
)]
struct Cli {
    /// Local settings file, for what is this machine's rather than the
    /// programme's: the key Evolution frames are signed with.
    ///
    /// Defaults to `$HARLETTIZER_CONFIG`, then
    /// `$XDG_CONFIG_HOME/harlettizer/config.yaml`, then
    /// `~/.config/harlettizer/config.yaml`; a default that is absent is no
    /// settings, a path given here has to exist.
    #[arg(long, global = true, value_name = "PATH")]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Convert between a master file set and an ADM BW64 file.
    ///
    /// The direction is taken from the input: a `.atmos` config in, a BW64
    /// file out; a BW64 file in, a master set out.
    Convert {
        /// The master set's `.atmos` config, or an ADM BW64 file.
        input: PathBuf,

        /// The BW64 file to write, or the `.atmos` config of the set to write.
        output: PathBuf,

        /// Which presentation of the master set to convert.
        #[arg(long, default_value_t = 0)]
        presentation: usize,
    },

    /// Encode a programme as a TrueHD stream.
    ///
    /// Takes a master set or an ADM BW64 file and writes the immersive
    /// presentation: the elements coded losslessly, and the master's own
    /// object metadata carried in the access units alongside them.
    Encode {
        /// A master set's `.atmos` config, or an ADM BW64 file.
        input: PathBuf,

        /// Write the stream here.
        #[arg(long)]
        out: PathBuf,

        /// Stop after this many frames of audio.
        #[arg(long)]
        frames: Option<u64>,

        /// Read the master's audio from one mono WAV per waveform,
        /// `<PREFIX>_<n>.wav` with n from 0 in the master's own order (its
        /// bed, then its objects), rather than from the interleaved file its
        /// header names.
        ///
        /// For a master that is being made rather than one that was
        /// delivered: decoded (`harletty decode --mono-prefix` writes exactly
        /// this), re-voiced, re-mixed. Interleaving the waveforms only for
        /// this to read them back apart costs a copy of the programme — tens
        /// of gigabytes for a film. Every file must be mono integer PCM of
        /// one rate, one width and one length. Only for a master set.
        #[arg(long, value_name = "PREFIX")]
        mono_prefix: Option<PathBuf>,

        /// Search less hard: one second-filter design, decided at every
        /// other restart.
        ///
        /// About a quarter off the time here, for a fraction of a per cent of
        /// stream — the encoder alone halves, but a master set's fold,
        /// metadata and I/O are work this does not touch. Measured in
        /// `docs/encode.md`.
        #[arg(long)]
        fast: bool,

        /// Fold the master's objects into this many elements, rather than
        /// giving each object one of its own.
        ///
        /// A delivery bitstream carries twelve, fourteen or sixteen. Without
        /// this the stream is one element per object, which only works because
        /// every master to hand is already the output of somebody else's
        /// clustering.
        #[arg(long, value_name = "ELEMENTS")]
        cluster: Option<usize>,

        /// Keep the master's elements exactly as they are and pan its last
        /// this-many objects onto them.
        ///
        /// For a programme being re-voiced: the master carries the original's
        /// elements, in the original's order and with the original's metadata,
        /// and after them the new sources — a dubbed dialogue, one object per
        /// channel of the track it came from. This says how many of the
        /// trailing objects are those sources; everything before them is an
        /// element and is kept.
        ///
        /// What it buys over `--cluster`: the elements' metadata is the
        /// master's element for element, and an element no source reaches is
        /// *copied* — not mixed, not rounded, the same samples it arrived as.
        /// Handing the whole scene to `--cluster` instead re-places and
        /// re-quantises every element, including the ones nothing was added
        /// to, and lets a source pull a real element away from where the
        /// original put it.
        ///
        /// Mutually exclusive with `--cluster`. See `docs/encode.md`.
        #[arg(long, value_name = "SOURCES", conflicts_with = "cluster")]
        overlay: Option<usize>,

        #[command(flatten)]
        overlaid: OverlayArgs,

        #[command(flatten)]
        mixing: MixArgs,

        /// Passes of the search that places each element where the fold's own
        /// cost is lowest, rather than where the clustering of directions put
        /// it. Zero declines it.
        ///
        /// Only means anything with `--cluster`. It is worth about a fifth of
        /// what the fold costs — 0.044 to 0.036 on a forty-object scene into
        /// twelve elements — and about a twentieth of the stream, because a
        /// better fold spreads an object over more elements and a denser
        /// mixture is harder to code losslessly. Nothing either way on a
        /// master that is not really folded.
        ///
        /// Four is where the error stops falling; the clustering costs 58 µs a
        /// block at zero and 268 at four, which over a two-hour programme is
        /// 48 s against the encoder's own 21 minutes. See `docs/clustering.md`.
        #[arg(long, value_name = "PASSES", default_value_t = hz_cluster::place::PASSES)]
        fold_search: usize,

        /// What a fold does with a bed channel other than the LFE: `pinned`,
        /// an element of its own at its speaker's place; `free`, an object
        /// of the bed class that shares the elements with the rest; or
        /// `auto`, pinned while every object can still have an element
        /// beside the bed and free otherwise.
        ///
        /// Only means anything with `--cluster`; without it a bed other than
        /// the LFE is refused. See `docs/clustering.md`.
        #[arg(long, value_name = "RULE", default_value = "auto")]
        beds: String,

        /// Blocks *behind* the one being placed whose energies steady it.
        ///
        /// The fold is steered by a window of blocks around the one it is
        /// placing rather than by that block alone — see
        /// `hz_cluster::smooth`. The blocks ahead are what an offline encoder
        /// can afford and are always taken; the blocks behind are a hold, and
        /// they are nought by default because on the synthesised scenes of
        /// `docs/clustering.md` they cost error on a contested twelve-element
        /// fold.
        ///
        /// On a real contested fold they do not: measured on a programme
        /// folded from fifteen objects into twelve elements, two blocks
        /// behind take better than a third off the excursions where an
        /// element leaves its place and comes back within a tenth of a
        /// second, for a mean error that does not move. So the window is a
        /// knob rather than a constant until a listening settles it. Only
        /// means anything with `--cluster`.
        #[arg(long, value_name = "N", default_value_t = hz_cluster::smooth::SMOOTHING.behind)]
        smooth_behind: usize,

        /// How much better somewhere else has to be before an element goes
        /// there, as a fraction of what staying costs.
        ///
        /// The dead band of `hz_cluster::place::HOLDING`. An element keeps
        /// the position it had unless the best place the fold's own cost can
        /// find beats staying by this much; it is not a cap on travel — that
        /// was built, measured and rejected — so the element stays exactly
        /// where it was or goes the whole way.
        ///
        /// Measured on a programme folded one element short of its sources,
        /// the shipped quarter takes the elements' wobble — leaving a place
        /// and coming back inside the two tenths of a second the ear takes to
        /// hear a movement — from 3.63 % of the windows an element is heard
        /// in to 0.84 %, for an error that does not move. What it costs is
        /// flips: the fit routes objects through a slightly stale position,
        /// so an element's active set changes oftener, and `--smooth-behind`
        /// is what pays that back. Nought turns the band off, which is how
        /// the two are told apart by ear. Only means anything with
        /// `--cluster`.
        #[arg(long, value_name = "FRACTION", default_value_t = hz_cluster::place::HOLDING)]
        fold_hold: f64,

        /// `measured`, a gain in decibels, or `off`.
        ///
        /// A decoder asked for compressed output applies the stream's dynamic
        /// range word; without one it has nothing to apply, which is what
        /// every stream this wrote before said. Zero — the default — states
        /// unity: no reduction asked for, which is a stated profile rather
        /// than an absent one.
        ///
        /// It is zero and not a curve because A/52 specifies the wire format
        /// of a gain and the intent behind it and stops there. The named
        /// characteristics are the reference encoder's own, and matching them
        /// means measuring them. See `docs/drc.md`.
        // `allow_hyphen_values`, or a negative gain — which is the only kind a
        // reduction is — reads as another flag and the command refuses it.
        #[arg(
            long,
            value_name = "MODE",
            default_value = "measured",
            allow_hyphen_values = true
        )]
        drc: String,

        /// Make the stream carry real folds: a decoder stopping after any
        /// substream is handed a 2.0, a 5.1 or a 7.1 rather than the leading
        /// elements.
        ///
        /// The channels stop being the elements and become a hierarchy of
        /// presentations — the first two carry the stereo fold itself, the next
        /// four what the 5.1 adds, and so on — with the last substream
        /// declaring what turns them back into elements. A full decode is
        /// unchanged and still lossless, and is checked against a real decoder.
        ///
        /// 🔴 Off by default because it cannot always be done. The cascade that
        /// builds the hierarchy amplifies what it is given — up to twice, and
        /// it declines past that — and the codec's domain is twenty-four bits,
        /// so a programme mixed near full scale has nowhere to put it and the
        /// stream is written without one. See `docs/mlp.md`.
        #[arg(long)]
        presentations: bool,

        /// Report how far along the encode is, on standard error.
        ///
        /// One `progress <percent>%` line each time the whole percent
        /// changes, so at most a hundred of them however long the programme
        /// is. For a caller driving a progress bar; the summary still goes to
        /// standard output and says nothing about it.
        #[arg(long)]
        progress: bool,
    },

    /// Render a programme to a 7.1.4 bed and write it as an IAMF sequence.
    ///
    /// Takes a master set or an ADM BW64 file. Its bed channels are routed to
    /// their speakers and its objects panned on the room's cube, and the bed is
    /// coded losslessly as one channel-based audio element under the IAMF
    /// v1.1 simple profile — what browsers and televisions decode. The mix
    /// presentation states the loudness measured on 7.1.4 and on the stereo
    /// pair a decoder folds it to. The output is a standalone `.iamf` stream,
    /// or a Matroska file whose one track is the sequence (`A_IAMF`) when
    /// `--out` ends in `.mka` or `.mkv`; `docs/iamf.md` has the FFmpeg line
    /// that puts the standalone stream in MP4.
    Iamf {
        /// A master set's `.atmos` config, or an ADM BW64 file.
        input: PathBuf,

        /// Write the stream here: into Matroska if it ends in `.mka` or
        /// `.mkv`, as a standalone IAMF stream otherwise.
        #[arg(long)]
        out: PathBuf,

        /// Stop after this many frames of audio.
        #[arg(long)]
        frames: Option<u64>,

        /// Read the master's audio from one mono WAV per waveform; see
        /// `encode --mono-prefix`.
        #[arg(long, value_name = "PREFIX")]
        mono_prefix: Option<PathBuf>,

        /// `flac`, lossless and about half the size; `lpcm`, the samples as
        /// they are; or `opus`, lossy, what a browser or YouTube is sent —
        /// only in a build with the `opus` feature, which links libopus.
        #[arg(long, default_value = "flac")]
        codec: String,

        /// Bits a sample for the lossless codecs: 16 or 24, and 32 for
        /// LPCM. FLAC stops at 24 because the decoders in use do. Opus has
        /// no depth, and takes the bed at 24.
        #[arg(long)]
        bits: Option<u32>,

        /// Opus only: kilobits a second for each channel. A coupled pair is
        /// coded at twice it and the LFE at a quarter, so 64 puts a 7.1.4 at
        /// about 720 kbit/s. YouTube's IAMF streams run at about 46.
        #[arg(long, value_name = "KBPS")]
        bitrate: Option<u32>,

        /// Samples a channel per temporal unit. Every FLAC frame is this
        /// long, so it is also the FLAC block size, 4096 unless asked and
        /// 4608 at most; an Opus packet is 960 unless asked — 20 ms — and
        /// 480, 1920 or 2880 otherwise.
        #[arg(long, value_name = "SAMPLES")]
        frame_size: Option<usize>,

        /// What a decoder playing to headphones does with the bed:
        /// `stereo`, the same fold as for two loudspeakers, or `binaural`,
        /// its own binaural renderer.
        #[arg(long, default_value = "stereo")]
        headphones: String,

        /// Carry the master's objects as IAMF v2.0 objects — each its own
        /// element, its path as position parameter blocks — rather than
        /// rendering them to a 7.1.4 bed. The master's bed channels, the LFE
        /// among them, travel as one channel-based element on the smallest
        /// IAMF layout that has them all; a bed channel no IAMF layout has (a
        /// top side, a wide) as an object that stands at its speaker. At most
        /// twenty-eight channels in all. Few decoders read v2.0 yet.
        #[arg(long)]
        objects: bool,

        /// With `--objects`: how positions are coded. `cart16`, the master's
        /// cube coordinates to a sixty-five-thousandth; `cart8`, to a
        /// hundred-and-twenty-seventh, for a third of the metadata; or
        /// `polar`, the cube put onto the sphere by BS.2127's conversion.
        #[arg(long, value_name = "CODING", default_value = "cart16")]
        positions: String,

        /// With `--objects`: the most object elements to carry — what is left
        /// of twenty-eight channels beside the bed element unless asked,
        /// seventeen beside an LFE element to stay within advanced-1. A master
        /// with more objects than this has them folded into this many
        /// elements, block by block, by the same clustering `encode --cluster`
        /// uses.
        #[arg(long, value_name = "N", conflicts_with = "overlay")]
        elements: Option<usize>,

        /// With `--objects`: keep the master's elements exactly as they are and
        /// pan its last this-many objects onto them — `encode --overlay`, the
        /// same computation, written as IAMF.
        ///
        /// For a programme being re-voiced: everything before the last K
        /// objects is an element and is kept — its bed as the bed element, its
        /// objects as object elements with the master's own paths — and the K
        /// sources (a dubbed dialogue, one object per channel of the track it
        /// came from) are panned onto them by `hz_cluster::overlay`, under the
        /// same guards, which refuse the encode as `encode` does. An element
        /// no source reaches is copied, not mixed. The sequence carries as many
        /// elements as the master brought, within IAMF's own limits. See
        /// `docs/iamf.md`.
        #[arg(
            long,
            value_name = "SOURCES",
            requires = "objects",
            conflicts_with = "voices_to_bed"
        )]
        overlay: Option<usize>,

        /// With `--objects`: render the master's last this-many objects — the
        /// dubbed voices — into a dialogue element of their own, on the room's
        /// cube as the bed mode renders, and carry the rest — the M&E — as
        /// `--objects` alone does, its bed as its bed element.
        ///
        /// The dialogue element's layout is the smallest that holds where the
        /// voices go. The mix presentation labels it `Dialogue`, lets a
        /// listener move it 12 dB either way (an IAMF v2.0 element gain
        /// offset), and states the loudness anchored on dialogue — the element
        /// alone, measured as the mix is. See `docs/iamf.md`.
        #[arg(long, value_name = "VOICES", requires = "objects")]
        voices_to_bed: Option<usize>,

        #[command(flatten)]
        overlaid: OverlayArgs,

        #[command(flatten)]
        mixing: MixArgs,

        /// Report how far along the encode is, on standard error.
        #[arg(long)]
        progress: bool,
    },
}

/// What shapes an overlay beyond which sources it has: its guards, whether a
/// source may take a spare element, and the account of what it did. The same
/// flags, with the same defaults, on `encode` and on `iamf --objects`, because
/// it is the same computation whichever stream it is written into.
#[derive(clap::Args)]
struct OverlayArgs {
    /// How far a fit confined to the bed elements may land from a source,
    /// as a fraction of what the source radiates, before the moving
    /// elements are let in. Zero declines the preference and fits over
    /// every element.
    ///
    /// A bed is still and a dynamic element is wherever the mix put it
    /// this block, so a static source carried by a moving element is
    /// rendered somewhere different every block although it never asked to
    /// move. Preferring the beds is what stops that, and the default is one
    /// step back from where it would start deciding anything: over a 7.1
    /// bed everything the floor ring covers costs under a tenth and
    /// anything with height costs over a half. Only means anything with
    /// `--overlay`. See `hz_cluster::overlay`.
    #[arg(long, value_name = "REACH", default_value_t = hz_cluster::overlay::BEDS_FIRST, allow_hyphen_values = true)]
    overlay_beds: f64,

    /// Refuse the encode if a source ends up more than this many degrees
    /// from where it asked to be, on more than a twentieth of the blocks
    /// it is audible in or for an unbroken run of more than two seconds.
    ///
    /// Thirty degrees is not a localisation bound — it is far past one.
    /// It is the point at which a voice is in a different part of the room
    /// from the picture, which is what a dub cannot ship with; half of it
    /// is remarked on instead, which is about where the rear blur stops
    /// forgiving. Nought declines the guard.
    #[arg(long, value_name = "DEGREES", default_value_t = overlay::OVERLAY_DRIFT)]
    overlay_drift: f64,

    /// Refuse the encode if the strongest element carrying a source holds
    /// less than this fraction of it, on more than a twentieth of the
    /// blocks it is audible in.
    ///
    /// Under it the source is diffuse: no one element is rendering it, so
    /// it arrives from everywhere the fit reached and moves whenever any
    /// of those elements does. A half is one element holding three
    /// quarters of the power. Nought declines the guard.
    #[arg(long, value_name = "WEIGHT", default_value_t = overlay::OVERLAY_SPREAD)]
    overlay_spread: f64,

    /// Refuse the encode if more than this percentage of the windows a
    /// source was heard in carry movement nobody asked for.
    ///
    /// The sources of a dub are static, so every out-and-back in where
    /// the carriers put them is the breathing an overlay can produce: a
    /// still voice carried by moving elements. This is the only guard
    /// with a time axis, and so the only one that can see that defect at
    /// all. Nought declines it. See `hz_cluster::motion`.
    #[arg(long, value_name = "PERCENT", default_value_t = overlay::OVERLAY_WOBBLE)]
    overlay_wobble: f64,

    /// Refuse the encode if a source's rendered level is more than this
    /// many decibels from what it asked for, on any presentation, for
    /// more than a twentieth of the blocks it is audible in.
    ///
    /// The case this is for: a voice landed on an element with a small
    /// stereo fold coefficient, which survives 7.1.4 and vanishes in the
    /// downmix. Computed from the geometry rather than from the audio —
    /// what a source comes out at on a presentation is a linear function
    /// of its weights and the elements' positions — so it is exact rather
    /// than estimated. Nought declines the guard.
    #[arg(long, value_name = "DB", default_value_t = overlay::OVERLAY_LEVEL)]
    overlay_level: f64,

    /// Refuse the encode if the worst source's fold costs more than this,
    /// as a fraction of its own gain vector, on more than a twentieth of
    /// the blocks it is audible in.
    ///
    /// `hz_cluster::metric`'s own measure, restricted to the sources; the
    /// elements are not in it, since an overlay does not fold them.
    /// Nought declines the guard.
    #[arg(long, value_name = "ERROR", default_value_t = overlay::OVERLAY_COST)]
    overlay_cost: f64,

    /// Route a source the fit could not place acceptably to the nearest
    /// bed element outright, rather than leaving it where the fit put it.
    /// On by default; `--overlay-fallback false` leaves the fit alone.
    ///
    /// A source on one bed is slightly misplaced and perfectly sharp,
    /// which is the trade: audible and a little off beats diffuse and
    /// wandering. Borrowing an inactive element for the stretch, or
    /// falling back to a full `--cluster`, are decisions about a
    /// programme and are the caller's, not this.
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set, num_args = 0..=1, default_missing_value = "true")]
    overlay_fallback: bool,

    /// Let a source take a spare element of its own when the master
    /// brings fewer than sixteen, rather than being panned onto the
    /// master's elements. Off by default: the stream carries exactly the
    /// elements the master brought, and every source is panned.
    ///
    /// A source with an element of its own is carried bit for bit and
    /// costs nothing, which is the best a source can get — and it makes
    /// the stream wider than the original: a twelve-element master
    /// re-voiced with four sources ships as sixteen. Whether a dub may
    /// change the original's width is a decision about the programme, so
    /// it is asked for rather than assumed. The spares go to the sources
    /// nearest the front centre. Only means anything with `--overlay`.
    /// See `docs/encode.md`.
    #[arg(long, action = clap::ArgAction::SetTrue)]
    overlay_spare: bool,

    /// Write, per block, exactly what the overlay did with it: which
    /// elements were copied and from which channel, which were mixed, and
    /// the weights and gains it mixed them with.
    ///
    /// For `cargo xtask overlay-check`, which has to reproduce the
    /// arithmetic to check it and cannot re-derive the fit without
    /// re-deriving the thing being checked. Not a format anything else
    /// should read; it says what this encoder did and nothing about what
    /// the stream is.
    #[arg(long, value_name = "PATH")]
    overlay_report: Option<PathBuf>,
}

/// What a mix does to the elements it makes: how a block's power is weighed,
/// what counts as audible, how the elements are kept inside the codec's
/// domain, and what they are rounded to. Shared by `encode` — a fold and an
/// overlay — and `iamf --objects --overlay`.
#[derive(clap::Args)]
struct MixArgs {
    /// Round the mixed elements to this many bits.
    ///
    /// Only means anything where something is mixed — `encode --cluster`,
    /// and the few elements `--overlay` adds a source to. A mix is a sum over
    /// real weights, so its low bits are the noise of the multiplication
    /// rather than signal, and a lossless coder carries them anyway. Under an
    /// overlay the elements nothing was added to are copied and keep every
    /// bit they arrived with, so this decides the depth of part of the stream
    /// and not all of it. Twenty is worth about a third of a TrueHD stream
    /// against twenty-four and puts the rounding noise near −120 dBFS; FLAC
    /// takes the same zeroes off the bottom of a subframe as wasted bits.
    ///
    /// Twenty-four keeps every bit the mix made. A TrueHD stream takes
    /// seventeen to twenty-four, because below seventeen the format's own
    /// dead-bit field can no longer say what was rounded away; an IA sequence
    /// sixteen to twenty-four, and never more than its `--bits`. Match it to
    /// the *source* master rather than guessing: a programme mastered at
    /// eighteen bits gains nothing from twenty. See `docs/encode.md`.
    #[arg(long, value_name = "BITS", default_value_t = mix::FOLD_BITS)]
    fold_depth: u32,

    /// The programme's dialnorm in decibels, −31 to −1, which sets the floor
    /// under what an object has to carry to be heard at the playback level:
    /// full scale at 105 dB SPL less the gain a decoder applies to bring the
    /// dialogue to −31, and the threshold of hearing under that. Below it an
    /// object seeds nothing and sets no worst case in a fold; under
    /// `--overlay` it is what decides a source is audible, and so which
    /// elements are mixed rather than copied and which blocks its guards are
    /// counted over. See `hz_cluster::floor`.
    #[arg(long, value_name = "DB", default_value_t = -31.0, allow_hyphen_values = true)]
    dialnorm: f64,

    /// How a mix keeps its elements inside the codec's domain: `limit`, a
    /// limiter with one gain for every element, ahead of the peak; `bound`,
    /// the fit bounding an element's coherent peak, which costs error and
    /// does not take every clip; `both`; or `off`, which clamps and counts
    /// the clips. `--overlay` uses the limiter half of it on the elements it
    /// mixes into; the bound is the clusterer's own and has nothing to bound
    /// where no element is placed. See `hz_cluster::headroom`.
    #[arg(long, value_name = "RULE", default_value = "limit")]
    headroom: String,

    /// How a block's power is weighed before it steers the mix: `flat`, the
    /// plain power a meter reads; `kweighted`, through the K filter of
    /// BS.1770; or `perceptual`, what each object adds to the loudness of the
    /// scene, band by band, with what is near it masking it.
    ///
    /// Flat is what every measurement in `docs/clustering.md` was made under,
    /// and the perceptual rule did not beat it on the metric; it is here to
    /// be listened to. It costs about a third of a folding encode's time. In
    /// a fold it steers where the elements go; under `--overlay` it is the
    /// energy that decides a source is audible. See
    /// `hz_cluster::scene::Loudness`.
    #[arg(long, value_name = "RULE", default_value = "flat")]
    loudness: String,
}

impl MixArgs {
    fn weighing(&self, input: &std::path::Path) -> hz_core::Result<hz_cluster::scene::Loudness> {
        match self.loudness.as_str() {
            "flat" => Ok(hz_cluster::scene::Loudness::Flat),
            "kweighted" | "k" => Ok(hz_cluster::scene::Loudness::KWeighted),
            "perceptual" => Ok(hz_cluster::scene::Loudness::Perceptual(
                hz_cluster::scene::PERCEPTION,
            )),
            other => Err(hz_core::Error::unsupported(
                input,
                format!("`{other}` for the loudness; flat, kweighted or perceptual"),
            )),
        }
    }

    fn headroom(&self, input: &std::path::Path) -> hz_core::Result<Headroom> {
        match self.headroom.as_str() {
            "both" => Ok(Headroom::Both),
            "bound" => Ok(Headroom::Bound),
            "limit" => Ok(Headroom::Limit),
            "off" => Ok(Headroom::Off),
            other => Err(hz_core::Error::unsupported(
                input,
                format!("`{other}` for the headroom; both, bound, limit or off"),
            )),
        }
    }

    /// The mix as both commands take it.
    fn mixing(&self, input: &std::path::Path) -> hz_core::Result<Mixing> {
        let weighing = self.weighing(input);
        let headroom = self.headroom(input);
        Ok(Mixing {
            fold_depth: self.fold_depth,
            dialnorm: self.dialnorm,
            headroom: headroom?,
            weighing: weighing?,
        })
    }
}

impl OverlayArgs {
    /// Nought is not a reach, it is the preference declined: no bed-only fit
    /// lands within nothing of the source. A negative one is neither, and the
    /// writer refuses it rather than read a typed minus sign as "declined".
    fn beds_first(&self) -> Option<f64> {
        (self.overlay_beds > 0.0).then_some(self.overlay_beds)
    }

    /// The overlay as both commands take it.
    fn options(self) -> overlay::Options {
        overlay::Options {
            beds_first: self.beds_first(),
            beds_asked: self.overlay_beds,
            bounds: overlay::Bounds {
                drift: self.overlay_drift,
                spread: self.overlay_spread,
                wobble: self.overlay_wobble,
                level: self.overlay_level,
                cost: self.overlay_cost,
                fallback: self.overlay_fallback,
            },
            spare: self.overlay_spare,
            report: self.overlay_report,
        }
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.command {
        Command::Convert {
            input,
            output,
            presentation,
        } => convert::run(&input, &output, presentation),
        Command::Encode {
            input,
            out,
            frames,
            mono_prefix,
            fast,
            cluster,
            overlay,
            overlaid,
            mixing,
            fold_search,
            beds,
            smooth_behind,
            fold_hold,
            drc,
            presentations,
            progress,
        } => {
            let mixing = mixing.mixing(&input);
            let beds = match beds.as_str() {
                "auto" => Ok(encode::Beds::Auto),
                "pinned" => Ok(encode::Beds::Pinned),
                "free" => Ok(encode::Beds::Free),
                other => Err(hz_core::Error::unsupported(
                    &input,
                    format!("`{other}` for the beds; auto, pinned or free"),
                )),
            };
            let drc = match drc.as_str() {
                "off" => Ok(None),
                "measured" => Ok(Some(encode::Drc::Measured)),
                db => db
                    .parse::<f64>()
                    .map(|db| Some(encode::Drc::Constant(db)))
                    .map_err(|_| {
                        hz_core::Error::unsupported(
                            &input,
                            format!(
                                "`{db}` as a dynamic range; `measured`, a number of decibels, \
                                 or `off`"
                            ),
                        )
                    }),
            };
            let settings = hz_io::settings::Settings::load(cli.config.as_deref());
            drc.and_then(|drc| {
                encode::run(encode::Config {
                    input,
                    out,
                    frames,
                    mono_prefix,
                    fast,
                    cluster,
                    overlay,
                    overlaying: overlaid.options(),
                    mixing: mixing?,
                    fold_search,
                    beds: beds?,
                    smooth_behind,
                    fold_hold,
                    drc,
                    presentations,
                    progress,
                    settings: settings?,
                })
            })
        }
        Command::Iamf {
            input,
            out,
            frames,
            mono_prefix,
            codec,
            bits,
            bitrate,
            frame_size,
            headphones,
            objects,
            positions,
            elements,
            overlay,
            voices_to_bed,
            overlaid,
            mixing,
            progress,
        } => {
            let codec = match (codec.as_str(), bitrate) {
                ("opus", rate) => Ok(hz_iamf::Codec::Opus {
                    bitrate: rate.unwrap_or(iamf::OPUS_BITRATE) * 1000,
                }),
                (_, Some(_)) => Err(hz_core::Error::unsupported(
                    &input,
                    "--bitrate with a lossless codec; it is for opus",
                )),
                ("flac", None) => Ok(hz_iamf::Codec::Flac),
                ("lpcm" | "pcm", None) => Ok(hz_iamf::Codec::Lpcm),
                (other, None) => Err(hz_core::Error::unsupported(
                    &input,
                    format!("`{other}` for the codec; flac, lpcm or opus"),
                )),
            };
            let lossy = matches!(codec, Ok(hz_iamf::Codec::Opus { .. }));
            let bits = match (lossy, bits) {
                (true, Some(_)) => Err(hz_core::Error::unsupported(
                    &input,
                    "--bits with opus, which has no depth",
                )),
                (_, bits) => Ok(bits.unwrap_or(24)),
            };
            let frame_size =
                frame_size.unwrap_or(if lossy { iamf::OPUS_FRAME } else { iamf::FRAME });
            let objects = if objects {
                match positions.as_str() {
                    "cart16" => Ok(Some(hz_iamf::PositionKind::Cart16)),
                    "cart8" => Ok(Some(hz_iamf::PositionKind::Cart8)),
                    "polar" => Ok(Some(hz_iamf::PositionKind::Polar)),
                    other => Err(hz_core::Error::unsupported(
                        &input,
                        format!("`{other}` for the positions; cart16, cart8 or polar"),
                    )),
                }
            } else {
                Ok(None)
            };
            let headphones = match headphones.as_str() {
                "stereo" => Ok(hz_iamf::Headphones::Stereo),
                "binaural" => Ok(hz_iamf::Headphones::Binaural),
                other => Err(hz_core::Error::unsupported(
                    &input,
                    format!("`{other}` for the headphones; stereo or binaural"),
                )),
            };
            let mixing = mixing.mixing(&input);
            codec.and_then(|codec| {
                iamf::run(iamf::Config {
                    input,
                    out,
                    frames,
                    mono_prefix,
                    codec,
                    bits: bits?,
                    frame: frame_size,
                    headphones: headphones?,
                    progress,
                    objects: objects?,
                    elements,
                    overlay,
                    voices_to_bed,
                    overlaying: overlaid.options(),
                    mixing: mixing?,
                })
            })
        }
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("harlettizer: {e}");
            ExitCode::FAILURE
        }
    }
}
