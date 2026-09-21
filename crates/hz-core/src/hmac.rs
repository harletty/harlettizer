//! SHA-256, and a keyed digest built on it.
//!
//! Written here rather than pulled in, because this crate sits under every
//! other one — including the codecs, which stay dependency-free so that
//! nothing in a hot loop has to be trusted or audited for the embedded target
//! — and a hash is two hundred lines of arithmetic with published test
//! vectors, which is less to carry than a dependency tree.
//!
//! Both are the standard constructions: SHA-256 as FIPS 180-4 specifies it and
//! the keyed digest as RFC 2104 does — the key padded to a block, folded with
//! `0x36` for the inner hash and `0x5c` for the outer, and a key longer than a
//! block hashed down first. The tests are the published vectors, FIPS 180-4's
//! for the hash and RFC 4231's for the keyed digest, which is what says the
//! arithmetic is the standard's and not merely self-consistent.
//!
//! What a keyed digest is for here: an Evolution frame closes with a
//! protection field the format leaves to the implementation. A writer that
//! has the key fills it with the digest of the access unit and the frame; one
//! that does not writes a constant. See `hz_mlp::protection`.

/// The bytes a SHA-256 digest occupies.
pub const DIGEST_BYTES: usize = 32;

/// The block SHA-256 consumes at a time, which is also the width a keyed
/// digest pads its key to.
const BLOCK_BYTES: usize = 64;

/// The first thirty-two bits of the fractional parts of the cube roots of the
/// first sixty-four primes.
const K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

/// The first thirty-two bits of the fractional parts of the square roots of
/// the first eight primes.
const INITIAL: [u32; 8] = [
    0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
];

/// A SHA-256 digest in progress.
///
/// Fed in any pieces; a message split at any byte hashes the same as the
/// whole. Sixty-four bytes are held between calls, which is what lets a digest
/// be cloned mid-way — the keyed digest below does that to reuse a key's
/// padded blocks across messages.
#[derive(Debug, Clone)]
pub struct Sha256 {
    state: [u32; 8],
    block: [u8; BLOCK_BYTES],
    /// Bytes of `block` in use, `0..64`.
    held: usize,
    /// Bytes fed so far, which the padding states in bits.
    fed: u64,
}

impl Default for Sha256 {
    fn default() -> Self {
        Self::new()
    }
}

impl Sha256 {
    pub fn new() -> Self {
        Self {
            state: INITIAL,
            block: [0; BLOCK_BYTES],
            held: 0,
            fed: 0,
        }
    }

    /// The digest of `bytes`, in one call.
    pub fn digest(bytes: &[u8]) -> [u8; DIGEST_BYTES] {
        let mut hash = Self::new();
        hash.update(bytes);
        hash.finish()
    }

    /// Feed more of the message.
    pub fn update(&mut self, mut bytes: &[u8]) {
        self.fed = self.fed.wrapping_add(bytes.len() as u64);

        // Top up a partial block first, and only compress it if it fills.
        if self.held > 0 {
            let room = BLOCK_BYTES - self.held;
            let take = room.min(bytes.len());
            self.block[self.held..self.held + take].copy_from_slice(&bytes[..take]);
            self.held += take;
            bytes = &bytes[take..];
            if self.held < BLOCK_BYTES {
                return;
            }
            let block = self.block;
            compress(&mut self.state, &block);
            self.held = 0;
        }

        // Whole blocks straight from the input, without copying them.
        let mut whole = bytes.chunks_exact(BLOCK_BYTES);
        for block in &mut whole {
            compress(&mut self.state, block.try_into().expect("a whole block"));
        }
        let rest = whole.remainder();
        self.block[..rest.len()].copy_from_slice(rest);
        self.held = rest.len();
    }

