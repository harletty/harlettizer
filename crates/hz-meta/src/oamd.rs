// SPDX-License-Identifier: GPL-3.0-or-later
//
// Carries material ported from the `truehd` crate
// (https://github.com/truehdd/truehdd) — Copyright (c) the truehdd authors —
// under the Apache License, Version 2.0, whose text is reproduced in
// LICENSES/Apache-2.0.txt.
// What was taken and what was changed is recorded in docs/provenance.md.

//! The object audio metadata payload.
//!
//! One payload describes every element of an immersive programme over one
//! access unit: what they are, and where each of them is at each of up to
//! eight moments within it. A delivery bitstream carries it verbatim — TrueHD
//! in the extra data of an access unit under Evolution identifier 11, E-AC-3
//! in an Evolution frame of a syncframe — so nothing here knows about either.
//!
//! # What is modelled
//!
//! The shape a real immersive programme uses: **every element a dynamic
//! object**, one of which may be the low frequency channel. That is what the
//! reference stream carries and it is what a sixteen-element presentation is.
//! The syntax also admits speaker-anchored beds and an intermediate spatial
//! format, and those are refused by name rather than approximated — a payload
//! read wrong is a payload whose objects land somewhere else.
//!
//! # Positions are codes here
//!
//! The wire carries `x` and `y` in sixty-seconds of the room and `z` in
//! fifteenths of it, and a later block may move an object by a small
//! *difference* in those same units. Keeping the codes rather than the metres
//! they stand for is what makes reading and writing exact inverses: the
//! rounding happens once, where a caller converts.

use hz_core::bits::BitWriter;
use hz_core::bits_read::{BitReader, Ran};
use std::fmt;

/// A payload this does not model, or one that does not parse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unreadable(pub String);

impl fmt::Display for Unreadable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "object audio metadata: {}", self.0)
    }
}

impl std::error::Error for Unreadable {}

impl From<Ran> for Unreadable {
    fn from(ran: Ran) -> Self {
        Self(ran.0)
    }
}

type Result<T> = std::result::Result<T, Unreadable>;

/// The most elements a programme may have.
pub const MAX_OBJECTS: usize = 159;

/// The most moments one payload may describe.
pub const MAX_BLOCKS: usize = 8;

/// The gain that means silence rather than a level.
const SILENT: i8 = -128;

/// The most an object's position code may be, in each axis. `x` and `y` run
/// from one wall to the other, `z` from the floor to the ceiling with a sign
/// of its own.
pub const MAX_HORIZONTAL: i32 = 62;
pub const MAX_VERTICAL: i32 = 15;

/// One access unit's worth of object audio metadata.
#[derive(Debug, Clone, PartialEq)]
pub struct ObjectAudioMetadata {
    /// Whether one of the elements is the low frequency channel. When it is,
    /// it is the first, it has no position of its own, and everything after it
    /// is a dynamic object.
    pub lfe: bool,
    /// The moments this payload describes, in order. At least one.
    pub blocks: Vec<Block>,
}

/// One moment within the access unit, and every element's state at it.
#[derive(Debug, Clone, PartialEq)]
pub struct Block {
    /// Where in the access unit it takes effect, in units of 32 samples.
    /// Six bits, so up to the 2016th sample.
    pub offset: u32,
    /// How long an element takes to reach this state from the last one.
    pub ramp: Ramp,
    /// One per element, in order, the low frequency channel first when there
    /// is one.
    pub objects: Vec<Object>,
}

/// How long an element takes to reach a block's state.
///
/// Five spellings of one number. [`Ramp::samples`] says what each comes to and
/// [`Ramp::from_samples`] picks the shortest that can say it, which is what a
/// writer wants: the named lengths cost seven bits where an outright count
/// costs fourteen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ramp {
    /// Immediately.
    None,
    /// The two lengths the syntax names in two bits.
    Short,
    Long,
    /// One of sixteen lengths it names in four more.
    Named(u8),
    /// Or any length up to 2047 samples, stated outright.
    Samples(u16),
}

/// The sixteen lengths the four-bit index names, in samples.
const NAMED_RAMPS: [u16; 16] = [
    32, 64, 128, 256, 320, 480, 1000, 1001, 1024, 1600, 1601, 1602, 1920, 2000, 2002, 2048,
];

/// What [`Ramp::Short`] and [`Ramp::Long`] come to.
const SHORT_RAMP: u16 = 512;
const LONG_RAMP: u16 = 1536;

/// The longest an outright count can state: the field is eleven bits.
const MAX_RAMP_SAMPLES: u16 = 2047;

impl Ramp {
    /// How many samples this ramp is.
    pub fn samples(self) -> u16 {
        match self {
            Self::None => 0,
            Self::Short => SHORT_RAMP,
            Self::Long => LONG_RAMP,
            Self::Named(index) => NAMED_RAMPS
                .get(usize::from(index))
                .copied()
                .unwrap_or_default(),
            Self::Samples(samples) => samples,
        }
    }

    /// The shortest spelling of a length in samples.
    ///
    /// The two-bit codes first, then the four-bit index, then an outright
    /// count — seven bits against fourteen, so the index is worth reaching
    /// for. 2048 is only reachable through the index; the count is eleven bits
    /// and stops one short of it.
    ///
    /// `None` for a length the syntax cannot state.
    pub fn from_samples(samples: u16) -> Option<Self> {
        match samples {
            0 => Some(Self::None),
            SHORT_RAMP => Some(Self::Short),
            LONG_RAMP => Some(Self::Long),
            other => match NAMED_RAMPS.iter().position(|named| *named == other) {
                Some(index) => Some(Self::Named(index as u8)),
                None if other <= MAX_RAMP_SAMPLES => Some(Self::Samples(other)),
                None => None,
            },
        }
    }
}

/// One element's state at one moment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Object {
    /// An inactive element is one a renderer leaves out entirely, and it says
    /// nothing else about itself.
    pub active: bool,
    pub gain: Gain,
    /// Absent for the low frequency channel, which is a bed and goes where the
    /// bed goes.
    pub render: Option<Render>,
}

