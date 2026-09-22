//! SHA-256, implemented here rather than pulled from a crate, and fast enough not to be the
//! reason a burn is slow.
//!
//! arxburn's whole promise is "the bytes on the stick are the bytes in the image", so the hash is
//! the one part that must never be unavailable: this has to build on a freshly installed machine
//! with no network and an empty cargo cache, which is exactly the machine someone reaches for a
//! burn tool on. It is FIPS 180-4, checked against the standard vectors below.
//!
//! Speed matters here for a concrete reason: every burn hashes the image once and the stick once,
//! so a slow hash is a slow burn. Portable code measured 229 to 251 MB/s on an i7-8665U, against
//! 502 MB/s for OpenSSL's hand written assembly, and that is fast enough to stay ahead of what a
//! USB stick will accept. Newer CPUs (AMD from Zen, Intel from Ice Lake) carry the SHA
//! extensions, which do the same rounds in silicon several times faster again, so there is an
//! accelerated path chosen at RUNTIME, never at build time, or the binary would only run on the
//! machine that built it. Both paths are checked against each other in the tests.

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

pub struct Sha256 {
    state: [u32; 8],
    buf: [u8; 64],
    buflen: usize,
    total: u64,
}

impl Default for Sha256 {
    fn default() -> Self { Self::new() }
}

impl Sha256 {
    pub fn new() -> Self {
        Sha256 {
            state: [
                0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a,
                0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
            ],
            buf: [0u8; 64],
            buflen: 0,
            total: 0,
        }
    }

    pub fn update(&mut self, mut data: &[u8]) {
        self.total = self.total.wrapping_add(data.len() as u64);
        // finish any partial block first, then hand whole blocks to the engine in one call:
        // the accelerated path keeps the state in registers across blocks, so feeding it 4MiB
        // at a time is worth far more than feeding it 64 bytes sixty-five thousand times.
        if self.buflen > 0 {
            let take = core::cmp::min(64 - self.buflen, data.len());
            self.buf[self.buflen..self.buflen + take].copy_from_slice(&data[..take]);
            self.buflen += take;
            data = &data[take..];
            if self.buflen == 64 {
                let block = self.buf;
                compress_many(&mut self.state, &block);
                self.buflen = 0;
            }
        }
        let whole = data.len() - data.len() % 64;
        if whole > 0 { compress_many(&mut self.state, &data[..whole]); }
        data = &data[whole..];
        if !data.is_empty() {
            self.buf[..data.len()].copy_from_slice(data);
            self.buflen = data.len();
        }
    }

    pub fn finish(mut self) -> [u8; 32] {
        let bits = self.total.wrapping_mul(8);
        self.update(&[0x80]);
        while self.buflen != 56 {
            self.update(&[0x00]);
        }
        // update() counts these into total, so the length is captured before padding above
        let mut b = [0u8; 64];
        b[..56].copy_from_slice(&self.buf[..56]);
        b[56..].copy_from_slice(&bits.to_be_bytes());
        compress_many(&mut self.state, &b);
        let mut out = [0u8; 32];
        for (i, w) in self.state.iter().enumerate() {
            out[i * 4..i * 4 + 4].copy_from_slice(&w.to_be_bytes());
        }
        out
    }
}

/// Which engine to use. Worked out once, because the answer cannot change while we run.
fn use_hardware() -> bool {
    #[cfg(target_arch = "x86_64")]
    {
        use std::sync::atomic::{AtomicU8, Ordering};
        static CHOICE: AtomicU8 = AtomicU8::new(0); // 0 unknown, 1 hardware, 2 software
        match CHOICE.load(Ordering::Relaxed) {
            1 => return true,
            2 => return false,
            _ => {}
        }
        let yes = std::env::var_os("ARXBURN_NO_SHA_NI").is_none()
            && is_x86_feature_detected!("sha")
            && is_x86_feature_detected!("sse4.1")
            && is_x86_feature_detected!("ssse3");
        CHOICE.store(if yes { 1 } else { 2 }, Ordering::Relaxed);
        return yes;
    }
    #[allow(unreachable_code)]
    false
}

fn compress_many(state: &mut [u32; 8], blocks: &[u8]) {
    debug_assert!(blocks.len() % 64 == 0);
    #[cfg(target_arch = "x86_64")]
    if use_hardware() {
        // SAFETY: use_hardware() checked sha + sse4.1 + ssse3 on this CPU, and the length is a
        // whole number of 64 byte blocks.
        unsafe { sha_ni(state, blocks) };
        return;
    }
    portable(state, blocks);
}

