// SPDX-License-Identifier: GPL-3.0-or-later
//
// Carries material ported from FFmpeg, which is licensed under the GNU Lesser
// General Public License, version 2.1 or later, and is used here under the
// GPL-3.0-or-later of this project as that licence permits:
//   libavcodec/ac3tab.c, libavcodec/ac3.c — Copyright (c) 2000 Fabrice Bellard
//   libavcodec/ac3enc.c — Copyright (c) 2000 Fabrice Bellard,
//                         2006-2010 Justin Ruggles
// What was taken and what was changed is recorded in docs/provenance.md.

//! The tables the format is made of.
//!
//! All but the last are the format's own — ATSC A/52 states them, and a
//! decoder holds the same ones — and were transcribed from FFmpeg's copy
//! rather than from the standard's PDF, because a transcription that has
//! already been decoded against for twenty years is the safer one. The last,
//! [`BANDWIDTH`], is FFmpeg's encoder's choice of audio bandwidth per bit
//! rate, and nothing a decoder sees.

/// The sample rates a syncframe can say, indexed by `fscod`.
pub const SAMPLE_RATES: [u32; 3] = [48_000, 44_100, 32_000];

/// The bit rates, in kb/s, indexed by `frmsizecod / 2`.
pub const BIT_RATES: [u16; 19] = [
    32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 384, 448, 512, 576, 640,
];

/// Syncframe sizes in 16-bit words, by `frmsizecod` and `fscod` (A/52 table
/// 5.18). At 44.1 kHz the two codes of a pair differ by a word, which is how
/// a stream at that rate keeps its average bit rate.
pub const FRAME_WORDS: [[u16; 3]; 38] = [
    [64, 69, 96],
    [64, 70, 96],
    [80, 87, 120],
    [80, 88, 120],
    [96, 104, 144],
    [96, 105, 144],
    [112, 121, 168],
    [112, 122, 168],
    [128, 139, 192],
    [128, 140, 192],
    [160, 174, 240],
    [160, 175, 240],
    [192, 208, 288],
    [192, 209, 288],
    [224, 243, 336],
    [224, 244, 336],
    [256, 278, 384],
    [256, 279, 384],
    [320, 348, 480],
    [320, 349, 480],
    [384, 417, 576],
    [384, 418, 576],
    [448, 487, 672],
    [448, 488, 672],
    [512, 557, 768],
    [512, 558, 768],
    [640, 696, 960],
    [640, 697, 960],
    [768, 835, 1152],
    [768, 836, 1152],
    [896, 975, 1344],
    [896, 976, 1344],
    [1024, 1114, 1536],
    [1024, 1115, 1536],
    [1152, 1253, 1728],
    [1152, 1254, 1728],
    [1280, 1393, 1920],
    [1280, 1394, 1920],
];

/// The first bin of each of the four rematrixing bands, and the end of the
/// last.
pub const REMATRIX_BANDS: [usize; 5] = [13, 25, 37, 61, 253];

/// The first bin of each of the fifty bit allocation bands, and the end of the
/// last.
pub const BAND_START: [u8; 51] = [
    0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25,
    26, 27, 28, 31, 34, 37, 40, 43, 46, 49, 55, 61, 67, 73, 79, 85, 97, 109, 121, 133, 157, 181,
    205, 229, 253,
];

/// The bit allocation band each bin falls in.
pub const BIN_TO_BAND: [u8; 253] = [
    0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25,
    26, 27, 28, 28, 28, 29, 29, 29, 30, 30, 30, 31, 31, 31, 32, 32, 32, 33, 33, 33, 34, 34, 34, 35,
    35, 35, 35, 35, 35, 36, 36, 36, 36, 36, 36, 37, 37, 37, 37, 37, 37, 38, 38, 38, 38, 38, 38, 39,
    39, 39, 39, 39, 39, 40, 40, 40, 40, 40, 40, 41, 41, 41, 41, 41, 41, 41, 41, 41, 41, 41, 41, 42,
    42, 42, 42, 42, 42, 42, 42, 42, 42, 42, 42, 43, 43, 43, 43, 43, 43, 43, 43, 43, 43, 43, 43, 44,
    44, 44, 44, 44, 44, 44, 44, 44, 44, 44, 44, 45, 45, 45, 45, 45, 45, 45, 45, 45, 45, 45, 45, 45,
    45, 45, 45, 45, 45, 45, 45, 45, 45, 45, 45, 46, 46, 46, 46, 46, 46, 46, 46, 46, 46, 46, 46, 46,
    46, 46, 46, 46, 46, 46, 46, 46, 46, 46, 46, 47, 47, 47, 47, 47, 47, 47, 47, 47, 47, 47, 47, 47,
    47, 47, 47, 47, 47, 47, 47, 47, 47, 47, 47, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
    48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 49, 49, 49, 49, 49, 49, 49, 49, 49, 49, 49, 49, 49,
    49, 49, 49, 49, 49, 49, 49, 49, 49, 49, 49,
];