/// What an element's gain is, in whole decibels.
///
/// The syntax has three ways to say it — nothing, silence, or an index — and
/// the index reaches +15 dB down to −49 dB but cannot express zero, which is
/// what the first of the three is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gain {
    /// Unchanged, which the wire writes as its own code rather than as an
    /// index of zero.
    Unity,
    Silent,
    Decibels(i8),
}

impl Gain {
    /// What it comes to in decibels, with `None` for silence.
    pub fn decibels(self) -> Option<i8> {
        match self {
            Self::Unity => Some(0),
            Self::Silent => None,
            Self::Decibels(db) => Some(db),
        }
    }

    /// A gain in whole decibels, as the syntax can say it.
    ///
    /// Zero comes back as [`Gain::Unity`] rather than as `Decibels(0)`: the
    /// index cannot express zero, and the code that can is the other one. A
    /// caller that builds `Decibels(0)` by hand is caught by [`Self::coded`]
    /// rather than written as the gain one step away from it.
    pub fn from_decibels(db: i8) -> Result<Self> {
        match db {
            SILENT => Ok(Self::Silent),
            0 => Ok(Self::Unity),
            db if (-49..=15).contains(&db) => Ok(Self::Decibels(db)),
            db => Err(Unreadable(format!(
                "a gain of {db} dB; the field reaches +15 down to -49"
            ))),
        }
    }

    /// A gain as a linear amplitude, rounded to the whole decibel the syntax
    /// carries.
    ///
    /// Zero and anything below the field's floor are [`Gain::Silent`]: the
    /// alternative is to state −49 dB for something the mix asked to be
    /// silent, which is audible.
    pub fn from_linear(amplitude: f64) -> Result<Self> {
        if !amplitude.is_finite() || amplitude < 0.0 {
            return Err(Unreadable(format!("a gain of {amplitude}")));
        }
        if amplitude == 0.0 {
            return Ok(Self::Silent);
        }
        let db = (20.0 * amplitude.log10()).round();
        if db < -49.0 {
            return Ok(Self::Silent);
        }
        Self::from_decibels(db.clamp(-49.0, 15.0) as i8)
    }

    /// The six-bit code the wire carries, for a gain that has one.
    ///
    /// Validated here rather than at the writer, because the writer's field is
    /// six bits wide and a code that does not fit is silently truncated into a
    /// different gain — the one defect in this crate that would be inaudible
    /// in the file and audible in the room.
    fn coded(self) -> Result<Option<u32>> {
        match self {
            Self::Unity | Self::Silent => Ok(None),
            Self::Decibels(0) => Err(Unreadable(
                "a gain of 0 dB is written as `Unity`, which the index cannot express".into(),
            )),
            Self::Decibels(db) if (-49..=15).contains(&db) => {
                Ok(Some(if db > 0 { 15 - db } else { 14 - db } as u32))
            }
            Self::Decibels(db) => Err(Unreadable(format!(
                "a gain of {db} dB; the field reaches +15 down to -49"
            ))),
        }
    }
}

/// Where an element is and how it is to be rendered there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Render {
    pub position: Position,
    pub distance: Distance,
    /// Which of the room's zones the element is confined to, and whether its
    /// height is to be honoured.
    pub zones: u8,
    pub elevation: bool,
    pub size: Size,
    /// How far the element follows the screen rather than the room, and how
    /// far into it. Absent when it follows the room.
    pub screen: Option<(u8, u8)>,
    /// Whether a renderer should place it at the nearest speaker outright.
    pub snap: bool,
}

impl Default for Render {
    fn default() -> Self {
        Self {
            position: Position::CENTRE,
            distance: Distance::Unstated,
            zones: 0,
            elevation: false,
            size: Size::Point,
            screen: None,
            snap: false,
        }
    }
}

/// An element's position, in the codes the wire carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Position {
    /// Left wall to right wall, 0 to 62.
    pub x: i32,
    /// Front wall to back wall, 0 to 62.
    pub y: i32,
    /// Floor to ceiling, −15 to 15.
    pub z: i32,
}

impl Position {
    pub const CENTRE: Self = Self { x: 31, y: 31, z: 0 };

    /// The position in the master format's own frame: each axis from −1 to 1,
    /// with `y` positive towards the front rather than the back.
    pub fn to_master(self) -> [f64; 3] {
        [
            (f64::from(self.x) / MAX_HORIZONTAL as f64 - 0.5) * 2.0,
            (0.5 - f64::from(self.y) / MAX_HORIZONTAL as f64) * 2.0,
            f64::from(self.z) / MAX_VERTICAL as f64,
        ]
    }

    /// And back, to the nearest code the wire can carry.
    pub fn from_master(position: [f64; 3]) -> Self {
        let horizontal = |value: f64| {
            ((value.clamp(-1.0, 1.0) / 2.0 + 0.5) * MAX_HORIZONTAL as f64).round() as i32
        };
        Self {
            x: horizontal(position[0]),
            y: horizontal(-position[1]),
            z: (position[2].clamp(-1.0, 1.0) * MAX_VERTICAL as f64).round() as i32,
        }
    }

    fn clamped(self) -> Self {
        Self {
            x: self.x.clamp(0, MAX_HORIZONTAL),
            y: self.y.clamp(0, MAX_HORIZONTAL),
            z: self.z.clamp(-MAX_VERTICAL, MAX_VERTICAL),
        }
    }
}

/// How far away an element is, when it says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Distance {
    Unstated,
    Infinite,
    /// One of sixteen distances, from 1.1 metres to 50.
    Index(u8),
}

/// How large an element is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Size {
    Point,
    /// The same in every axis, in thirty-firsts of the room.
    Uniform(u8),
    /// Width, depth and height separately.
    Box(u8, u8, u8),
}

/// The element identifier for the object element, which is the only one this
/// reads or writes. The others describe trims, extended precision, headphone
/// rendering and object descriptions.
const ELEMENT_OBJECT: u32 = 1;