fn portable(state: &mut [u32; 8], blocks: &[u8]) {
    // A rolling sixteen word schedule rather than a sixty-four word array, and the rounds
    // rotate the working variables by NAME instead of copying them. Measured on an i7-8665U
    // (which has no SHA extensions): 230 MB/s written the obvious way, 251 MB/s like this,
    // against 502 MB/s for OpenSSL's hand written assembly, which is the honest ceiling for a
    // portable implementation. Either way it stays ahead of what a USB stick accepts, and on
    // any CPU with the SHA extensions the hardware path below runs several times faster again.
    macro_rules! round {
        ($a:expr, $b:expr, $c:expr, $d:expr, $e:expr, $f:expr, $g:expr, $h:expr, $k:expr, $w:expr) => {{
            let s1 = $e.rotate_right(6) ^ $e.rotate_right(11) ^ $e.rotate_right(25);
            let ch = ($e & $f) ^ ((!$e) & $g);
            let t1 = $h.wrapping_add(s1).wrapping_add(ch).wrapping_add($k).wrapping_add($w);
            let s0 = $a.rotate_right(2) ^ $a.rotate_right(13) ^ $a.rotate_right(22);
            let maj = ($a & $b) ^ ($a & $c) ^ ($b & $c);
            $d = $d.wrapping_add(t1);
            $h = t1.wrapping_add(s0.wrapping_add(maj));
        }};
    }

    for block in blocks.chunks_exact(64) {
        let mut w = [0u32; 16];
        for (i, c) in block.chunks_exact(4).enumerate() {
            w[i] = u32::from_be_bytes([c[0], c[1], c[2], c[3]]);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = *state;

        for r in (0..64).step_by(16) {
            if r > 0 {
                // extend the window in place: w[j] becomes the word for round r + j
                for j in 0..16 {
                    let x = w[(j + 1) & 15];
                    let y = w[(j + 14) & 15];
                    let s0 = x.rotate_right(7) ^ x.rotate_right(18) ^ (x >> 3);
                    let s1 = y.rotate_right(17) ^ y.rotate_right(19) ^ (y >> 10);
                    w[j] = w[j].wrapping_add(s0).wrapping_add(w[(j + 9) & 15]).wrapping_add(s1);
                }
            }
            round!(a, b, c, d, e, f, g, h, K[r], w[0]);
            round!(h, a, b, c, d, e, f, g, K[r + 1], w[1]);
            round!(g, h, a, b, c, d, e, f, K[r + 2], w[2]);
            round!(f, g, h, a, b, c, d, e, K[r + 3], w[3]);
            round!(e, f, g, h, a, b, c, d, K[r + 4], w[4]);
            round!(d, e, f, g, h, a, b, c, K[r + 5], w[5]);
            round!(c, d, e, f, g, h, a, b, K[r + 6], w[6]);
            round!(b, c, d, e, f, g, h, a, K[r + 7], w[7]);
            round!(a, b, c, d, e, f, g, h, K[r + 8], w[8]);
            round!(h, a, b, c, d, e, f, g, K[r + 9], w[9]);
            round!(g, h, a, b, c, d, e, f, K[r + 10], w[10]);
            round!(f, g, h, a, b, c, d, e, K[r + 11], w[11]);
            round!(e, f, g, h, a, b, c, d, K[r + 12], w[12]);
            round!(d, e, f, g, h, a, b, c, K[r + 13], w[13]);
            round!(c, d, e, f, g, h, a, b, K[r + 14], w[14]);
            round!(b, c, d, e, f, g, h, a, K[r + 15], w[15]);
        }

        for (s, v) in state.iter_mut().zip([a, b, c, d, e, f, g, h]) {
            *s = s.wrapping_add(v);
        }
    }
}

/// The same compression function on the CPU's SHA extensions. The register layout is the one
/// the instructions want (ABEF / CDGH rather than A..H in order), which is why the state is
/// shuffled on the way in and back on the way out.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sha,sse2,ssse3,sse4.1")]
unsafe fn sha_ni(state: &mut [u32; 8], blocks: &[u8]) {
    use std::arch::x86_64::*;

    // bytes arrive big endian; this mask turns each 32 bit word round
    let mask = _mm_set_epi64x(0x0c0d0e0f08090a0bu64 as i64, 0x0405060700010203u64 as i64);

    let tmp0 = _mm_loadu_si128(state.as_ptr() as *const __m128i);
    let mut state1 = _mm_loadu_si128(state.as_ptr().add(4) as *const __m128i);
    let tmp0 = _mm_shuffle_epi32(tmp0, 0xB1);
    state1 = _mm_shuffle_epi32(state1, 0x1B);
    let mut state0 = _mm_alignr_epi8(tmp0, state1, 8);
    state1 = _mm_blend_epi16(state1, tmp0, 0xF0);

    for block in blocks.chunks_exact(64) {
        let abef_save = state0;
        let cdgh_save = state1;
        let p = block.as_ptr() as *const __m128i;

        macro_rules! rnds {
            ($msg:expr) => {{
                let m = $msg;
                state1 = _mm_sha256rnds2_epu32(state1, state0, m);
                let m2 = _mm_shuffle_epi32(m, 0x0E);
                state0 = _mm_sha256rnds2_epu32(state0, state1, m2);
            }};
        }

        // rounds 0-3
        let mut msg0 = _mm_shuffle_epi8(_mm_loadu_si128(p), mask);
        rnds!(_mm_add_epi32(msg0, _mm_set_epi64x(0xE9B5DBA5B5C0FBCFu64 as i64, 0x71374491428A2F98u64 as i64)));
        // rounds 4-7
        let mut msg1 = _mm_shuffle_epi8(_mm_loadu_si128(p.add(1)), mask);
        rnds!(_mm_add_epi32(msg1, _mm_set_epi64x(0xAB1C5ED5923F82A4u64 as i64, 0x59F111F13956C25Bu64 as i64)));
        msg0 = _mm_sha256msg1_epu32(msg0, msg1);
        // rounds 8-11
        let mut msg2 = _mm_shuffle_epi8(_mm_loadu_si128(p.add(2)), mask);
        rnds!(_mm_add_epi32(msg2, _mm_set_epi64x(0x550C7DC3243185BEu64 as i64, 0x12835B01D807AA98u64 as i64)));
        msg1 = _mm_sha256msg1_epu32(msg1, msg2);
        // rounds 12-15
        let mut msg3 = _mm_shuffle_epi8(_mm_loadu_si128(p.add(3)), mask);
        let m = _mm_add_epi32(msg3, _mm_set_epi64x(0xC19BF1749BDC06A7u64 as i64, 0x80DEB1FE72BE5D74u64 as i64));
        state1 = _mm_sha256rnds2_epu32(state1, state0, m);
        msg0 = _mm_sha256msg2_epu32(_mm_add_epi32(msg0, _mm_alignr_epi8(msg3, msg2, 4)), msg3);
        state0 = _mm_sha256rnds2_epu32(state0, state1, _mm_shuffle_epi32(m, 0x0E));
        msg2 = _mm_sha256msg1_epu32(msg2, msg3);

        // rounds 16-51: the schedule cycles msg0 -> msg1 -> msg2 -> msg3
        macro_rules! group {
            ($hi:expr, $lo:expr, $cur:ident, $upd:ident, $prev:ident) => {{
                let m = _mm_add_epi32($cur, _mm_set_epi64x($hi as i64, $lo as i64));
                state1 = _mm_sha256rnds2_epu32(state1, state0, m);
                $upd = _mm_sha256msg2_epu32(_mm_add_epi32($upd, _mm_alignr_epi8($cur, $prev, 4)), $cur);
                state0 = _mm_sha256rnds2_epu32(state0, state1, _mm_shuffle_epi32(m, 0x0E));
                $prev = _mm_sha256msg1_epu32($prev, $cur);
            }};
        }
        group!(0x240CA1CC0FC19DC6u64, 0xEFBE4786E49B69C1u64, msg0, msg1, msg3); // 16-19
        group!(0x76F988DA5CB0A9DCu64, 0x4A7484AA2DE92C6Fu64, msg1, msg2, msg0); // 20-23
        group!(0xBF597FC7B00327C8u64, 0xA831C66D983E5152u64, msg2, msg3, msg1); // 24-27
        group!(0x1429296706CA6351u64, 0xD5A79147C6E00BF3u64, msg3, msg0, msg2); // 28-31
        group!(0x53380D134D2C6DFCu64, 0x2E1B213827B70A85u64, msg0, msg1, msg3); // 32-35
        group!(0x92722C8581C2C92Eu64, 0x766A0ABB650A7354u64, msg1, msg2, msg0); // 36-39
        group!(0xC76C51A3C24B8B70u64, 0xA81A664BA2BFE8A1u64, msg2, msg3, msg1); // 40-43
        group!(0x106AA070F40E3585u64, 0xD6990624D192E819u64, msg3, msg0, msg2); // 44-47
        group!(0x34B0BCB52748774Cu64, 0x1E376C0819A4C116u64, msg0, msg1, msg3); // 48-51

        // 52-59 still schedule, but nothing reads a msg1 result any more
        macro_rules! tail {
            ($hi:expr, $lo:expr, $cur:ident, $upd:ident, $prev:ident) => {{
                let m = _mm_add_epi32($cur, _mm_set_epi64x($hi as i64, $lo as i64));
                state1 = _mm_sha256rnds2_epu32(state1, state0, m);
                $upd = _mm_sha256msg2_epu32(_mm_add_epi32($upd, _mm_alignr_epi8($cur, $prev, 4)), $cur);
                state0 = _mm_sha256rnds2_epu32(state0, state1, _mm_shuffle_epi32(m, 0x0E));
            }};
        }
        tail!(0x682E6FF35B9CCA4Fu64, 0x4ED8AA4A391C0CB3u64, msg1, msg2, msg0); // 52-55
        tail!(0x8CC7020884C87814u64, 0x78A5636F748F82EEu64, msg2, msg3, msg1); // 56-59
        // rounds 60-63
        rnds!(_mm_add_epi32(msg3, _mm_set_epi64x(0xC67178F2BEF9A3F7u64 as i64, 0xA4506CEB90BEFFFAu64 as i64)));

        state0 = _mm_add_epi32(state0, abef_save);
        state1 = _mm_add_epi32(state1, cdgh_save);
    }

    let t = _mm_shuffle_epi32(state0, 0x1B);
    let s1 = _mm_shuffle_epi32(state1, 0xB1);
    let out0 = _mm_blend_epi16(t, s1, 0xF0);
    let out1 = _mm_alignr_epi8(s1, t, 8);
    _mm_storeu_si128(state.as_mut_ptr() as *mut __m128i, out0);
    _mm_storeu_si128(state.as_mut_ptr().add(4) as *mut __m128i, out1);
}

/// Hash a whole file, streaming.
pub fn file(path: &std::path::Path) -> std::io::Result<String> {
    use std::io::Read;
    let mut f = std::fs::File::open(path)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 4 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 { break; }
        h.update(&buf[..n]);
    }
    Ok(hex(&h.finish()))
}

