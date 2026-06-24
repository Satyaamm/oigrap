// Pure-Rust MD5 implementation (RFC 1321). No external dependencies.

// Per-round shift amounts
#[allow(dead_code)]
const S: [u32; 64] = [
    7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22,
    5,  9, 14, 20, 5,  9, 14, 20, 5,  9, 14, 20, 5,  9, 14, 20,
    4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23,
    6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
];

// Pre-computed table K[i] = floor(2^32 * |sin(i+1)|)
#[allow(dead_code)]
const K: [u32; 64] = [
    0xd76aa478, 0xe8c7b756, 0x242070db, 0xc1bdceee,
    0xf57c0faf, 0x4787c62a, 0xa8304613, 0xfd469501,
    0x698098d8, 0x8b44f7af, 0xffff5bb1, 0x895cd7be,
    0x6b901122, 0xfd987193, 0xa679438e, 0x49b40821,
    0xf61e2562, 0xc040b340, 0x265e5a51, 0xe9b6c7aa,
    0xd62f105d, 0x02441453, 0xd8a1e681, 0xe7d3fbc8,
    0x21e1cde6, 0xc33707d6, 0xf4d50d87, 0x455a14ed,
    0xa9e3e905, 0xfcefa3f8, 0x676f02d9, 0x8d2a4c8a,
    0xfffa3942, 0x8771f681, 0x6d9d6122, 0xfde5380c,
    0xa4beea44, 0x4bdecfa9, 0xf6bb4b60, 0xbebfbc70,
    0x289b7ec6, 0xeaa127fa, 0xd4ef3085, 0x04881d05,
    0xd9d4d039, 0xe6db99e5, 0x1fa27cf8, 0xc4ac5665,
    0xf4292244, 0x432aff97, 0xab9423a7, 0xfc93a039,
    0x655b59c3, 0x8f0ccc92, 0xffeff47d, 0x85845dd1,
    0x6fa87e4f, 0xfe2ce6e0, 0xa3014314, 0x4e0811a1,
    0xf7537e82, 0xbd3af235, 0x2ad7d2bb, 0xeb86d391,
];

#[allow(dead_code)]
/// Compute the MD5 digest of `data` and return it as a lowercase hex string.
pub fn md5_hex(data: &[u8]) -> String {
    let digest = md5_digest(data);
    let mut hex = String::with_capacity(32);
    for byte in digest {
        hex.push_str(&format!("{:02x}", byte));
    }
    hex
}

#[allow(dead_code)]
/// Compute the raw 16-byte MD5 digest.
pub fn md5_digest(data: &[u8]) -> [u8; 16] {
    // Initial hash state
    let mut a0: u32 = 0x67452301;
    let mut b0: u32 = 0xefcdab89;
    let mut c0: u32 = 0x98badcfe;
    let mut d0: u32 = 0x10325476;

    // Pre-processing: pad the message
    let msg = pad(data);

    // Process each 512-bit (64-byte) chunk
    for chunk in msg.chunks(64) {
        // Break chunk into sixteen 32-bit little-endian words
        let mut m = [0u32; 16];
        for (i, word) in m.iter_mut().enumerate() {
            let off = i * 4;
            *word = u32::from_le_bytes(chunk[off..off+4].try_into().unwrap());
        }

        let mut a = a0;
        let mut b = b0;
        let mut c = c0;
        let mut d = d0;

        for i in 0..64usize {
            let (f, g) = if i < 16 {
                ((b & c) | (!b & d), i)
            } else if i < 32 {
                ((d & b) | (!d & c), (5 * i + 1) % 16)
            } else if i < 48 {
                (b ^ c ^ d, (3 * i + 5) % 16)
            } else {
                (c ^ (b | !d), (7 * i) % 16)
            };

            let temp = d;
            d = c;
            c = b;
            b = b.wrapping_add(
                a.wrapping_add(f)
                 .wrapping_add(K[i])
                 .wrapping_add(m[g])
                 .rotate_left(S[i])
            );
            a = temp;
        }

        a0 = a0.wrapping_add(a);
        b0 = b0.wrapping_add(b);
        c0 = c0.wrapping_add(c);
        d0 = d0.wrapping_add(d);
    }

    // Produce final digest in little-endian byte order
    let mut digest = [0u8; 16];
    digest[0..4].copy_from_slice(&a0.to_le_bytes());
    digest[4..8].copy_from_slice(&b0.to_le_bytes());
    digest[8..12].copy_from_slice(&c0.to_le_bytes());
    digest[12..16].copy_from_slice(&d0.to_le_bytes());
    digest
}

/// Pad the input according to RFC 1321:
/// append 0x80, then zero bytes, then 8-byte little-endian bit length,
/// so total length is a multiple of 64 bytes.
fn pad(data: &[u8]) -> Vec<u8> {
    let original_len = data.len();
    let bit_len = (original_len as u64).wrapping_mul(8);

    let mut msg = data.to_vec();
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0x00);
    }
    msg.extend_from_slice(&bit_len.to_le_bytes());
    msg
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_md5_implementation() {
        // RFC 1321 test vector: MD5("") = d41d8cd98f00b204e9800998ecf8427e
        assert_eq!(md5_hex(b""), "d41d8cd98f00b204e9800998ecf8427e");
    }

    #[test]
    fn test_md5_hello() {
        // MD5("hello") = 5d41402abc4b2a76b9719d911017c592
        assert_eq!(md5_hex(b"hello"), "5d41402abc4b2a76b9719d911017c592");
    }

    #[test]
    fn test_md5_abc() {
        // RFC 1321 test vector: MD5("abc") = 900150983cd24fb0d6963f7d28e17f72
        assert_eq!(md5_hex(b"abc"), "900150983cd24fb0d6963f7d28e17f72");
    }

    #[test]
    fn test_md5_long_string() {
        // RFC 1321 test vector: MD5("abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq")
        assert_eq!(
            md5_hex(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            "8215ef0796a20bcaaae116d3876c664a"
        );
    }
}