impl ObjectAudioMetadata {
    /// Read a payload.
    ///
    /// Refuses, by name, a programme shape this does not model rather than
    /// reading on and placing objects somewhere else.
    pub fn read(bytes: &[u8]) -> Result<Self> {
        let mut reader = BitReader::new(bytes);

        let version = reader.get(2)?;
        if version != 0 {
            // Past two the version escapes into a field of its own, and the
            // layout after it is not this one.
            return Err(Unreadable(format!("version {version}")));
        }

        let mut count = reader.get(5)?;
        if count == 31 {
            count += reader.get(7)?;
        }
        let count = count as usize + 1;
        if count > MAX_OBJECTS {
            return Err(Unreadable(format!("{count} elements")));
        }

        // The programme: every element a dynamic object, one of which may be
        // the low frequency channel.
        if !reader.get_bit()? {
            return Err(Unreadable(
                "a programme with speaker-anchored beds or an intermediate \
                 spatial format, which is not modelled"
                    .into(),
            ));
        }
        let lfe = reader.get_bit()?;

        if reader.get_bit()? {
            return Err(Unreadable("alternate object data".into()));
        }

        let mut elements = reader.get(4)?;
        if elements == 15 {
            elements += reader.get(5)?;
        }

        let mut blocks = None;
        for _ in 0..elements {
            let id = reader.get(4)?;
            // The declared size covers everything after itself, so an element
            // this does not read is stepped over rather than guessed at.
            let size = (reader.get_variable(4, 4)? as usize + 1) * 8;
            let start = reader.bit_position();

            let discard = reader.get_bit()?;
            let _ = discard;
            if id == ELEMENT_OBJECT {
                blocks = Some(read_object_element(&mut reader, count, lfe)?);
            }

            let read = reader.bit_position() - start;
            if read > size {
                return Err(Unreadable(format!(
                    "element {id} declared {size} bits and used {read}"
                )));
            }
            reader.skip(size - read)?;
        }

        let blocks = blocks.ok_or_else(|| Unreadable("no object element".into()))?;
        Ok(Self { lfe, blocks })
    }

    /// How many elements each of its blocks describes.
    pub fn objects(&self) -> usize {
        self.blocks.first().map_or(0, |block| block.objects.len())
    }
}

/// The object element: when its blocks fall, and every element's state in each.
fn read_object_element(reader: &mut BitReader<'_>, count: usize, lfe: bool) -> Result<Vec<Block>> {
    // A sample offset within the access unit, which this does not model
    // because nothing needs it: a block already says where it falls.
    match reader.get(2)? {
        0 => {}
        1 => {
            reader.skip(2)?;
        }
        2 => {
            reader.skip(5)?;
        }
        other => return Err(Unreadable(format!("sample offset code {other}"))),
    }

    let block_count = reader.get(3)? as usize + 1;
    let mut blocks: Vec<Block> = Vec::with_capacity(block_count);
    for _ in 0..block_count {
        let offset = reader.get(6)?;
        let ramp = match reader.get(2)? {
            0 => Ramp::None,
            1 => Ramp::Short,
            2 => Ramp::Long,
            _ => {
                if reader.get_bit()? {
                    Ramp::Named(reader.get(4)? as u8)
                } else {
                    Ramp::Samples(reader.get(11)? as u16)
                }
            }
        };
        blocks.push(Block {
            offset,
            ramp,
            objects: Vec::with_capacity(count),
        });
    }

    if !reader.get_bit()? {
        reader.skip(5)?; // reserved
    }

    // The bed, when there is one, is the first element and has no position.
    let beds = usize::from(lfe);
    // Two kinds of memory, and they are carried differently. What a block
    // leaves behind is what the *next block of the same element* starts from,
    // so it resets at every element. The gain predictor is the other way
    // round: it holds the *previous element's* gain at the same block, so it
    // runs across elements and is indexed by block.
    let mut carried_gain = [Gain::Unity; MAX_BLOCKS];
    for object in 0..count {
        let mut held = Object {
            active: false,
            gain: Gain::Silent,
            render: None,
        };
        let mut held_render = Render::default();
        for (index, block) in blocks.iter_mut().enumerate() {
            let state = read_object(
                reader,
                index,
                object >= beds,
                &held,
                &held_render,
                &mut carried_gain,
                object == 0,
            )?;
            // An element that is not active this block leaves nothing behind:
            // a decoder resets what it holds, so anything after it that says
            // "unchanged" means unchanged from *that*.
            held_render = state.render.unwrap_or_default();
            held = state;
            block.objects.push(state);
        }
    }

    Ok(blocks)
}

/// One element's state in one block.
fn read_object(
    reader: &mut BitReader<'_>,
    index: usize,
    dynamic: bool,
    held: &Object,
    held_render: &Render,
    carried_gain: &mut [Gain; MAX_BLOCKS],
    first_object: bool,
) -> Result<Object> {
    let inactive = reader.get_bit()?;

    // The first block always states everything; a later one says whether it
    // does, keeps what stands, or states a subset.
    let status = if inactive {
        0
    } else if index == 0 {
        1
    } else {
        reader.get(2)?
    };
    let gain = match status {
        0 => Gain::Silent,
        1 | 3 => {
            let fields = if status == 1 { 3 } else { reader.get(2)? };
            let gain = if fields & 1 != 0 {
                let carried = if first_object {
                    Gain::Unity
                } else {
                    carried_gain[index]
                };
                let gain = match reader.get(2)? {
                    0 => Gain::Unity,
                    1 => Gain::Silent,
                    2 => {
                        let code = reader.get(6)? as i8;
                        Gain::from_decibels(if code <= 14 { 15 - code } else { 14 - code })?
                    }
                    _ => carried,
                };
                carried_gain[index] = gain;
                gain
            } else {
                held.gain
            };
            if fields & 2 != 0 && !reader.get_bit()? {
                reader.skip(5)?; // a priority other than the default
            }
            gain
        }
        _ => held.gain,
    };

    // A bed element says nothing about where it is; the bed decides that.
    let render_status = if inactive || !dynamic {
        0
    } else if index == 0 {
        1
    } else {
        reader.get(2)?
    };
    let render = match render_status {
        0 => None,
        1 | 3 => Some(read_render(reader, render_status, index, held_render)?),
        _ => Some(*held_render),
    };

    if reader.get_bit()? {
        let size = (reader.get(4)? as usize + 1) * 8;
        reader.skip(size)?; // a table this does not read
    }

    Ok(Object {
        active: !inactive,
        gain,
        render,
    })
}