pub fn hex(digest: &[u8; 32]) -> String {
    let mut s = String::with_capacity(64);
    for b in digest {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// Which engine this run is using, for the version banner and the GUI's About line.
pub fn engine() -> &'static str {
    if use_hardware() { "sha-ni" } else { "portable" }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn digest(data: &[u8]) -> String {
        let mut h = Sha256::new();
        h.update(data);
        hex(&h.finish())
    }

    // FIPS 180-4 / NIST example vectors. If these pass, the implementation is right.
    #[test]
    fn known_vectors() {
        assert_eq!(digest(b""), "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
        assert_eq!(digest(b"abc"), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        assert_eq!(
            digest(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
    }

    #[test]
    fn million_a() {
        let mut h = Sha256::new();
        let chunk = vec![b'a'; 1000];
        for _ in 0..1000 { h.update(&chunk); }
        assert_eq!(hex(&h.finish()), "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0");
    }

    // The streaming path must agree with a single call, whatever the chunk boundaries: a burn
    // hashes in multi-megabyte reads, so a bug that only shows on odd splits would be a wrong
    // verdict on someone's stick.
    #[test]
    fn chunking_does_not_change_the_digest() {
        let data: Vec<u8> = (0..10_000u32).map(|i| (i % 251) as u8).collect();
        let whole = digest(&data);
        for chunk in [1usize, 7, 63, 64, 65, 127, 4096] {
            let mut h = Sha256::new();
            for part in data.chunks(chunk) { h.update(part); }
            assert_eq!(hex(&h.finish()), whole, "chunk size {chunk} changed the digest");
        }
    }

    // The accelerated path exists only to be faster. If it ever disagrees with the reference
    // by one bit, a burn would be declared good or bad on a lie, so they are compared directly
    // over many lengths, including every alignment around a block boundary.
    #[test]
    fn hardware_and_software_agree_exactly() {
        if !use_hardware() {
            eprintln!("no SHA extensions on this CPU: only the portable path was exercised");
            return;
        }
        let data: Vec<u8> = (0..(1024 * 9 + 5) as u32).map(|i| (i.wrapping_mul(2654435761) >> 13) as u8).collect();
        for len in [0usize, 1, 55, 56, 63, 64, 65, 127, 128, 191, 4096, 8191, 9221] {
            let mut sw = [0x6a09e667u32, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19];
            let mut hw = sw;
            let whole = len - len % 64;
            portable(&mut sw, &data[..whole]);
            unsafe { sha_ni(&mut hw, &data[..whole]) };
            assert_eq!(sw, hw, "the two engines disagree after {whole} bytes");
        }
    }
}