/// What adding two power spectral densities in the log domain adds to the
/// larger, by the difference between them.
pub const LOG_ADD: [u8; 260] = [
    0x40, 0x3f, 0x3e, 0x3d, 0x3c, 0x3b, 0x3a, 0x39, 0x38, 0x37, 0x36, 0x35, 0x34, 0x34, 0x33, 0x32,
    0x31, 0x30, 0x2f, 0x2f, 0x2e, 0x2d, 0x2c, 0x2c, 0x2b, 0x2a, 0x29, 0x29, 0x28, 0x27, 0x26, 0x26,
    0x25, 0x24, 0x24, 0x23, 0x23, 0x22, 0x21, 0x21, 0x20, 0x20, 0x1f, 0x1e, 0x1e, 0x1d, 0x1d, 0x1c,
    0x1c, 0x1b, 0x1b, 0x1a, 0x1a, 0x19, 0x19, 0x18, 0x18, 0x17, 0x17, 0x16, 0x16, 0x15, 0x15, 0x15,
    0x14, 0x14, 0x13, 0x13, 0x13, 0x12, 0x12, 0x12, 0x11, 0x11, 0x11, 0x10, 0x10, 0x10, 0x0f, 0x0f,
    0x0f, 0x0e, 0x0e, 0x0e, 0x0d, 0x0d, 0x0d, 0x0d, 0x0c, 0x0c, 0x0c, 0x0c, 0x0b, 0x0b, 0x0b, 0x0b,
    0x0a, 0x0a, 0x0a, 0x0a, 0x0a, 0x09, 0x09, 0x09, 0x09, 0x09, 0x08, 0x08, 0x08, 0x08, 0x08, 0x08,
    0x07, 0x07, 0x07, 0x07, 0x07, 0x07, 0x06, 0x06, 0x06, 0x06, 0x06, 0x06, 0x06, 0x06, 0x05, 0x05,
    0x05, 0x05, 0x05, 0x05, 0x05, 0x05, 0x04, 0x04, 0x04, 0x04, 0x04, 0x04, 0x04, 0x04, 0x04, 0x04,
    0x04, 0x03, 0x03, 0x03, 0x03, 0x03, 0x03, 0x03, 0x03, 0x03, 0x03, 0x03, 0x03, 0x03, 0x03, 0x02,
    0x02, 0x02, 0x02, 0x02, 0x02, 0x02, 0x02, 0x02, 0x02, 0x02, 0x02, 0x02, 0x02, 0x02, 0x02, 0x02,
    0x02, 0x02, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01,
    0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01,
    0x01, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00,
];

/// The absolute hearing threshold per band, by `fscod`.
pub const HEARING_THRESHOLD: [[u16; 3]; 50] = [
    [0x04d0, 0x04f0, 0x0580],
    [0x04d0, 0x04f0, 0x0580],
    [0x0440, 0x0460, 0x04b0],
    [0x0400, 0x0410, 0x0450],
    [0x03e0, 0x03e0, 0x0420],
    [0x03c0, 0x03d0, 0x03f0],
    [0x03b0, 0x03c0, 0x03e0],
    [0x03b0, 0x03b0, 0x03d0],
    [0x03a0, 0x03b0, 0x03c0],
    [0x03a0, 0x03a0, 0x03b0],
    [0x03a0, 0x03a0, 0x03b0],
    [0x03a0, 0x03a0, 0x03b0],
    [0x03a0, 0x03a0, 0x03a0],
    [0x0390, 0x03a0, 0x03a0],
    [0x0390, 0x0390, 0x03a0],
    [0x0390, 0x0390, 0x03a0],
    [0x0380, 0x0390, 0x03a0],
    [0x0380, 0x0380, 0x03a0],
    [0x0370, 0x0380, 0x03a0],
    [0x0370, 0x0380, 0x03a0],
    [0x0360, 0x0370, 0x0390],
    [0x0360, 0x0370, 0x0390],
    [0x0350, 0x0360, 0x0390],
    [0x0350, 0x0360, 0x0390],
    [0x0340, 0x0350, 0x0380],
    [0x0340, 0x0350, 0x0380],
    [0x0330, 0x0340, 0x0380],
    [0x0320, 0x0340, 0x0370],
    [0x0310, 0x0320, 0x0360],
    [0x0300, 0x0310, 0x0350],
    [0x02f0, 0x0300, 0x0340],
    [0x02f0, 0x02f0, 0x0330],
    [0x02f0, 0x02f0, 0x0320],
    [0x02f0, 0x02f0, 0x0310],
    [0x0300, 0x02f0, 0x0300],
    [0x0310, 0x0300, 0x02f0],
    [0x0340, 0x0320, 0x02f0],
    [0x0390, 0x0350, 0x02f0],
    [0x03e0, 0x0390, 0x0300],
    [0x0420, 0x03e0, 0x0310],
    [0x0460, 0x0420, 0x0330],
    [0x0490, 0x0450, 0x0350],
    [0x04a0, 0x04a0, 0x03c0],
    [0x0460, 0x0490, 0x0410],
    [0x0440, 0x0460, 0x0470],
    [0x0440, 0x0440, 0x04a0],
    [0x0520, 0x0480, 0x0460],
    [0x0800, 0x0630, 0x0440],
    [0x0840, 0x0840, 0x0450],
    [0x0840, 0x0840, 0x04e0],
];

