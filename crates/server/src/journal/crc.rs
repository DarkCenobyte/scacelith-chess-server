//! CRC-32C (Castagnoli), the checksum of the journal records: reflected polynomial `0x82F63B78`,
//! initial value and final XOR all ones (the iSCSI / ext4 variant, `crc32c(b"123456789")` is
//! `0xE3069283`). Slicing-by-8 over tables computed at compile time.

/// The reflected Castagnoli polynomial.
const POLY: u32 = 0x82F6_3B78;

/// `TABLE[0]` is the classic byte-at-a-time table; `TABLE[s][n]` is the CRC of byte `n` followed
/// by `s` zero bytes, which lets the main loop fold 8 bytes per step.
static TABLE: [[u32; 256]; 8] = build_table();

const fn build_table() -> [[u32; 256]; 8] {
    let mut t = [[0u32; 256]; 8];
    let mut n = 0;
    while n < 256 {
        let mut c = n as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 { (c >> 1) ^ POLY } else { c >> 1 };
            k += 1;
        }
        t[0][n] = c;
        n += 1;
    }
    let mut n = 0;
    while n < 256 {
        let mut c = t[0][n];
        let mut s = 1;
        while s < 8 {
            c = t[0][(c & 0xff) as usize] ^ (c >> 8);
            t[s][n] = c;
            s += 1;
        }
        n += 1;
    }
    t
}

/// CRC-32C of `data`.
pub fn crc32c(data: &[u8]) -> u32 {
    crc32c_update(0, data)
}

/// Continues a CRC-32C: `crc32c_update(crc32c(a), b) == crc32c(a ++ b)`.
pub fn crc32c_update(crc: u32, data: &[u8]) -> u32 {
    let t = &TABLE;
    let mut c = !crc;
    let (chunks, rest) = data.as_chunks::<8>();
    for b in chunks {
        let x = c ^ u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
        c = t[7][(x & 0xff) as usize]
            ^ t[6][((x >> 8) & 0xff) as usize]
            ^ t[5][((x >> 16) & 0xff) as usize]
            ^ t[4][(x >> 24) as usize]
            ^ t[3][b[4] as usize]
            ^ t[2][b[5] as usize]
            ^ t[1][b[6] as usize]
            ^ t[0][b[7] as usize];
    }
    for &b in rest {
        c = t[0][((c ^ u32::from(b)) & 0xff) as usize] ^ (c >> 8);
    }
    !c
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Bit-at-a-time reference.
    fn reference(data: &[u8]) -> u32 {
        let mut c = !0u32;
        for &b in data {
            c ^= u32::from(b);
            for _ in 0..8 {
                c = if c & 1 != 0 { (c >> 1) ^ POLY } else { c >> 1 };
            }
        }
        !c
    }

    /// Deterministic pseudo-random bytes (xorshift64*).
    fn bytes(n: usize, seed: u64) -> Vec<u8> {
        let mut s = seed | 1;
        (0..n)
            .map(|_| {
                s ^= s >> 12;
                s ^= s << 25;
                s ^= s >> 27;
                (s.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 56) as u8
            })
            .collect()
    }

    #[test]
    fn standard_check_values() {
        assert_eq!(crc32c(b"123456789"), 0xE306_9283);
        assert_eq!(crc32c(&[]), 0);
        // RFC 3720 B.4.
        assert_eq!(crc32c(&[0u8; 32]), 0x8A91_36AA);
        assert_eq!(crc32c(&[0xffu8; 32]), 0x62A8_AB43);
        let inc: Vec<u8> = (0..32).collect();
        assert_eq!(crc32c(&inc), 0x46DD_794E);
        let dec: Vec<u8> = (0..32).rev().collect();
        assert_eq!(crc32c(&dec), 0x113F_DB5C);
    }

    #[test]
    fn agrees_with_the_bitwise_reference_and_continues() {
        for (i, n) in [1usize, 7, 8, 9, 15, 16, 17, 63, 64, 65, 1000, 4097].into_iter().enumerate() {
            let b = bytes(n, 0x9E37_79B9_7F4A_7C15 ^ i as u64);
            assert_eq!(crc32c(&b), reference(&b), "length {n}");
            for cut in [0, 1, n / 2, n.saturating_sub(1), n] {
                assert_eq!(
                    crc32c_update(crc32c(&b[..cut]), &b[cut..]),
                    crc32c(&b),
                    "continuation at {cut}/{n}"
                );
            }
            let mut shifted = b"xx".to_vec();
            shifted.extend_from_slice(&b);
            assert_eq!(crc32c(&shifted[2..]), crc32c(&b), "unaligned start");
        }
    }
}
