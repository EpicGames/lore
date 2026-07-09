use std::sync::Arc;

use bytes::Bytes;
use criterion::{criterion_group, criterion_main, Criterion};
use rand::Rng;

use lore_storage::fragment_engine::chunk_boundaries;
use lore_storage::FRAGMENT_SIZE_EXPECTED;
use lore_storage::FRAGMENT_SIZE_MINIMUM;
use lore_storage::FRAGMENT_SIZE_THRESHOLD;

fn make_random_buffer(size: usize) -> Bytes {
    let mut data = vec![0u8; size];
    rand::rng().fill(&mut data[..]);
    Bytes::from(data)
}

/// Simulates the old per-chunk approach: one compute-pool spawn + oneshot per boundary.
fn old_per_chunk_boundaries(buffer: &Bytes) -> Vec<(usize, usize)> {
    let size = buffer.len();
    let buffer_guard = buffer.clone();

    let chunker = {
        let slice: &[u8] = unsafe { &*(buffer.as_ref() as *const [u8]) };
        Arc::new(fastcdc::v2020::FastCDC::with_level(
            slice,
            FRAGMENT_SIZE_MINIMUM as u32,
            FRAGMENT_SIZE_EXPECTED as u32,
            FRAGMENT_SIZE_THRESHOLD as u32,
            fastcdc::v2020::Normalization::Level1,
        ))
    };

    let mut offset = 0;
    let mut boundaries = Vec::new();

    while offset < size {
        let remain = size - offset;
        let chunker = chunker.clone();
        let guard = buffer_guard.clone();
        let (tx, rx) = tokio::sync::oneshot::channel();
        lore_base::runtime::compute_pool().spawn(move || {
            let (_, end) = chunker.cut(offset, remain);
            drop(guard);
            let _ = tx.send(end);
        });
        let end = rx.blocking_recv().expect("chunker task failed");
        boundaries.push((offset, end));
        offset = end;
    }

    boundaries
}

fn bench_chunking(c: &mut Criterion) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();

    let sizes: &[usize] = &[
        64 * 1024,
        1024 * 1024,
        16 * 1024 * 1024,
        64 * 1024 * 1024,
    ];

    let buffers: Vec<(String, Bytes)> = sizes
        .iter()
        .map(|&size| {
            let label = if size >= 1024 * 1024 {
                format!("{}MiB", size / (1024 * 1024))
            } else {
                format!("{}KiB", size / 1024)
            };
            (label, make_random_buffer(size))
        })
        .collect();

    let mut group = c.benchmark_group("fastcdc_chunking");

    for (label, buffer) in &buffers {
        group.bench_function(format!("batched_{label}"), |b| {
            b.iter(|| {
                rt.block_on(async {
                    chunk_boundaries(
                        std::hint::black_box(buffer.clone()),
                        std::hint::black_box(0),
                    )
                    .await
                    .expect("chunking succeeds");
                })
            })
        });

        group.bench_function(format!("per_chunk_{label}"), |b| {
            b.iter(|| {
                std::hint::black_box(old_per_chunk_boundaries(std::hint::black_box(buffer)));
            })
        });
    }

    group.finish();

    {
        let buffer_16mb = make_random_buffer(16 * 1024 * 1024);
        c.bench_function("fixed_size/16MiB", |b| {
            b.iter(|| {
                rt.block_on(async {
                    chunk_boundaries(
                        std::hint::black_box(buffer_16mb.clone()),
                        std::hint::black_box(FRAGMENT_SIZE_EXPECTED),
                    )
                    .await
                    .expect("chunking succeeds");
                })
            })
        });
    }
}

criterion_group!(benches, bench_chunking);
criterion_main!(benches);