/// The bit allocation pointer for each of the 64 addresses a bin's distance
/// above its mask maps to.
pub const BAP: [u8; 64] = [
    0, 1, 1, 1, 1, 1, 2, 2, 3, 3, 3, 4, 4, 5, 5, 6, 6, 6, 6, 7, 7, 7, 7, 8, 8, 8, 8, 9, 9, 9, 9,
    10, 10, 10, 10, 11, 11, 11, 11, 12, 12, 12, 12, 13, 13, 13, 13, 14, 14, 14, 14, 14, 14, 14, 14,
    15, 15, 15, 15, 15, 15, 15, 15, 15,
];

/// Bits per mantissa for each pointer from five up; below five, mantissas are
/// grouped and counted apart.
pub const BAP_BITS: [u16; 16] = [0, 0, 0, 3, 0, 4, 5, 6, 7, 8, 9, 10, 11, 12, 14, 16];

pub const SLOW_DECAY: [i32; 4] = [0x0f, 0x11, 0x13, 0x15];
pub const FAST_DECAY: [i32; 4] = [0x3f, 0x53, 0x67, 0x7b];
pub const SLOW_GAIN: [i32; 4] = [0x540, 0x4d8, 0x478, 0x410];
pub const DB_PER_BIT: [i32; 4] = [0x000, 0x700, 0x900, 0xb00];
pub const FLOOR: [i32; 8] = [0x2f0, 0x2b0, 0x270, 0x230, 0x1f0, 0x170, 0x0f0, -0x800];
pub const FAST_GAIN: [i32; 8] = [0x080, 0x100, 0x180, 0x200, 0x280, 0x300, 0x380, 0x400];

/// The audio bandwidth code FFmpeg's encoder chooses, by full-bandwidth
/// channel count less one, `fscod` and bit rate index. Not the format's: a
/// choice, ported with the rest so that the first comparison against
/// `ffmpeg -c:a ac3` compares the transform and the allocation and not two
/// different bandwidths.
pub const BANDWIDTH: [[[u8; 19]; 3]; 5] = [
    [
        [
            0, 0, 0, 12, 16, 32, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
        ],
        [
            0, 0, 0, 16, 20, 36, 56, 56, 56, 56, 56, 56, 56, 56, 56, 56, 56, 56, 56,
        ],
        [
            0, 0, 0, 32, 40, 60, 60, 60, 60, 60, 60, 60, 60, 60, 60, 60, 60, 60, 60,
        ],
    ],
    [
        [
            0, 0, 0, 0, 0, 0, 0, 20, 24, 32, 48, 48, 48, 48, 48, 48, 48, 48, 48,
        ],
        [
            0, 0, 0, 0, 0, 0, 4, 24, 28, 36, 56, 56, 56, 56, 56, 56, 56, 56, 56,
        ],
        [
            0, 0, 0, 0, 0, 0, 20, 44, 52, 60, 60, 60, 60, 60, 60, 60, 60, 60, 60,
        ],
    ],
    [
        [
            0, 0, 0, 0, 0, 0, 0, 0, 0, 16, 24, 32, 40, 48, 48, 48, 48, 48, 48,
        ],
        [
            0, 0, 0, 0, 0, 0, 0, 0, 4, 20, 28, 36, 44, 56, 56, 56, 56, 56, 56,
        ],
        [
            0, 0, 0, 0, 0, 0, 0, 0, 20, 40, 48, 60, 60, 60, 60, 60, 60, 60, 60,
        ],
    ],
    [
        [
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 12, 24, 32, 48, 48, 48, 48, 48, 48,
        ],
        [
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 16, 28, 36, 56, 56, 56, 56, 56, 56,
        ],
        [
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 32, 48, 60, 60, 60, 60, 60, 60, 60,
        ],
    ],
    [
        [
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 8, 20, 32, 40, 48, 48, 48, 48,
        ],
        [
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 12, 24, 36, 44, 56, 56, 56, 56,
        ],
        [
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 28, 44, 60, 60, 60, 60, 60, 60,
        ],
    ],
];