/// Where an element is, and how it is to be rendered there.
fn read_render(
    reader: &mut BitReader<'_>,
    status: u32,
    index: usize,
    held: &Render,
) -> Result<Render> {
    let mut render = *held;
    let fields = if status == 1 { 15 } else { reader.get(4)? };

    if fields & 1 != 0 {
        // The first block has nothing to differ from.
        let differential = index != 0 && reader.get_bit()?;
        render.position = if differential {
            Position {
                x: held.position.x + reader.get_signed(3)?,
                y: held.position.y + reader.get_signed(3)?,
                z: held.position.z + reader.get_signed(3)?,
            }
        } else {
            let x = reader.get(6)? as i32;
            let y = reader.get(6)? as i32;
            let sign = if reader.get_bit()? { 1 } else { -1 };
            Position {
                x,
                y,
                z: sign * reader.get(4)? as i32,
            }
        }
        // Each axis is clamped where it stands, and it is the clamped value a
        // later block's difference applies to.
        .clamped();

        render.distance = if reader.get_bit()? {
            if reader.get_bit()? {
                Distance::Infinite
            } else {
                Distance::Index(reader.get(4)? as u8)
            }
        } else {
            Distance::Unstated
        };
    }

    if fields & 2 != 0 {
        // Seven is not a zone constraint; it stands for none.
        render.zones = match reader.get(3)? {
            7 => 0,
            zones => zones as u8,
        };
        render.elevation = reader.get_bit()?;
    }

    if fields & 4 != 0 {
        render.size = match reader.get(2)? {
            1 => Size::Uniform(reader.get(5)? as u8),
            2 => Size::Box(
                reader.get(5)? as u8,
                reader.get(5)? as u8,
                reader.get(5)? as u8,
            ),
            _ => Size::Point,
        };
    }

    if fields & 8 != 0 {
        render.screen = reader
            .get_bit()?
            .then(|| -> Result<(u8, u8)> { Ok((reader.get(3)? as u8, reader.get(2)? as u8)) })
            .transpose()?;
    }

    render.snap = reader.get_bit()?;
    Ok(render)
}

impl ObjectAudioMetadata {
    /// Write a payload.
    ///
    /// The exact inverse of [`Self::read`] for anything this models: a payload
    /// written and read back is the same payload, and the reference stream's
    /// own is reproduced from what was read out of it.
    ///
    /// Blocks after the first say only what changed. That is not a size
    /// optimisation so much as what the syntax is for — an element that has
    /// not moved says two bits and stops.
    pub fn write(&self) -> Result<Vec<u8>> {
        let count = self.objects();
        if count == 0 || count > MAX_OBJECTS {
            return Err(Unreadable(format!("{count} elements")));
        }
        if self.blocks.is_empty() || self.blocks.len() > MAX_BLOCKS {
            return Err(Unreadable(format!("{} blocks", self.blocks.len())));
        }
        if self.blocks.iter().any(|block| block.objects.len() != count) {
            return Err(Unreadable("the blocks describe different elements".into()));
        }

        let body = self.write_object_element(count)?;
        // The declared size covers the bit that precedes the body as well.
        let content = 1 + body.bit_position();
        let size = content.div_ceil(8);

        let mut writer = BitWriter::with_capacity(size + 8);
        writer.put(2, 0); // version
        let coded = count - 1;
        if coded < 31 {
            writer.put(5, coded as u32);
        } else {
            writer.put(5, 31);
            writer.put(7, (coded - 31) as u32);
        }

        writer.put(1, 1); // every element a dynamic object
        writer.put(1, u32::from(self.lfe));
        writer.put(1, 0); // no alternate object data
        writer.put(4, 1); // one element follows

        writer.put(4, ELEMENT_OBJECT);
        writer.put_variable(4, 4, size as u32 - 1);
        writer.put(1, 0); // a decoder that does not know this keeps it anyway
        writer.put_bits(&body.padded(), body.bit_position());
        // The element is a whole number of bytes and its body is not, so it
        // ends in whatever it takes to reach the size declared above.
        writer.put_zeroes(size * 8 - content);

        writer.align_to(8);
        Ok(writer.finish())
    }

    /// The object element's body: when its blocks fall, and every element's
    /// state in each.
    fn write_object_element(&self, count: usize) -> Result<BitWriter> {
        let mut w = BitWriter::with_capacity(256);
        w.put(2, 0); // the metadata takes effect at the start of the unit
        w.put(3, self.blocks.len() as u32 - 1);
        for block in &self.blocks {
            if block.offset > 63 {
                return Err(Unreadable(format!("a block offset of {}", block.offset)));
            }
            w.put(6, block.offset);
            match block.ramp {
                Ramp::None => w.put(2, 0),
                Ramp::Short => w.put(2, 1),
                Ramp::Long => w.put(2, 2),
                Ramp::Named(index) => {
                    w.put(2, 3);
                    w.put(1, 1);
                    w.put(4, u32::from(index));
                }
                Ramp::Samples(samples) => {
                    w.put(2, 3);
                    w.put(1, 0);
                    w.put(11, u32::from(samples));
                }
            }
        }
        w.put(1, 1); // no reserved data

        let beds = usize::from(self.lfe);
        // Both kinds of memory a decoder keeps, mirrored so that "unchanged"
        // is written only when it really is: what the previous block of this
        // element left, and the previous element's gain at this block.
        let mut carried_gain = [Gain::Unity; MAX_BLOCKS];
        for object in 0..count {
            let mut held = Object {
                active: false,
                gain: Gain::Silent,
                render: None,
            };
            let mut held_render = Render::default();
            for (index, block) in self.blocks.iter().enumerate() {
                let state = &block.objects[object];
                write_object(
                    &mut w,
                    index,
                    object >= beds,
                    state,
                    &held,
                    &held_render,
                    &mut carried_gain,
                    object == 0,
                )?;
                held_render = state.render.unwrap_or_default();
                held = if state.active {
                    *state
                } else {
                    // An inactive element states nothing, so a decoder resets
                    // what it holds — and so does this, or the next block
                    // would say "unchanged" from something the decoder has
                    // already forgotten.
                    Object {
                        active: false,
                        gain: Gain::Silent,
                        render: None,
                    }
                };
            }
        }
        Ok(w)
    }
}

