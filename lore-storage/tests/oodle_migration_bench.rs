// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT

//! Timing for the Oodle migration pass over stores shaped like a long-lived client store.
//!
//! Each fixture reproduces the composition sampled from a client store that predates the switch
//! to Zstd: the mix of codecs, how much of it is durable, and the size distribution of each kind
//! of entry. Only local-only Oodle entries are decoded and re-encoded by the pass, so only they
//! carry content of their own, Oodle-encoded and addressed by its hash. Every other entry is
//! skipped without its bytes being read, and points into a shared pool of filler.
//!
//! Two layouts are built at each size:
//!
//! - **flat**, every group at 256 buckets, the layout a store written before lazy fan-out reads
//!   back at. Its bucket files carry the current version and a level marker, which leaves the
//!   flush the pass ends each group with on the same path a pre-fan-out store's takes.
//! - **lazy**, the bucket count lazy fan-out settles on for the entry count.
//!
//! `LORE_BENCH_STORE` names an existing store root, the directory holding `immutable/`, to time
//! instead of the fixtures. It is copied first and never migrated in place.
//!
//! Each fixture is timed straight after it is written, so the page cache is warm.
//!
//! Run with:
//!     `OODLE_LIB_DIR=<oodle lib dir> cargo test -p lore-storage --release --features oodle \
//!         --test oodle_migration_bench -- --ignored --nocapture`
//!
//! `LORE_BENCH_SIZES_GIB` lists the stored sizes to build, comma separated, and defaults to `1`.
//! `LORE_BENCH_LAYOUTS` lists the layouts, and defaults to `flat,lazy`. `LORE_BENCH_MIX` lists
//! the codec mixes, and defaults to `sampled`: `no-oodle` makes every Oodle entry Zstd instead,
//! and `durable-oodle` makes every Oodle entry durably stored, each timing a pass with nothing to
//! re-encode. Fixtures are built under `TMPDIR`, and a migrated store holds about a fifth more
//! than its stored size.

#[cfg(all(test, feature = "oodle"))]
mod tests {
    use std::collections::HashMap;
    use std::path::Path;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::AtomicU64;
    use std::sync::atomic::Ordering;
    use std::time::Duration;
    use std::time::Instant;
    use std::time::SystemTime;

    use bytes::Bytes;
    use lore_base::lore_spawn;
    use lore_base::test_util::TempDir;
    use lore_base::types::Address;
    use lore_base::types::Context;
    use lore_base::types::FRAGMENT_SIZE_THRESHOLD;
    use lore_base::types::Fragment;
    use lore_base::types::FragmentFlags;
    use lore_base::types::Hash;
    use lore_base::types::Partition;
    use lore_storage::CompressionMode;
    use lore_storage::LocalImmutableStore;
    use lore_storage::hash;
    use lore_storage::immutable_store::ImmutableStore;
    use lore_storage::local::immutable_store::ImmutableStoreSettings;
    use lore_storage::local::immutable_store::oodle_migration::migrate_groups;
    use rand::Rng;
    use rand::SeedableRng;
    use rand::rngs::SmallRng;
    use tokio::task::JoinSet;

    const GIB: f64 = (1u64 << 30) as f64;
    const MIB: f64 = (1u64 << 20) as f64;

    /// Where an entry's payload comes from.
    #[derive(Clone, Copy)]
    enum Payload {
        /// Content of its own, Oodle-encoded, which the pass decodes and re-encodes.
        Encoded,
        /// A slice of the filler pool, which the pass never reads.
        Filler,
        /// None: the entry is metadata only.
        Absent,
    }

    /// One kind of entry in the sampled store.
    struct Kind {
        /// Entries of this kind in the sampled store, which is the weight it is drawn with.
        count: u32,
        flags: u32,
        payload: Payload,
        /// Content size at every twentieth quantile, from the minimum to the maximum.
        content: [u64; 21],
        /// Stored size at the same quantiles. An `Encoded` entry's stored size is whatever its
        /// encoding comes to.
        stored: [u64; 21],
    }

