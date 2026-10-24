//! CRC-32C (Castagnoli), the checksum guarding every byte SpiroDB writes.
//!
//! Uses the CPU's CRC instructions when present (the CRC extension on aarch64,
//! SSE4.2 on x86_64) and falls back to a slice-by-8 table implementation.

const POLY: u32 = 0x82f6_3b78; // Castagnoli polynomial, bit-reversed

static TABLES: [[u32; 256]; 8] = build_tables();

const fn build_tables() -> [[u32; 256]; 8] {
    let mut tables = [[0u32; 256]; 8];
    let mut i = 0;
    while i < 256 {
        let mut crc = i as u32;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ POLY } else { crc >> 1 };
            bit += 1;
        }
        tables[0][i] = crc;
        i += 1;
    }
    let mut i = 0;
    while i < 256 {
        let mut t = 1;
        while t < 8 {
            let prev = tables[t - 1][i];
            tables[t][i] = (prev >> 8) ^ tables[0][(prev & 0xff) as usize];
            t += 1;
        }
        i += 1;
    }
    tables
}

pub fn crc32c(data: &[u8]) -> u32 {
    #[cfg(target_arch = "aarch64")]
    if std::arch::is_aarch64_feature_detected!("crc") {
        // SAFETY: the CPU supports the `crc` feature.
        return unsafe { hardware_aarch64(data) };
    }
    #[cfg(target_arch = "x86_64")]
    if is_x86_feature_detected!("sse4.2") {
        // SAFETY: the CPU supports the `sse4.2` feature.
        return unsafe { hardware_x86_64(data) };
    }
    software(data)
}

fn software(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    let (chunks, rest) = data.as_chunks::<8>();
    for c in chunks {
        let lo = u32::from_le_bytes([c[0], c[1], c[2], c[3]]) ^ crc;
        let hi = u32::from_le_bytes([c[4], c[5], c[6], c[7]]);
        crc = TABLES[7][(lo & 0xff) as usize]
            ^ TABLES[6][(lo >> 8 & 0xff) as usize]
            ^ TABLES[5][(lo >> 16 & 0xff) as usize]
            ^ TABLES[4][(lo >> 24) as usize]
            ^ TABLES[3][(hi & 0xff) as usize]
            ^ TABLES[2][(hi >> 8 & 0xff) as usize]
            ^ TABLES[1][(hi >> 16 & 0xff) as usize]
            ^ TABLES[0][(hi >> 24) as usize];
    }
    for &b in rest {
        crc = (crc >> 8) ^ TABLES[0][((crc ^ u32::from(b)) & 0xff) as usize];
    }
    !crc
}

#[cfg(target_arch = "aarch64")]
#[target_feature(enable = "crc")]
#[allow(unused_unsafe)]
fn hardware_aarch64(data: &[u8]) -> u32 {
    use std::arch::aarch64::{__crc32cb, __crc32cd};
    let mut crc = !0u32;
    let (chunks, rest) = data.as_chunks::<8>();
    for &c in chunks {
        crc = unsafe { __crc32cd(crc, u64::from_le_bytes(c)) };
    }
    for &b in rest {
        crc = unsafe { __crc32cb(crc, b) };
    }
    !crc
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse4.2")]
#[allow(unused_unsafe)]
fn hardware_x86_64(data: &[u8]) -> u32 {
    use std::arch::x86_64::{_mm_crc32_u8, _mm_crc32_u64};
    let mut crc = u64::from(!0u32);
    let (chunks, rest) = data.as_chunks::<8>();
    for &c in chunks {
        crc = unsafe { _mm_crc32_u64(crc, u64::from_le_bytes(c)) };
    }
    let mut crc = crc as u32;
    for &b in rest {
        crc = unsafe { _mm_crc32_u8(crc, b) };
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_vectors() {
        assert_eq!(crc32c(b""), 0);
        assert_eq!(crc32c(b"123456789"), 0xe306_9283);
        assert_eq!(software(b"123456789"), 0xe306_9283);
        assert_eq!(crc32c(&[0u8; 32]), 0x8a91_36aa);
    }

    #[test]
    fn hardware_matches_software() {
        let data: Vec<u8> = (0..1000u32).map(|i| (i * 31 % 251) as u8).collect();
        for len in 0..data.len() {
            assert_eq!(crc32c(&data[..len]), software(&data[..len]), "length {len}");
        }
    }
}