/// One element's state in one block.
#[allow(clippy::too_many_arguments)]
fn write_object(
    w: &mut BitWriter,
    index: usize,
    dynamic: bool,
    state: &Object,
    held: &Object,
    held_render: &Render,
    carried_gain: &mut [Gain; MAX_BLOCKS],
    first_object: bool,
) -> Result<()> {
    w.put(1, u32::from(!state.active));
    if !state.active {
        if state.render.is_some() || state.gain != Gain::Silent {
            return Err(Unreadable(
                "an inactive element with a gain or a position: the syntax has \
                 nowhere to put them and a decoder would read neither"
                    .into(),
            ));
        }
        // An inactive element says nothing else, but the table bit is read
        // for every block whatever the element is doing.
        w.put(1, 0);
        return Ok(());
    }

    // The first block states everything and says so by being first; a later
    // one that changed nothing says exactly that.
    let unchanged = index > 0 && held.gain == state.gain;
    if index > 0 {
        w.put(2, if unchanged { 2 } else { 3 });
    }
    if !unchanged {
        if index > 0 {
            w.put(2, 3); // both the gain and the priority
        }
        // Same gain as the element before this one at the same block: two bits
        // instead of eight, and the reference spells it this way even where it
        // saves nothing. The first element has no element before it, and the
        // reference states its gain outright rather than leaning on the zero a
        // decoder would start from.
        if !first_object && state.gain == carried_gain[index] {
            w.put(2, 3);
        } else {
            match state.gain {
                Gain::Unity => w.put(2, 0),
                Gain::Silent => w.put(2, 1),
                Gain::Decibels(_) => {
                    let code = state
                        .gain
                        .coded()?
                        .expect("a decibel gain has a code or an error");
                    w.put(2, 2);
                    w.put(6, code);
                }
            }
        }
        carried_gain[index] = state.gain;
        w.put(1, 1); // the default priority
    }

    if dynamic {
        let render = state
            .render
            .ok_or_else(|| Unreadable("a dynamic object with no position".into()))?;
        let unchanged = index > 0 && *held_render == render;
        if index > 0 {
            w.put(2, if unchanged { 2 } else { 3 });
        }
        if !unchanged {
            write_render(w, index, &render, held_render)?;
        }
    } else if state.render.is_some() {
        return Err(Unreadable(
            "a bed element with a position of its own".into(),
        ));
    }

    w.put(1, 0); // no table follows
    Ok(())
}

/// The widest step a differential position can say: three bits, two's
/// complement, so a step of four in the positive direction has to be stated
/// outright.
const DIFFERENTIAL: std::ops::RangeInclusive<i32> = -4..=3;