    const OODLE: u32 = FragmentFlags::PayloadCompressedOodle2.bits();
    const ZSTD: u32 = FragmentFlags::PayloadCompressedZstd.bits();
    const DURABLE: u32 = FragmentFlags::PayloadStoredDurable.bits();

    #[rustfmt::skip]
    const KINDS: [Kind; 7] = [
        // Oodle, durable: left as it is.
        Kind { count: 25_715, flags: OODLE | DURABLE, payload: Payload::Filler,
            content: [257, 288, 320, 320, 320, 728, 1517, 2614, 3525, 5396, 7902, 12079, 21550, 39455, 49280, 49280, 57467, 65568, 65568, 66439, 261906],
            stored: [86, 124, 155, 155, 164, 357, 710, 1044, 1344, 1657, 2023, 2543, 3292, 4549, 5792, 7032, 8465, 11920, 19813, 23083, 192295] },
        // Uncompressed, durable: skipped.
        Kind { count: 7_184, flags: DURABLE, payload: Payload::Filler,
            content: [7, 64, 96, 96, 96, 96, 96, 128, 128, 128, 128, 132, 200, 207, 211, 218, 230, 33730, 80463, 109583, 217185406],
            stored: [7, 48, 80, 80, 96, 96, 96, 96, 96, 128, 128, 128, 128, 132, 200, 207, 211, 217, 228, 252, 239412] },
        // Oodle, local only: re-encoded.
        Kind { count: 5_556, flags: OODLE, payload: Payload::Encoded,
            content: [257, 288, 288, 288, 288, 313, 320, 320, 320, 320, 320, 49280, 49280, 49280, 49280, 49280, 49280, 49280, 49280, 49280, 142358],
            stored: [0; 21] },
        // Uncompressed, local only: skipped.
        Kind { count: 3_368, flags: 0, payload: Payload::Filler,
            content: [12, 64, 64, 64, 64, 64, 64, 64, 64, 64, 96, 96, 96, 96, 128, 128, 128, 128, 192, 224, 107693],
            stored: [12, 64, 64, 64, 64, 64, 64, 64, 64, 64, 96, 96, 96, 96, 128, 128, 128, 128, 192, 224, 384] },
        // Oodle, no local payload: skipped.
        Kind { count: 1_377, flags: OODLE, payload: Payload::Absent,
            content: [419, 36390, 60113, 66073, 67979, 77868, 87482, 98622, 112651, 128634, 145211, 165459, 193767, 233525, 262144, 262144, 262144, 262144, 262144, 262144, 262144],
            stored: [264, 1927, 2739, 3457, 4143, 4584, 5000, 5443, 5893, 6390, 6923, 7417, 8114, 8832, 9488, 10054, 10838, 11940, 13200, 15439, 33220] },
        // Zstd, durable: skipped.
        Kind { count: 306, flags: ZSTD | DURABLE, payload: Payload::Filler,
            content: [2462, 35718, 44444, 52651, 61241, 66124, 68576, 71890, 74393, 77256, 80943, 86460, 91415, 95098, 99504, 105920, 115290, 126191, 146375, 160479, 262144],
            stored: [813, 6740, 11571, 13978, 15478, 16634, 17689, 19077, 20553, 21710, 22775, 24501, 26546, 28310, 31160, 32341, 34283, 36860, 41270, 52074, 106597] },
        // Uncompressed, no local payload: skipped.
        Kind { count: 37, flags: 0, payload: Payload::Absent,
            content: [33902, 39091, 42588, 53663, 56342, 65859, 66020, 67443, 74522, 76714, 78274, 80826, 86471, 92824, 98029, 101519, 102675, 113705, 117232, 143187, 168140],
            stored: [33902, 39091, 42588, 53663, 56342, 65859, 66020, 67443, 74522, 76714, 78274, 80826, 86471, 92824, 98029, 101519, 102675, 113705, 117232, 143187, 168140] },
    ];

