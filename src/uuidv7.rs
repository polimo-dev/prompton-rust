//! RFC 9562 UUIDv7 generation, with no external dependencies.
//!
//! A monitoring-log `id` is an idempotency key the SDK issues *before* the provider call, so it
//! has to be time-ordered: 48 bits of unix milliseconds, the version nibble `7`, 12 random bits,
//! the variant bits `10`, then 62 more random bits. The server's column is a UUIDv7 type — a v4
//! id is accepted by request validation and then fails on write — so never substitute a v4.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// A new UUIDv7 as lowercase hex with dashes.
pub fn generate() -> String {
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    from_parts(ms, next_u64(), next_u64())
}

/// Builds a UUIDv7 from an explicit timestamp and two random words (used by the tests).
pub fn from_parts(unix_ms: u64, rand_a: u64, rand_b: u64) -> String {
    let mut bytes = [0u8; 16];
    let ms = unix_ms & 0x0000_ffff_ffff_ffff;
    bytes[0..6].copy_from_slice(&ms.to_be_bytes()[2..8]);

    let a = (rand_a & 0x0fff) as u16;
    bytes[6] = 0x70 | ((a >> 8) as u8 & 0x0f);
    bytes[7] = (a & 0xff) as u8;

    let b = rand_b.to_be_bytes();
    bytes[8] = 0x80 | (b[0] & 0x3f);
    bytes[9..16].copy_from_slice(&b[1..8]);

    format_hyphenated(&bytes)
}

/// The unix milliseconds encoded in a UUIDv7 string, or `None` when it is not one.
pub fn timestamp_ms(uuid: &str) -> Option<u64> {
    let hex: String = uuid.chars().filter(|c| *c != '-').collect();
    if hex.len() != 32 {
        return None;
    }
    u64::from_str_radix(&hex[0..12], 16).ok()
}

/// Whether the string is a UUID with version nibble 7 and the RFC 9562 variant bits.
pub fn is_uuid_v7(uuid: &str) -> bool {
    let bytes = uuid.as_bytes();
    if bytes.len() != 36 {
        return false;
    }
    let dashes = [8, 13, 18, 23];
    for (i, byte) in bytes.iter().enumerate() {
        let expect_dash = dashes.contains(&i);
        if expect_dash && *byte != b'-' {
            return false;
        }
        if !expect_dash && !byte.is_ascii_hexdigit() {
            return false;
        }
    }
    let version = uuid.as_bytes()[14];
    let variant = uuid.as_bytes()[19].to_ascii_lowercase();
    version == b'7' && matches!(variant, b'8' | b'9' | b'a' | b'b')
}

fn format_hyphenated(bytes: &[u8; 16]) -> String {
    let mut s = String::with_capacity(36);
    for (i, byte) in bytes.iter().enumerate() {
        if matches!(i, 4 | 6 | 8 | 10) {
            s.push('-');
        }
        s.push(char::from_digit((byte >> 4) as u32, 16).unwrap_or('0'));
        s.push(char::from_digit((byte & 0x0f) as u32, 16).unwrap_or('0'));
    }
    s
}

// A SplitMix64 stream seeded once from the OS (or from the clock and an address when the OS
// source is unavailable). Ids only have to be unique, not unpredictable.
//
// The step has to be a single atomic read-modify-write: a `load` followed by a `store` lets two
// threads read the same word and hand back the same id, and an id is the idempotency key of a
// monitoring log, so a collision makes the server absorb the record as a duplicate and the log
// disappears with no error anywhere.
static STATE: AtomicU64 = AtomicU64::new(0);

/// The SplitMix64 increment (the golden-ratio odd constant).
const GOLDEN: u64 = 0x9e37_79b9_7f4a_7c15;

fn next_u64() -> u64 {
    if STATE.load(Ordering::Relaxed) == 0 {
        // Zero is the "not seeded yet" sentinel, so force the seed non-zero: the stream must
        // never be able to land back on it. Whoever loses the race just keeps the winner's seed.
        let _ = STATE.compare_exchange(0, seed() | 1, Ordering::Relaxed, Ordering::Relaxed);
    }
    let next = STATE
        .fetch_add(GOLDEN, Ordering::Relaxed)
        .wrapping_add(GOLDEN);
    let mut z = next;
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

fn seed() -> u64 {
    if let Some(word) = os_random() {
        if word != 0 {
            return word;
        }
    }
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(1);
    let stack = &nanos as *const u64 as u64;
    nanos ^ stack.rotate_left(17) ^ 0x2545_f491_4f6c_dd1d
}

#[cfg(unix)]
fn os_random() -> Option<u64> {
    use std::io::Read;
    let mut file = std::fs::File::open("/dev/urandom").ok()?;
    let mut buf = [0u8; 8];
    file.read_exact(&mut buf).ok()?;
    Some(u64::from_le_bytes(buf))
}

#[cfg(not(unix))]
fn os_random() -> Option<u64> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn generates_well_formed_v7_ids() {
        let id = generate();
        assert_eq!(id.len(), 36);
        assert!(is_uuid_v7(&id), "{id} is not a UUIDv7");
    }

    #[test]
    fn encodes_the_timestamp() {
        let id = from_parts(0x0198_f2a1_0000, 0x123, 0x456);
        assert_eq!(timestamp_ms(&id), Some(0x0198_f2a1_0000));
        assert!(id.starts_with("0198f2a1-0000-7"), "{id}");
    }

    #[test]
    fn ids_are_unique_and_time_ordered() {
        let mut seen = HashSet::new();
        for _ in 0..10_000 {
            assert!(seen.insert(generate()));
        }
        let early = from_parts(1_000, 1, 2);
        let late = from_parts(2_000, 1, 2);
        assert!(early < late);
    }

    #[test]
    fn ids_are_unique_across_threads() {
        // Eight threads generating inside the same millisecond is exactly the case a non-atomic
        // read-modify-write on the RNG state gets wrong.
        let threads: Vec<_> = (0..8)
            .map(|_| {
                std::thread::spawn(|| (0..20_000).map(|_| generate()).collect::<Vec<String>>())
            })
            .collect();
        let mut seen = HashSet::new();
        let mut total = 0usize;
        for thread in threads {
            for id in thread.join().expect("the generator never panics") {
                total += 1;
                assert!(seen.insert(id.clone()), "{id} was generated twice");
            }
        }
        assert_eq!(seen.len(), total);
    }

    #[test]
    fn rejects_a_v4_id() {
        assert!(!is_uuid_v7("d2b0f1e4-6f5d-4a1e-9f3a-0b0c0d0e0f10"));
        assert!(!is_uuid_v7("not-a-uuid"));
    }
}