    /// Pad, and produce the digest.
    pub fn finish(mut self) -> [u8; DIGEST_BYTES] {
        let bits = self.fed.wrapping_mul(8);
        // One bit, then zeroes to eight bytes short of a block boundary, then
        // the length. If the one bit lands in the last eight bytes of a block
        // the length goes in the next one.
        self.update(&[0x80]);
        while self.held != BLOCK_BYTES - 8 {
            self.update(&[0]);
        }
        // Not through `update`: the length is not part of the message.
        self.block[BLOCK_BYTES - 8..].copy_from_slice(&bits.to_be_bytes());
        let block = self.block;
        compress(&mut self.state, &block);

        let mut out = [0u8; DIGEST_BYTES];
        for (word, chunk) in self.state.iter().zip(out.chunks_exact_mut(4)) {
            chunk.copy_from_slice(&word.to_be_bytes());
        }
        out
    }
}

/// One block through the compression function.
fn compress(state: &mut [u32; 8], block: &[u8; BLOCK_BYTES]) {
    let mut w = [0u32; 64];
    for (t, chunk) in block.chunks_exact(4).enumerate() {
        w[t] = u32::from_be_bytes(chunk.try_into().expect("four bytes"));
    }
    for t in 16..64 {
        let s0 = w[t - 15].rotate_right(7) ^ w[t - 15].rotate_right(18) ^ (w[t - 15] >> 3);
        let s1 = w[t - 2].rotate_right(17) ^ w[t - 2].rotate_right(19) ^ (w[t - 2] >> 10);
        w[t] = w[t - 16]
            .wrapping_add(s0)
            .wrapping_add(w[t - 7])
            .wrapping_add(s1);
    }

    let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = *state;
    for t in 0..64 {
        let big_s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
        let choose = (e & f) ^ (!e & g);
        let t1 = h
            .wrapping_add(big_s1)
            .wrapping_add(choose)
            .wrapping_add(K[t])
            .wrapping_add(w[t]);
        let big_s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
        let majority = (a & b) ^ (a & c) ^ (b & c);
        let t2 = big_s0.wrapping_add(majority);
        h = g;
        g = f;
        f = e;
        e = d.wrapping_add(t1);
        d = c;
        c = b;
        b = a;
        a = t1.wrapping_add(t2);
    }

    for (word, add) in state.iter_mut().zip([a, b, c, d, e, f, g, h]) {
        *word = word.wrapping_add(add);
    }
}

/// A keyed SHA-256 digest, with its key already folded in.
///
/// Holds the two hashes a key opens — the inner one over the key folded with
/// `0x36`, the outer over the key folded with `0x5c` — so that signing a
/// message is two clones and no work on the key. A stream signs one access
/// unit at a time, tens of thousands of them, and the key is the same for all.
#[derive(Debug, Clone)]
pub struct HmacSha256 {
    inner: Sha256,
    outer: Sha256,
}

impl HmacSha256 {
    /// Open a key. Any length: one longer than a block is hashed down first,
    /// and any length is padded up with zeroes.
    pub fn new(key: &[u8]) -> Self {
        let mut padded = [0u8; BLOCK_BYTES];
        if key.len() > BLOCK_BYTES {
            padded[..DIGEST_BYTES].copy_from_slice(&Sha256::digest(key));
        } else {
            padded[..key.len()].copy_from_slice(key);
        }

        let mut inner = Sha256::new();
        let mut outer = Sha256::new();
        let mut block = [0u8; BLOCK_BYTES];
        for (out, byte) in block.iter_mut().zip(padded) {
            *out = byte ^ 0x36;
        }
        inner.update(&block);
        for (out, byte) in block.iter_mut().zip(padded) {
            *out = byte ^ 0x5c;
        }
        outer.update(&block);
        Self { inner, outer }
    }

    /// Start a message.
    pub fn sign(&self) -> Signer<'_> {
        Signer {
            inner: self.inner.clone(),
            key: self,
        }
    }

    /// The digest of one message given whole.
    pub fn digest(&self, message: &[u8]) -> [u8; DIGEST_BYTES] {
        let mut signer = self.sign();
        signer.update(message);
        signer.finish()
    }
}

/// One message being signed.
#[derive(Debug, Clone)]
pub struct Signer<'a> {
    inner: Sha256,
    key: &'a HmacSha256,
}