/// Where an element is, and how it is to be rendered there.
///
/// The position goes out as a difference from `held` whenever all three axes
/// can say their step in three bits — nine bits against seventeen — and
/// outright otherwise. A moving object is a small step a block, so the
/// difference is the usual case and the outright form is what a cut costs.
///
/// The base is what the *decoder* holds, which is the previous block of this
/// element after its own clamp, and `Render::default()` where the element was
/// inactive. Both sides carry it the same way, so the writer can lean on it.
fn write_render(w: &mut BitWriter, index: usize, render: &Render, held: &Render) -> Result<()> {
    if index > 0 {
        w.put(4, 15); // every field of it
    }

    let position = render.position;
    if !(0..=MAX_HORIZONTAL).contains(&position.x)
        || !(0..=MAX_HORIZONTAL).contains(&position.y)
        || !(-MAX_VERTICAL..=MAX_VERTICAL).contains(&position.z)
    {
        return Err(Unreadable(format!("a position of {position:?}")));
    }
    let steps = [
        position.x - held.position.x,
        position.y - held.position.y,
        position.z - held.position.z,
    ];
    // The first block has nothing to differ from: the flag itself is not in
    // the syntax there.
    if index > 0 && steps.iter().all(|step| DIFFERENTIAL.contains(step)) {
        w.put(1, 1);
        for step in steps {
            w.put_signed(3, step);
        }
    } else {
        if index > 0 {
            w.put(1, 0); // stated outright
        }
        w.put(6, position.x as u32);
        w.put(6, position.y as u32);
        w.put(1, u32::from(position.z >= 0));
        w.put(4, position.z.unsigned_abs());
    }

    match render.distance {
        Distance::Unstated => w.put(1, 0),
        Distance::Infinite => {
            w.put(1, 1);
            w.put(1, 1);
        }
        Distance::Index(index) => {
            w.put(1, 1);
            w.put(1, 0);
            w.put(4, u32::from(index));
        }
    }

    // Zero is not written as itself: seven is what stands for no constraint,
    // and a decoder reads both as none.
    w.put(3, u32::from(render.zones));
    w.put(1, u32::from(render.elevation));

    match render.size {
        Size::Point => w.put(2, 0),
        Size::Uniform(size) => {
            w.put(2, 1);
            w.put(5, u32::from(size));
        }
        Size::Box(width, depth, height) => {
            w.put(2, 2);
            w.put(5, u32::from(width));
            w.put(5, u32::from(depth));
            w.put(5, u32::from(height));
        }
    }

    match render.screen {
        Some((screen, depth)) => {
            w.put(1, 1);
            w.put(3, u32::from(screen));
            w.put(2, u32::from(depth));
        }
        None => w.put(1, 0),
    }

    w.put(1, u32::from(render.snap));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A moving object's position goes out as a step, and a payload that says
    /// steps is smaller than the same payload saying positions outright.
    ///
    /// The comparison is against the same trajectory scaled past what a
    /// three-bit step can say, so the saving is measured against the writer's
    /// own other branch rather than against a number written down here.
    #[test]
    fn a_position_that_moves_a_little_goes_out_as_a_step() {
        let payload = |stride: i32| {
            let blocks: Vec<Block> = (0..4i32)
                .map(|block| Block {
                    offset: block as u32 * 16,
                    ramp: Ramp::Short,
                    objects: (0..12)
                        .map(|object| Object {
                            active: true,
                            gain: Gain::Unity,
                            render: Some(Render {
                                position: Position {
                                    x: 20 + object + block * stride,
                                    y: 30 + object - block,
                                    z: (object % 3) + block,
                                },
                                ..Render::default()
                            }),
                        })
                        .collect(),
                })
                .collect();
            ObjectAudioMetadata { lfe: false, blocks }
                .write()
                .expect("a writable payload")
        };

        let stepped = payload(2);
        let jumping = payload(8);
        // Thirty-six differentials, eight bits saved by each.
        assert_eq!(jumping.len() - stepped.len(), 36);

        // And the step is a step from what the *decoder* holds, so what comes
        // back is the trajectory that went in.
        let read = ObjectAudioMetadata::read(&stepped).expect("a payload");
        assert_eq!(read.write().unwrap(), stepped);
        assert_eq!(
            read.blocks[3].objects[5].render.unwrap().position.x,
            20 + 5 + 6
        );
    }

    /// An element that was inactive left a decoder holding nothing, so the
    /// block that brings it back states everything rather than saying
    /// "unchanged" from what was forgotten.
    #[test]
    fn an_element_that_comes_back_states_itself_again() {
        let at = |block: usize, active: bool| Object {
            active,
            gain: if active {
                Gain::Decibels(-6)
            } else {
                Gain::Silent
            },
            render: active.then(|| Render {
                position: Position {
                    x: 31,
                    y: 40 + block as i32,
                    z: 3,
                },
                ..Render::default()
            }),
        };
        let blocks: Vec<Block> = (0..3)
            .map(|block| Block {
                offset: block as u32 * 16,
                ramp: Ramp::Short,
                // Two elements, the second of which goes away for one block.
                objects: vec![at(block, true), at(block, block != 1)],
            })
            .collect();

        let payload = ObjectAudioMetadata {
            lfe: false,
            blocks: blocks.clone(),
        };
        let bytes = payload.write().expect("a writable payload");
        let read = ObjectAudioMetadata::read(&bytes).expect("a payload");
        assert_eq!(read.blocks, blocks, "what came back is what went in");
    }

    /// More than thirty-two elements escape into a field of their own, and the
    /// escape is the reader's and the writer's in the same shape.
    #[test]
    fn a_wide_programme_escapes_the_element_count() {
        for count in [31, 32, 33, MAX_OBJECTS] {
            let blocks = vec![Block {
                offset: 0,
                ramp: Ramp::Long,
                objects: (0..count)
                    .map(|object| Object {
                        active: true,
                        gain: Gain::Unity,
                        render: Some(Render {
                            position: Position {
                                x: (object % 63) as i32,
                                y: 0,
                                z: 0,
                            },
                            ..Render::default()
                        }),
                    })
                    .collect(),
            }];
            let payload = ObjectAudioMetadata { lfe: false, blocks };
            let bytes = payload.write().expect("a writable payload");
            let read = ObjectAudioMetadata::read(&bytes).expect("a payload");
            assert_eq!(read, payload, "{count} elements");
            // Thirty-one is the last count that fits the narrow field, and the
            // escape costs seven bits above it.
            assert_eq!(read.objects(), count);
        }
        let too_many = ObjectAudioMetadata {
            lfe: false,
            blocks: vec![Block {
                offset: 0,
                ramp: Ramp::None,
                objects: vec![
                    Object {
                        active: true,
                        gain: Gain::Unity,
                        render: Some(Render::default()),
                    };
                    MAX_OBJECTS + 1
                ],
            }],
        };
        assert!(too_many.write().is_err());
    }

    /// Fifteen elements or more escape their own count, and the fourteen this
    /// does not model are stepped over rather than guessed at.
    ///
    /// Nothing here writes such a payload — one object element is all this
    /// emits — so the bytes are assembled by hand, which is also the only way
    /// to prove the reader steps over an element it has never seen.
    #[test]
    fn a_payload_of_many_elements_steps_over_the_ones_it_cannot_read() {
        let real = ObjectAudioMetadata {
            lfe: false,
            blocks: vec![Block {
                offset: 0,
                ramp: Ramp::Long,
                objects: (0..3)
                    .map(|object| Object {
                        active: true,
                        gain: Gain::Unity,
                        render: Some(Render {
                            position: Position {
                                x: 10 * object,
                                y: 20,
                                z: 1,
                            },
                            ..Render::default()
                        }),
                    })
                    .collect(),
            }],
        };
        let body = real.write_object_element(3).expect("a writable element");

        let mut w = BitWriter::with_capacity(64);
        w.put(2, 0); // version
        w.put(5, 2); // three elements
        w.put(1, 1); // every element a dynamic object
        w.put(1, 0); // no low frequency channel
        w.put(1, 0); // no alternate object data
        w.put(4, 15); // the escape
        w.put(5, 0); // and nothing above it: fifteen elements
        // Fourteen elements this does not model, each one byte of its own.
        for id in 0..14u32 {
            w.put(4, if id == ELEMENT_OBJECT { 15 } else { id });
            w.put_variable(4, 4, 0); // one byte
            w.put(8, 0xa5);
        }
        let content = 1 + body.bit_position();
        let size = content.div_ceil(8);
        w.put(4, ELEMENT_OBJECT);
        w.put_variable(4, 4, size as u32 - 1);
        w.put(1, 0);
        w.put_bits(&body.padded(), body.bit_position());
        w.put_zeroes(size * 8 - content);
        w.align_to(8);

        let read = ObjectAudioMetadata::read(&w.finish()).expect("a payload");
        assert_eq!(read, real, "the object element among fifteen");
    }

    /// The payload of a sixteen-element programme in the shape a delivery
    /// stream carries — a low frequency bed and fifteen dynamic objects, one
    /// block, standing still — pinned byte for byte.
    ///
    /// Pinned rather than regenerated so that a change to the coding is a
    /// failing test and not a silently different stream. What the bytes do
    /// not prove is that the grammar is the format's rather than one that
    /// happens to parse: that is the harness's job, which reads a stream
    /// somebody else wrote and holds this reader's objects next to an
    /// independent decoder's (`cargo xtask thd`, and the `oracle` feature).
    const PINNED: [u8; 65] = [
        0x1f, 0x84, 0x4b, 0x80, 0x00, 0xa2, 0x71, 0x3b, 0x98, 0x40, 0xe4, 0x71, 0x60, 0x81, 0xcc,
        0xd7, 0x21, 0x03, 0xa1, 0x97, 0x02, 0x07, 0x52, 0xf8, 0x04, 0x0e, 0xc5, 0x93, 0x08, 0x1d,
        0xca, 0x6c, 0x10, 0x3c, 0x13, 0x64, 0x20, 0x79, 0x23, 0xe0, 0x40, 0xf4, 0x41, 0x00, 0x81,
        0xec, 0x76, 0x61, 0x03, 0xe0, 0xd5, 0x82, 0x07, 0xd1, 0x7c, 0x84, 0x0f, 0xc2, 0x9c, 0x08,
        0x1f, 0xc4, 0x60, 0x10, 0x00,
    ];

    /// Where the pinned payload puts its objects: a different position for
    /// each, so a field read at the wrong width lands somewhere else.
    fn pinned_position(index: i32) -> Position {
        Position {
            x: (index * 4) % 63,
            y: (62 - index * 3).rem_euclid(63),
            z: (index % 5) * 3,
        }
    }

    #[test]
    fn the_pinned_payload_reads_as_itself() {
        let oamd = ObjectAudioMetadata::read(&PINNED).expect("a payload");

        assert!(oamd.lfe, "the first element is the low frequency channel");
        assert_eq!(oamd.objects(), 16, "one bed and fifteen dynamic objects");
        assert_eq!(oamd.blocks.len(), 1);

        let block = &oamd.blocks[0];
        assert_eq!(block.offset, 0);
        assert_eq!(block.ramp, Ramp::Long, "1536 samples");

        // The bed has no position; every object has its own, with its height
        // honoured.
        assert!(block.objects[0].active);
        assert_eq!(block.objects[0].gain, Gain::Unity);
        assert_eq!(
            block.objects[0].render, None,
            "a bed goes where the bed goes"
        );

        for (index, object) in block.objects.iter().enumerate().skip(1) {
            let render = object.render.expect("a dynamic object has a position");
            assert_eq!(render.position, pinned_position(index as i32), "{index}");
            assert!(render.elevation, "{index}");
            assert!(!render.snap, "{index}");
            assert_eq!(render.size, Size::Point, "{index}");
            assert_eq!(render.distance, Distance::Unstated, "{index}");
            assert_eq!(object.gain, Gain::Unity, "{index}");
        }
    }

    /// And writing back what was read reproduces the pinned bytes.
    ///
    /// Not merely "a payload a decoder accepts": the same payload, which is
    /// what says the writer and the reader are inverses rather than two
    /// guesses that happen to agree with each other.
    #[test]
    fn the_pinned_payload_writes_back_as_itself() {
        let oamd = ObjectAudioMetadata::read(&PINNED).expect("a payload");
        let written = oamd.write().expect("it was read, so it is writable");
        assert_eq!(written, PINNED, "the pinned bytes");

        // And the programme it describes, built from scratch, writes to the
        // same bytes: the pin is the programme and not an accident of the
        // reader.
        let objects: Vec<Object> = (0..16)
            .map(|index| Object {
                active: true,
                gain: Gain::Unity,
                render: (index > 0).then(|| Render {
                    position: pinned_position(index),
                    distance: Distance::Unstated,
                    zones: 0,
                    elevation: true,
                    size: Size::Point,
                    screen: None,
                    snap: false,
                }),
            })
            .collect();
        let built = ObjectAudioMetadata {
            lfe: true,
            blocks: vec![Block {
                offset: 0,
                ramp: Ramp::Long,
                objects,
            }],
        };
        assert_eq!(built.write().unwrap(), PINNED);
    }

    /// A payload with everything in it — several blocks, objects that move and
    /// change gain, and every field of the render info — survives the round
    /// trip. What the pinned payload exercises is one block of objects
    /// standing still, so the rest is exercised here.
    #[test]
    fn a_payload_with_everything_in_it_reads_back_as_itself() {
        let objects = 6;
        let blocks: Vec<Block> = (0..4)
            .map(|block| Block {
                offset: block * 16,
                ramp: match block {
                    0 => Ramp::None,
                    1 => Ramp::Short,
                    2 => Ramp::Named(9),
                    _ => Ramp::Samples(1234),
                },
                objects: (0..objects)
                    .map(|object| {
                        // An element that is not active states nothing else,
                        // so that is what it has to be given here: the syntax
                        // has nowhere to put a gain or a position for it.
                        if object == 4 && block == 2 {
                            return Object {
                                active: false,
                                gain: Gain::Silent,
                                render: None,
                            };
                        }
                        Object {
                            active: true,
                            gain: match (object + block as usize) % 4 {
                                0 => Gain::Unity,
                                1 => Gain::Silent,
                                2 => Gain::Decibels(-13),
                                _ => Gain::Decibels(7),
                            },
                            render: (object > 0).then(|| Render {
                                position: Position {
                                    x: (object as i32 * 7 + block as i32) % 63,
                                    y: (object as i32 * 11) % 63,
                                    z: (object as i32 * 5) % 16 - 8,
                                },
                                distance: match object % 3 {
                                    0 => Distance::Unstated,
                                    1 => Distance::Infinite,
                                    _ => Distance::Index(9),
                                },
                                zones: (object as u8) % 7,
                                elevation: object % 2 == 0,
                                size: match object % 3 {
                                    0 => Size::Point,
                                    1 => Size::Uniform(17),
                                    _ => Size::Box(3, 9, 30),
                                },
                                screen: (object % 4 == 1).then_some((5, 2)),
                                snap: object % 2 == 1,
                            }),
                        }
                    })
                    .collect(),
            })
            .collect();

        let oamd = ObjectAudioMetadata { lfe: true, blocks };
        let bytes = oamd.write().expect("a writable payload");
        let read = ObjectAudioMetadata::read(&bytes).expect("and a readable one");
        assert_eq!(read, oamd);
        // And writing what came back is the same bytes again, which is what
        // says the coding has no state the round trip smuggles past it.
        assert_eq!(read.write().unwrap(), bytes);
    }

    /// The gain field reaches +15 dB down to −49 and cannot say zero, which is
    /// what its own code is for. Every value it can carry comes back.
    #[test]
    fn every_gain_the_field_can_carry_reads_back() {
        for db in -49i8..=15 {
            let gain = Gain::from_decibels(db).expect("in range");
            let oamd = ObjectAudioMetadata {
                lfe: false,
                blocks: vec![Block {
                    offset: 0,
                    ramp: Ramp::None,
                    objects: vec![Object {
                        active: true,
                        gain,
                        render: Some(Render::default()),
                    }],
                }],
            };
            let read = ObjectAudioMetadata::read(&oamd.write().unwrap()).unwrap();
            assert_eq!(read.blocks[0].objects[0].gain, gain, "{db} dB");
            assert_eq!(
                read.blocks[0].objects[0].gain.decibels(),
                Some(db),
                "{db} dB"
            );
        }
        assert!(Gain::from_decibels(16).is_err());
        assert!(Gain::from_decibels(-50).is_err());
        assert_eq!(Gain::Silent.decibels(), None);
    }

    /// A programme shape this does not model is refused by name rather than
    /// read as something else: a payload whose objects land somewhere else is
    /// worse than one that does not parse.
    #[test]
    fn a_programme_shape_that_is_not_modelled_is_refused() {
        // Version zero, one element, then a program assignment that is not the
        // dynamic-object-only one.
        let bytes = [0b0000_0000, 0u8, 0, 0];
        let why = ObjectAudioMetadata::read(&bytes).unwrap_err().to_string();
        assert!(why.contains("speaker-anchored"), "{why}");
    }

    /// Positions cross into the master format's frame and back without
    /// drifting, which is where a sign or a factor of two would show.
    #[test]
    fn a_position_survives_the_trip_through_the_master_frame() {
        for x in [0, 1, 31, 61, 62] {
            for z in [-15, -1, 0, 1, 15] {
                let position = Position { x, y: 62 - x, z };
                assert_eq!(Position::from_master(position.to_master()), position);
            }
        }
        // The corners, named rather than computed.
        assert_eq!(Position { x: 0, y: 0, z: 0 }.to_master(), [-1.0, 1.0, 0.0]);
        assert_eq!(
            Position {
                x: 62,
                y: 62,
                z: 15
            }
            .to_master(),
            [1.0, -1.0, 1.0]
        );
        assert_eq!(Position::CENTRE.to_master()[2], 0.0);
    }
}

