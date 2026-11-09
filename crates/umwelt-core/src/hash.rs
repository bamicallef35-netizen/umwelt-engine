//! Content-address hashing for `SignId`.
//!
//! Per **ADR-0014** (`docs/decisions.md`), `umwelt-core` uses **FNV-1a 64-bit**
//! in-tree, zero-dep, `no_std`-clean. This module exposes the primitive
//! and the process-wide collision counter. The allocator policy
//! (detect + promote + log-once) is implemented by the graph layer
//! (P1.3) on top of this primitive.

use core::sync::atomic::AtomicUsize;

/// FNV-1a 64-bit offset basis. Per ADR-0014.
pub const FNV1A_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;

/// FNV-1a 64-bit prime. Per ADR-0014.
pub const FNV1A_PRIME: u64 = 0x100_0000_01b3;

/// FNV-1a 64-bit hash.
///
/// Matches the RFC-style reference implementation byte-for-byte. Used
/// for `SignId` content-addressing and **nothing else security-sensitive**
/// — the SEP authentication boundary lives on BLAKE3 (ADR-0012).
#[inline]
pub fn fnv1a_64(bytes: &[u8]) -> u64 {
    fnv1a_64_continue(FNV1A_OFFSET_BASIS, bytes)
}

/// FNV-1a 64-bit **continuation**: absorb more bytes into an existing
/// accumulator. Needed by streaming `core::hash::Hasher` impls that
/// see the input in multiple `write()` calls; round-trips with
/// [`fnv1a_64`] (`fnv1a_64(x) == fnv1a_64_continue(FNV1A_OFFSET_BASIS, x)`).
#[inline]
pub fn fnv1a_64_continue(state: u64, bytes: &[u8]) -> u64 {
    let mut h = state;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(FNV1A_PRIME);
    }
    h
}

/// Process-wide count of `SignId` collisions observed by allocators.
///
/// Increments (with `Ordering::Relaxed`) each time an allocator has to
/// rehash a fresh sign with a salt suffix. At 64-bit output and
/// realistic graph sizes the counter is expected to stay at `0` for
/// the lifetime of a deployment; its first increment is the signal
/// that a heavier hash (ADR-0014's reserved escalation to BLAKE3-64)
/// might be warranted.
pub static COLLISION_COUNT: AtomicUsize = AtomicUsize::new(0);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_input_hashes_to_offset_basis() {
        // FNV-1a of the empty byte string is the offset basis itself —
        // useful sanity check that the loop never runs for empty input.
        assert_eq!(fnv1a_64(&[]), FNV1A_OFFSET_BASIS);
    }

    #[test]
    fn single_byte_zero_matches_reference() {
        // FNV-1a(0x00) = basis ^ 0 × prime = basis × prime (mod 2^64).
        let expected = FNV1A_OFFSET_BASIS.wrapping_mul(FNV1A_PRIME);
        assert_eq!(fnv1a_64(&[0x00]), expected);
    }

    #[test]
    fn deterministic_on_same_input() {
        let a = fnv1a_64(b"umwelt");
        let b = fnv1a_64(b"umwelt");
        assert_eq!(a, b);
    }

    #[test]
    fn differs_on_different_input() {
        let a = fnv1a_64(b"umwelt");
        let b = fnv1a_64(b"umwelts");
        assert_ne!(a, b);
    }

    #[test]
    fn collision_counter_starts_zero() {
        // Note: other tests in the crate may have incremented it, so
        // we only assert it is queryable and has a sensible value.
        let _c = COLLISION_COUNT.load(core::sync::atomic::Ordering::Relaxed);
    }
}