impl Signer<'_> {
    pub fn update(&mut self, bytes: &[u8]) {
        self.inner.update(bytes);
    }

    pub fn finish(self) -> [u8; DIGEST_BYTES] {
        let mut outer = self.key.outer.clone();
        outer.update(&self.inner.finish());
        outer.finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    fn unhex(text: &str) -> Vec<u8> {
        (0..text.len())
            .step_by(2)
            .map(|at| u8::from_str_radix(&text[at..at + 2], 16).unwrap())
            .collect()
    }

    /// FIPS 180-4's examples, and the empty message.
    #[test]
    fn the_published_sha256_vectors() {
        assert_eq!(
            hex(&Sha256::digest(b"")),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            hex(&Sha256::digest(b"abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            hex(&Sha256::digest(
                b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"
            )),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
        assert_eq!(
            hex(&Sha256::digest(
                b"abcdefghbcdefghicdefghijdefghijkefghijklfghijklmghijklmnhijklmnoijklmnopjklmnopqklmnopqrlmnopqrsmnopqrstnopqrstu"
            )),
            "cf5b16a778af8380036ce59e7b0492370b249b11e8f07a51afac45037afee9d1"
        );
        let mut hash = Sha256::new();
        for _ in 0..1_000_000 {
            hash.update(b"a");
        }
        assert_eq!(
            hex(&hash.finish()),
            "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0"
        );
    }

    /// The padding's two cases: a message whose length lands in the last
    /// eight bytes of a block needs a second block for its length, and one
    /// that fills a block exactly needs a whole block of padding.
    #[test]
    fn a_message_of_any_length_hashes_the_same_however_it_is_fed() {
        for length in [55usize, 56, 63, 64, 65, 119, 120, 127, 128, 129, 1000] {
            let message: Vec<u8> = (0..length).map(|n| (n * 31 + 7) as u8).collect();
            let whole = Sha256::digest(&message);
            for split in [1usize, 3, 17, 63, 64, 65] {
                let mut hash = Sha256::new();
                for piece in message.chunks(split) {
                    hash.update(piece);
                }
                assert_eq!(
                    hash.finish(),
                    whole,
                    "length {length}, fed {split} at a time"
                );
            }
        }
    }

    /// RFC 4231's test cases: a short key, a text key, a key of one byte
    /// repeated, one longer than a block, and one longer than a block over a
    /// long message.
    #[test]
    fn the_published_hmac_vectors() {
        let cases: [(&str, &[u8], &str); 5] = [
            (
                "0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b",
                b"Hi There",
                "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7",
            ),
            (
                "4a656665",
                b"what do ya want for nothing?",
                "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843",
            ),
            (
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                &[0xdd; 50],
                "773ea91e36800e46854db8ebd09181a72959098b3ef8c122d9635514ced565fe",
            ),
            (
                &"aa".repeat(131),
                b"Test Using Larger Than Block-Size Key - Hash Key First",
                "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54",
            ),
            (
                &"aa".repeat(131),
                b"This is a test using a larger than block-size key and a larger than block-size data. The key needs to be hashed before being used by the HMAC algorithm.",
                "9b09ffa71b942fcb27635fbcd5b0e944bfdc63644f0713938a7f51535c3a35e2",
            ),
        ];
        for (key, message, expected) in cases {
            let key = HmacSha256::new(&unhex(key));
            assert_eq!(hex(&key.digest(message)), expected);

            // And the same through a signer fed in pieces.
            let mut signer = key.sign();
            for piece in message.chunks(7) {
                signer.update(piece);
            }
            assert_eq!(hex(&signer.finish()), expected);
        }
    }

    /// A key's padded blocks are computed once and shared: signing twice with
    /// one opened key gives what opening it twice would.
    #[test]
    fn an_opened_key_signs_many_messages() {
        let key = HmacSha256::new(b"a key of some length, under a block");
        let first = key.digest(b"first");
        let second = key.digest(b"second");
        assert_ne!(first, second);
        assert_eq!(
            first,
            HmacSha256::new(b"a key of some length, under a block").digest(b"first")
        );
    }
}