#[cfg(test)]
mod field_tests {
    use super::*;

    /// Every spelling of a ramp says how many samples it is, and the shortest
    /// spelling of that number is the one a writer picks.
    #[test]
    fn a_ramp_round_trips_through_its_length() {
        assert_eq!(Ramp::None.samples(), 0);
        assert_eq!(Ramp::Short.samples(), 512);
        assert_eq!(Ramp::Long.samples(), 1536);
        assert_eq!(Ramp::Named(0).samples(), 32);
        assert_eq!(Ramp::Named(15).samples(), 2048);
        assert_eq!(Ramp::Samples(777).samples(), 777);

        for samples in 0..=2048u16 {
            let Some(ramp) = Ramp::from_samples(samples) else {
                panic!("{samples} samples should be sayable");
            };
            assert_eq!(ramp.samples(), samples, "{samples}");
        }
        // Past the outright count's eleven bits, and not one of the named
        // lengths.
        assert_eq!(Ramp::from_samples(2049), None);
        assert_eq!(Ramp::from_samples(u16::MAX), None);
    }

    /// The named index is seven bits and an outright count is fourteen, so a
    /// length that has a name is written by name.
    #[test]
    fn a_named_length_is_preferred_to_an_outright_count() {
        for (index, samples) in NAMED_RAMPS.iter().enumerate() {
            assert_eq!(
                Ramp::from_samples(*samples),
                Some(Ramp::Named(index as u8)),
                "{samples} samples has a name"
            );
        }
        // Except where a two-bit code says it in even fewer.
        assert_eq!(Ramp::from_samples(512), Some(Ramp::Short));
        assert_eq!(Ramp::from_samples(1536), Some(Ramp::Long));
    }

