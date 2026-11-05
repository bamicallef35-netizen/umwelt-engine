//! SignId-hash microbench.
//!
//! Per ADR-0014 and the P1.2 line in `docs/performance.md`, a single
//! `SignId::from_identity` call must cost ≤ 200 ns on host. This
//! bench feeds a fixed 21-byte canonical-identity blob through
//! `fnv1a_64` plus the full `SignIdentity → SignId` path so the
//! number in the docs is the number we measure.
//!
//! Run with `cargo bench -p umwelt-core --bench hash`.

use criterion::{black_box, criterion_group, criterion_main, Criterion};
use umwelt_core::{
    hash::fnv1a_64, SemioticLayer, SignDomain, SignId, SignIdentity, SIGN_IDENTITY_BYTES,
};

fn bench_fnv1a_64_21b(c: &mut Criterion) {
    // Deterministic 21-byte input (matches ADR-0014 canonical width).
    let mut input = [0u8; SIGN_IDENTITY_BYTES];
    for (i, slot) in input.iter_mut().enumerate() {
        *slot = i as u8 ^ 0x5A;
    }
    c.bench_function("fnv1a_64/21b", |b| {
        b.iter(|| fnv1a_64(black_box(&input)));
    });
}

fn bench_sign_id_from_identity(c: &mut Criterion) {
    let identity = SignIdentity {
        layer: SemioticLayer::Icon,
        domain: SignDomain::Spatial,
        sign_type: 7,
        agent_id: 0xDEAD_BEEF,
        spatial_ref: [-1, 2, -3],
        time_bucket: 0x1234_5678_9ABC_DEF0,
    };
    c.bench_function("sign_id/from_identity", |b| {
        b.iter(|| SignId::from_identity(black_box(&identity)));
    });
}

criterion_group!(benches, bench_fnv1a_64_21b, bench_sign_id_from_identity);
criterion_main!(benches);