/// The default coupling band structure (A/52 table 7.24, E-AC-3's E2.16):
/// whether each sub-band of twelve bins joins the band before it.
pub const DEFAULT_CPL_BAND_STRUCT: [bool; 18] = [
    false, false, false, false, false, false, false, false, true, false, true, true, false, true,
    true, true, true, true,
];

/// The coupling start sub-band FFmpeg's encoder chooses, by channel mode less
/// two, `fscod` and bit rate index; −1 leaves coupling off. Like
/// [`BANDWIDTH`], a choice and not the format's.
pub const CPL_START: [[[i8; 19]; 3]; 6] = [
    // 2/0
    [
        [
            0, 0, 0, 0, 0, 0, 0, 1, 1, 7, 8, 11, 12, -1, -1, -1, -1, -1, -1,
        ],
        [
            0, 0, 0, 0, 0, 0, 1, 3, 5, 7, 10, 12, 13, -1, -1, -1, -1, -1, -1,
        ],
        [
            0, 0, 0, 0, 1, 2, 2, 9, 13, 15, -1, -1, -1, -1, -1, -1, -1, -1, -1,
        ],
    ],
    // 3/0
    [
        [
            0, 0, 0, 0, 0, 0, 0, 0, 2, 2, 6, 9, 11, 12, 13, -1, -1, -1, -1,
        ],
        [
            0, 0, 0, 0, 0, 0, 0, 0, 2, 2, 6, 9, 11, 12, 13, -1, -1, -1, -1,
        ],
        [-1; 19],
    ],
    // 2/1
    [
        [
            0, 0, 0, 0, 0, 0, 0, 0, 2, 2, 6, 9, 11, 12, 13, -1, -1, -1, -1,
        ],
        [
            0, 0, 0, 0, 0, 0, 0, 0, 2, 2, 6, 9, 11, 12, 13, -1, -1, -1, -1,
        ],
        [-1; 19],
    ],
    // 3/1
    [
        [
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 3, 2, 10, 11, 11, 12, 12, 14, -1,
        ],
        [
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 3, 2, 10, 11, 11, 12, 12, 14, -1,
        ],
        [-1; 19],
    ],
    // 2/2
    [
        [
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 3, 2, 10, 11, 11, 12, 12, 14, -1,
        ],
        [
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 3, 2, 10, 11, 11, 12, 12, 14, -1,
        ],
        [-1; 19],
    ],
    // 3/2
    [
        [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 6, 8, 11, 12, 12, -1, -1],
        [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 6, 8, 11, 12, 12, -1, -1],
        [-1; 19],
    ],
];

#[cfg(test)]
mod tests {
    use super::*;

    /// The band table and the bin table describe the same partition.
    #[test]
    fn the_band_tables_agree() {
        for band in 0..50 {
            for bin in BAND_START[band]..BAND_START[band + 1] {
                assert_eq!(
                    usize::from(BIN_TO_BAND[usize::from(bin)]),
                    band,
                    "bin {bin}"
                );
            }
        }
    }

    /// Frame sizes are the bit rate's: at 48 kHz a syncframe of 1536 samples
    /// lasts 32 ms, so a word count is the rate in kb/s times two.
    #[test]
    fn the_frame_sizes_are_the_bit_rates() {
        for (code, words) in FRAME_WORDS.iter().enumerate() {
            let rate = u32::from(BIT_RATES[code / 2]);
            assert_eq!(u32::from(words[0]), rate * 2);
            assert_eq!(u32::from(words[2]), rate * 3);
            // 44.1 kHz: a frame lasts 1536 / 44 100 s, which is a fraction of
            // a word that the odd code of each pair rounds up.
            let exact = rate as f64 * 1000.0 * 1536.0 / 44_100.0 / 16.0;
            assert_eq!(
                u32::from(words[1]),
                exact.floor() as u32 + (code as u32 & 1)
            );
        }
    }
}