    /// A gain the field cannot hold has to be refused rather than truncated
    /// into a different one: the code is six bits and the writer masks.
    #[test]
    fn a_gain_outside_the_field_is_refused() {
        assert!(Gain::from_decibels(16).is_err());
        assert!(Gain::from_decibels(-50).is_err());
        assert_eq!(Gain::from_decibels(0).unwrap(), Gain::Unity);
        assert_eq!(Gain::from_decibels(-6).unwrap(), Gain::Decibels(-6));

        // And a value built by hand, which is where a caller gets it wrong.
        assert!(Gain::Decibels(30).coded().is_err());
        assert!(Gain::Decibels(0).coded().is_err());
        assert!(Gain::Decibels(-49).coded().is_ok());
        assert!(Gain::Decibels(15).coded().is_ok());
    }

    /// A linear amplitude becomes the whole decibel nearest it, and silence
    /// stays silence rather than becoming the quietest gain there is.
    #[test]
    fn a_linear_gain_rounds_to_the_field() {
        assert_eq!(Gain::from_linear(1.0).unwrap(), Gain::Unity);
        assert_eq!(Gain::from_linear(0.0).unwrap(), Gain::Silent);
        assert_eq!(Gain::from_linear(1e-6).unwrap(), Gain::Silent);
        assert_eq!(Gain::from_linear(0.5).unwrap(), Gain::Decibels(-6));
        assert_eq!(Gain::from_linear(2.0).unwrap(), Gain::Decibels(6));
        // Above the ceiling it clamps rather than failing: a mix asking for
        // more than +15 dB gets the loudest the field has.
        assert_eq!(Gain::from_linear(100.0).unwrap(), Gain::Decibels(15));
        assert!(Gain::from_linear(-1.0).is_err());
        assert!(Gain::from_linear(f64::NAN).is_err());
    }
}