    /// Share of entries under the zero context; the rest spread over [`CONTEXTS`] others.
    const ZERO_CONTEXT_SHARE: f64 = 0.674;
    const CONTEXTS: u32 = 2_855;

    /// Bytes of filler every `Filler` payload is a slice of. Larger than any stored size drawn.
    const FILLER_LEN: usize = 1 << 20;

    /// Symbols encoded content is drawn from. Eight of them, three bits a byte, put Oodle's ratio
    /// near the 0.4 the sampled store's local-only entries came to.
    const SYMBOLS: [u8; 8] = *b"etaoinsr";

    /// Stored size over content size for content drawn from [`SYMBOLS`], as the pass measures it.
    const ENCODED_RATIO: f64 = 0.38;

    #[derive(Clone, Copy, PartialEq)]
    enum Layout {
        Flat,
        Lazy,
    }

    impl Layout {
        fn name(self) -> &'static str {
            match self {
                Layout::Flat => "flat",
                Layout::Lazy => "lazy",
            }
        }

        fn settings(self) -> ImmutableStoreSettings {
            ImmutableStoreSettings {
                initial_fan_out_level: match self {
                    Layout::Flat => lore_storage::local::fan_out::FAN_OUT_LEVEL_MAX,
                    Layout::Lazy => 1,
                },
                ..client_settings()
            }
        }
    }

    /// Client-shaped: nothing is durable until an entry says so.
    fn client_settings() -> ImmutableStoreSettings {
        ImmutableStoreSettings {
            protect_local_fragment: true,
            implicit_durable_stored: false,
            ..Default::default()
        }
    }

    fn partition() -> Partition {
        Partition::from([7u8; 16])
    }

    fn sizes_gib() -> Vec<f64> {
        std::env::var("LORE_BENCH_SIZES_GIB")
            .unwrap_or_else(|_| "1".to_string())
            .split(',')
            .filter_map(|size| size.trim().parse().ok())
            .filter(|size: &f64| *size > 0.0)
            .collect()
    }

    fn layouts() -> Vec<Layout> {
        std::env::var("LORE_BENCH_LAYOUTS")
            .unwrap_or_else(|_| "flat,lazy".to_string())
            .split(',')
            .filter_map(|name| match name.trim() {
                "flat" => Some(Layout::Flat),
                "lazy" => Some(Layout::Lazy),
                _ => None,
            })
            .collect()
    }

    /// Which codecs a fixture's entries carry.
    #[derive(Clone, Copy)]
    enum Mix {
        /// The sampled store as it is.
        Sampled,
        /// The sampled store with every Oodle entry Zstd instead, so the pass has nothing to do.
        NoOodle,
        /// The sampled store with every Oodle entry durably stored, so the pass re-encodes
        /// nothing.
        DurableOodle,
    }

    impl Mix {
        fn name(self) -> &'static str {
            match self {
                Mix::Sampled => "sampled",
                Mix::NoOodle => "no-oodle",
                Mix::DurableOodle => "durable-oodle",
            }
        }

        fn flags(self, flags: u32) -> u32 {
            match self {
                Mix::NoOodle if flags & OODLE != 0 => (flags & !OODLE) | ZSTD,
                Mix::DurableOodle if flags & OODLE != 0 => flags | DURABLE,
                Mix::Sampled | Mix::NoOodle | Mix::DurableOodle => flags,
            }
        }
    }

    fn mixes() -> Vec<Mix> {
        std::env::var("LORE_BENCH_MIX")
            .unwrap_or_else(|_| "sampled".to_string())
            .split(',')
            .filter_map(|name| match name.trim() {
                "sampled" => Some(Mix::Sampled),
                "no-oodle" => Some(Mix::NoOodle),
                "durable-oodle" => Some(Mix::DurableOodle),
                _ => None,
            })
            .collect()
    }

    /// The value at quantile `u` of the twentieth-quantile table `points`, interpolated linearly.
    fn at_quantile(points: &[u64; 21], u: f64) -> u64 {
        let position = u * 20.0;
        let lower = (position as usize).min(19);
        let fraction = position - lower as f64;
        let (low, high) = (points[lower] as f64, points[lower + 1] as f64);
        (low + (high - low) * fraction).round() as u64
    }

    fn draw_kind(rng: &mut SmallRng) -> &'static Kind {
        let total: u32 = KINDS.iter().map(|kind| kind.count).sum();
        let mut pick = rng.random_range(0..total);
        for kind in &KINDS {
            if pick < kind.count {
                return kind;
            }
            pick -= kind.count;
        }
        &KINDS[0]
    }

    fn draw_context(rng: &mut SmallRng) -> Context {
        if rng.random_bool(ZERO_CONTEXT_SHARE) {
            return Context::from([0u8; 16]);
        }
        let index = rng.random_range(1..=CONTEXTS);
        let mut bytes = [0u8; 16];
        bytes[..4].copy_from_slice(&index.to_le_bytes());
        Context::from(bytes)
    }

    fn encodable_content(rng: &mut SmallRng, len: usize) -> Vec<u8> {
        let mut content = Vec::with_capacity(len);
        while content.len() < len {
            let mut bits: u64 = rng.random();
            for _ in 0..21 {
                if content.len() == len {
                    break;
                }
                content.push(SYMBOLS[(bits & 7) as usize]);
                bits >>= 3;
            }
        }
        content
    }

    /// Store entries drawn from [`KINDS`] until the store holds `target` stored bytes between all
    /// fillers.
    async fn fill(
        store: Arc<LocalImmutableStore>,
        filler: Bytes,
        written: Arc<AtomicU64>,
        target: u64,
        seed: u64,
        mix: Mix,
    ) {
        let mut rng = SmallRng::seed_from_u64(seed);
        while written.load(Ordering::Relaxed) < target {
            let kind = draw_kind(&mut rng);
            let u: f64 = rng.random();
            let context = draw_context(&mut rng);
            let content_size = at_quantile(&kind.content, u);
            let (address, fragment, payload) = match (mix, kind.payload) {
                (Mix::Sampled, Payload::Encoded) => {
                    let content = encodable_content(&mut rng, content_size as usize);
                    let raw = Fragment {
                        flags: 0,
                        size_payload: content.len() as u32,
                        size_content: content.len() as u64,
                    };
                    let (fragment, encoded) =
                        lore_storage::compress::compress_without_deprecation_checks(
                            raw,
                            &content,
                            CompressionMode::Oodle,
                        )
                        .expect("the content is Oodle-compressible");
                    let address = Address {
                        hash: hash::hash_slice(&content),
                        context,
                    };
                    let size = fragment.size_payload as usize;
                    (address, fragment, Some(encoded.slice(..size)))
                }
                (_, payload) => {
                    let stored = match payload {
                        // The entry the pass would re-encode is Zstd or durable instead, stored at
                        // the ratio the encoded content comes to.
                        Payload::Encoded => (content_size as f64 * ENCODED_RATIO) as usize,
                        Payload::Filler | Payload::Absent => at_quantile(&kind.stored, u) as usize,
                    }
                    .clamp(1, FRAGMENT_SIZE_THRESHOLD);
                    let address = Address {
                        hash: Hash::from(rng.random::<[u8; 32]>()),
                        context,
                    };
                    let fragment = Fragment {
                        flags: mix.flags(kind.flags),
                        size_payload: stored as u32,
                        size_content: content_size,
                    };
                    let payload = (!matches!(payload, Payload::Absent)).then(|| {
                        let offset = rng.random_range(0..=FILLER_LEN - stored);
                        filler.slice(offset..offset + stored)
                    });
                    (address, fragment, payload)
                }
            };
            let stored = payload.as_ref().map_or(0, |payload| payload.len() as u64);
            store
                .clone()
                .store(partition(), address, fragment, payload, false)
                .await
                .expect("the store accepts the entry");
            written.fetch_add(stored, Ordering::Relaxed);
        }
    }

    /// Write a store at `root` holding `target` stored bytes of `mix`, laid out by `layout`.
    async fn build(root: &Path, layout: Layout, mix: Mix, target: u64) -> Duration {
        let start = Instant::now();
        let store = LocalImmutableStore::new(Some(root.to_path_buf()), layout.settings())
            .await
            .expect("store opens");
        let mut rng = SmallRng::seed_from_u64(0);
        let filler = Bytes::from(
            (0..FILLER_LEN)
                .map(|_| rng.random::<u8>())
                .collect::<Vec<u8>>(),
        );
        let written = Arc::new(AtomicU64::new(0));
        let workers = std::thread::available_parallelism().map_or(8, |n| n.get());
        let mut tasks = JoinSet::new();
        for seed in 0..workers {
            lore_spawn!(
                tasks,
                fill(
                    store.clone(),
                    filler.clone(),
                    written.clone(),
                    target,
                    seed as u64 + 1,
                    mix,
                )
            );
        }
        while let Some(joined) = tasks.join_next().await {
            joined.expect("a filler finishes");
        }
        let dyn_store: Arc<dyn ImmutableStore> = store;
        dyn_store.flush(false).await.expect("store flushes");
        start.elapsed()
    }

    /// What a store holds before the pass, by the rules the pass treats entries with.
    #[derive(Default)]
    struct Survey {
        entries: u64,
        bucket_files: u64,
        buckets_per_group: Vec<usize>,
        reencoded: u64,
        reencoded_content: u64,
        reencoded_stored: u64,
        durable: u64,
        stored: u64,
    }

    async fn survey(root: &Path) -> Survey {
        let store = LocalImmutableStore::new(Some(root.to_path_buf()), client_settings())
            .await
            .expect("store opens");
        store.deserialize_all_buckets().await.expect("buckets load");
        let mut survey = Survey::default();
        for group in &store.group {
            let bucket_count = group.bucket_count.load(Ordering::Relaxed);
            survey.buckets_per_group.push(bucket_count);
            for index in 0..bucket_count {
                let bucket = group.bucket(index).read().await;
                if !bucket.entry.is_empty() {
                    survey.bucket_files += 1;
                }
                for entry in bucket.entry.iter() {
                    survey.entries += 1;
                    let data = entry.data;
                    if data.pack_file == 0 {
                        continue;
                    }
                    survey.stored += u64::from(data.size_payload);
                    if data.flags & OODLE == 0 {
                        continue;
                    }
                    if data.flags & DURABLE != 0 {
                        survey.durable += 1;
                    } else {
                        survey.reencoded += 1;
                        survey.reencoded_content += data.size_content;
                        survey.reencoded_stored += u64::from(data.size_payload);
                    }
                }
            }
        }
        survey
    }

    /// Every file under `dir`, with its modification time and size.
    fn files(dir: &Path) -> HashMap<PathBuf, (SystemTime, u64)> {
        let mut found = HashMap::new();
        let mut pending = vec![dir.to_path_buf()];
        while let Some(dir) = pending.pop() {
            for entry in std::fs::read_dir(&dir).expect("directory reads") {
                let entry = entry.expect("directory entry reads");
                let metadata = entry.metadata().expect("metadata reads");
                if metadata.is_dir() {
                    pending.push(entry.path());
                } else {
                    let modified = metadata.modified().expect("modification time reads");
                    found.insert(entry.path(), (modified, metadata.len()));
                }
            }
        }
        found
    }

    fn is_bucket_file(path: &Path) -> bool {
        path.file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("index_"))
    }

    fn copy_tree(from: &Path, to: &Path) {
        std::fs::create_dir_all(to).expect("directory creates");
        for entry in std::fs::read_dir(from).expect("directory reads") {
            let entry = entry.expect("directory entry reads");
            let target = to.join(entry.file_name());
            if entry.file_type().expect("file type reads").is_dir() {
                copy_tree(&entry.path(), &target);
            } else {
                std::fs::copy(entry.path(), &target).expect("file copies");
            }
        }
    }

    /// Time the first open and a full pass over the store at `root`, and print a table row.
    async fn measure(label: &str, root: &Path, build: Option<Duration>) {
        let survey = survey(root).await;
        let immutable = root.join("immutable");
        let before = files(&immutable);

        let start = Instant::now();
        let store = LocalImmutableStore::new(Some(root.to_path_buf()), client_settings())
            .await
            .expect("store opens");
        let opened = start.elapsed();
        let path = store.path.clone().expect("store has a path");

        let start = Instant::now();
        let completed = migrate_groups(store.clone(), &path, 255)
            .await
            .expect("the pass runs");
        let migration = start.elapsed();
        assert!(completed, "every group migrates");
        assert_eq!(
            store.info.read().await.next_group_index_to_migrate_oodle,
            -1,
            "a completed pass leaves nothing for the next one"
        );
        drop(store);

        let after = files(&immutable);
        let rewritten = before
            .iter()
            .filter(|(path, (modified, _))| {
                is_bucket_file(path) && after.get(*path).is_some_and(|(now, _)| now != modified)
            })
            .count();
        let disk_before: u64 = before.values().map(|(_, len)| len).sum();
        let disk_after: u64 = after.values().map(|(_, len)| len).sum();

        let mut levels: Vec<usize> = survey.buckets_per_group.clone();
        levels.sort_unstable();
        levels.dedup();
        let levels = levels
            .iter()
            .map(usize::to_string)
            .collect::<Vec<_>>()
            .join("/");
        let ratio = survey.reencoded_stored as f64 / survey.reencoded_content.max(1) as f64;
        println!(
            "| {label} | {:.2} GiB | {} | {levels} | {} | {} ({:.1} MiB, ratio {ratio:.2}) | {} | {rewritten} | {:.3} s | {:.1} s | {:.2} ms | {:.2} → {:.2} GiB | {} |",
            survey.stored as f64 / GIB,
            survey.entries,
            survey.bucket_files,
            survey.reencoded,
            survey.reencoded_content as f64 / MIB,
            survey.durable,
            opened.as_secs_f64(),
            migration.as_secs_f64(),
            migration.as_secs_f64() * 1_000.0 / rewritten.max(1) as f64,
            disk_before as f64 / GIB,
            disk_after as f64 / GIB,
            build.map_or("-".to_string(), |build| format!(
                "{:.0} s",
                build.as_secs_f64()
            )),
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "benchmark: run on demand with --ignored"]
    async fn oodle_migration_across_store_sizes() {
        println!(
            "| Store | Stored | Entries | Buckets per group | Bucket files | Re-encoded | Durable Oodle | Rewritten bucket files | Open | Migration | Per rewritten file | On disk before → after | Build |"
        );
        println!("| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |");

        if let Ok(source) = std::env::var("LORE_BENCH_STORE") {
            let dir = TempDir::new("oodle-migration-bench-copy-");
            copy_tree(
                &Path::new(&source).join("immutable"),
                &dir.child("immutable"),
            );
            measure("copy of LORE_BENCH_STORE", dir.path(), None).await;
            return;
        }

        for mix in mixes() {
            for size in sizes_gib() {
                for layout in layouts() {
                    let dir = TempDir::new("oodle-migration-bench-");
                    let built = build(dir.path(), layout, mix, (size * GIB) as u64).await;
                    let label = format!("{} {}", layout.name(), mix.name());
                    measure(&label, dir.path(), Some(built)).await;
                }
            }
        }
    }
}
