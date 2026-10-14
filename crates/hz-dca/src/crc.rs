//! The CRC-16 the format protects its headers with.
//!
//! CCITT polynomial `0x1021`, initial value `0xffff`, most significant bit
//! first, no reflection and no final XOR (ETSI TS 102 114, the extension
//! substream header's `nCRC16ExtSSHeader`). Written so that the residue over a
//! protected range *including* its two checksum bytes is zero, which is how a
//! decoder checks it and how [`check`] does.

/// The table, built at compile time: a header is checked every frame, and a
/// bit-at-a-time loop over it is eight times the work for no gain.
const TABLE: [u16; 256] = {
    let mut table = [0u16; 256];
    let mut byte = 0;
    while byte < 256 {
        let mut crc = (byte as u16) << 8;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 0x8000 != 0 {
                (crc << 1) ^ 0x1021
            } else {
                crc << 1
            };
            bit += 1;
        }
        table[byte] = crc;
        byte += 1;
    }
    table
};

/// The checksum of `bytes`, to be stored big-endian after them.
pub fn crc16(bytes: &[u8]) -> u16 {
    let mut crc = 0xffffu16;
    for &byte in bytes {
        crc = (crc << 8) ^ TABLE[usize::from((crc >> 8) as u8 ^ byte)];
    }
    crc
}

/// Whether `protected` — data followed by its two checksum bytes — is intact.
pub fn check(protected: &[u8]) -> bool {
    protected.len() >= 2 && crc16(protected) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_standard_check_value() {
        // CRC-16/CCITT-FALSE of "123456789".
        assert_eq!(crc16(b"123456789"), 0x29b1);
    }

    #[test]
    fn the_residue_over_data_and_checksum_is_zero() {
        let data = [0x12u8, 0x34, 0x56, 0x78, 0x9a];
        let crc = crc16(&data);
        let mut protected = data.to_vec();
        protected.extend_from_slice(&crc.to_be_bytes());
        assert!(check(&protected));
        protected[2] ^= 0x10;
        assert!(!check(&protected));
    }
}
