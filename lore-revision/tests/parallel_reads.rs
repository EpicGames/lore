// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
#[cfg(test)]
mod tests {
    #![allow(clippy::disallowed_methods)] // Test fixture writes; not subject to repository write-token discipline.

    use std::io::Write;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering;
    use std::time::Duration;
    use std::time::Instant;

    use lore_base::lore_spawn;
    use lore_base::runtime::LORE_CONTEXT;
    use lore_base::runtime::runtime;
    use lore_base::types::Hash;
    use lore_revision::branch;
    use lore_revision::commit;
    use lore_revision::commit::CommitOptions;
    use lore_revision::file;
    use lore_revision::instance;
    use lore_revision::interface::LoreArray;
    use lore_revision::interface::LoreString;
    use lore_revision::lore::BranchId;
    use lore_revision::lore::RepositoryId;
    use lore_revision::metadata::Metadata;
    use lore_revision::node::NodeFlags;
    use lore_revision::repository;
    use lore_revision::repository::RepositoryAccess;
    use lore_revision::repository::RepositoryContext;
    use lore_revision::stage;
    use lore_revision::stage::StageOptions;
    use lore_revision::state;
    use tokio::task::JoinSet;

    include!("helper.rs");

    const SEED_FILE_COUNT: usize = 32;
    const SEED_FILE_BYTES: usize = 1024;

    // Stress-test configuration: many concurrent readers + writers all
    // hammering the same anchor key at once. This is intentionally harsher
    // than any realistic client load — it exists to surface contention bugs
    // that the realistic-burst test would not.
    const STRESS_READER_TASKS: usize = 16;
    const STRESS_READS_PER_TASK: usize = 200;
    const STRESS_WRITER_TASKS: usize = 4;
    const STRESS_WRITES_PER_TASK: usize = 500;

    // Burst-test configuration: matches the desktop Lore client's actual
    // shape — reads dominate, writes are rare. Phase 1 models a file-tree
    // expansion (many concurrent reads, no writes). Phase 2 models the
    // post-commit refresh (one real `stage + commit`, then a small burst
    // of follow-up reads). See `desktop_client_burst_pattern` for details.
    const BURST_READER_TASKS: usize = 12;
    const BURST_READS_PER_TASK: usize = 25;
    const BURST_REFRESH_READER_TASKS: usize = 4;
    const BURST_REFRESH_READS_PER_TASK: usize = 5;

    /// Pathological-stress observational test for the in-process
    /// parallel-read API.
    ///
    /// Spawns many concurrent reader and writer tokio tasks against a single
    /// shared `Arc<RepositoryContext>` (the path the parallel-access feature
    /// is designed to enable) and lets them blast away at each other with no
    /// artificial `yield_now` / `sleep` — exercising the read-while-write
    /// paths through `ReadHandle` / `WriteHandle` and the underlying mutable
    /// store.
    ///
    /// The setup uses `repository::load_and_connect_with_token`, which
    /// builds the production on-disk immutable + mutable stores under
    /// `tempdir/.lore/`. Reads hit real disk I/O; writes flush through the
    /// real on-disk mutable store.
    ///
    /// Each reader iteration: `load_current_anchor` → `State::deserialize`
    /// → `state.tree` → `Metadata::deserialize`. Each writer rapidly
    /// alternates the current anchor between two valid revision signatures.
    ///
    /// Logs aggregate timings; correctness asserts only — no perf
    /// assertions.
    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn parallel_reads_with_concurrent_writes() {
        let execution = setup_test_execution();
        let repository_id = RepositoryId::from(uuid::Uuid::now_v7());

        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let SeededRepo {
                    repository,
                    sig1,
                    sig2,
                    default_branch_id,
                    _tempdir,
                    ..
                } = Box::pin(setup_seeded_repo(repository_id)).await;

                // Roll the anchor back to sig1 so writers alternating
                // sig1 ↔ sig2 produce real visible state changes (the
                // branch head is at sig2 from setup). Readers must accept
                // either signature.
                instance::store_current_anchor(&repository, sig1)
                    .await
                    .expect("rollback anchor for stress");

                let valid_sigs = [sig1, sig2];

                let started = Instant::now();
                let mut readers: JoinSet<Vec<Duration>> = JoinSet::new();
                for _ in 0..STRESS_READER_TASKS {
                    let repo = repository.clone();
                    lore_spawn!(readers, async move {
                        run_reader_loop(
                            repo,
                            STRESS_READS_PER_TASK,
                            sig1,
                            sig2,
                            default_branch_id,
                        )
                        .await
                    });
                }

                let mut writers: JoinSet<Vec<Duration>> = JoinSet::new();
                for w in 0..STRESS_WRITER_TASKS {
                    let repo = repository.clone();
                    lore_spawn!(writers, async move {
                        let mut samples = Vec::with_capacity(STRESS_WRITES_PER_TASK);
                        for i in 0..STRESS_WRITES_PER_TASK {
                            let sig = valid_sigs[(w + i) % valid_sigs.len()];
                            let t = Instant::now();
                            instance::store_current_anchor(&repo, sig)
                                .await
                                .expect("store_current_anchor");
                            samples.push(t.elapsed());
                        }
                        samples
                    });
                }

                let mut read_samples: Vec<Duration> = Vec::new();
                while let Some(joined) = readers.join_next().await {
                    read_samples.extend(joined.expect("reader task panicked"));
                }
                let mut write_samples: Vec<Duration> = Vec::new();
                while let Some(joined) = writers.join_next().await {
                    write_samples.extend(joined.expect("writer task panicked"));
                }
                let total_wall = started.elapsed();

                let (rev_after, branch_after) =
                    instance::load_current_anchor(&repository)
                        .await
                        .expect("final load_current_anchor");
                assert!(
                    rev_after == sig1 || rev_after == sig2,
                    "final anchor revision was neither valid signature"
                );
                assert_eq!(branch_after, default_branch_id, "final anchor branch");
                let _state = state::State::deserialize(repository.clone(), rev_after)
                    .await
                    .expect("State::deserialize after storm");

                summarize(
                    "stress.read iteration (anchor + state + tree + metadata)",
                    &mut read_samples,
                );
                summarize("stress.write.store_current_anchor", &mut write_samples);
                eprintln!("stress total wall-clock: {total_wall:?}");
                eprintln!(
                    "stress configuration: reader_tasks={STRESS_READER_TASKS} reads_per_task={STRESS_READS_PER_TASK} writer_tasks={STRESS_WRITER_TASKS} writes_per_task={STRESS_WRITES_PER_TASK}",
                );
            }))
            .await
            .expect("Test task failed");
    }

    /// Realistic observational test modelling the desktop Lore client's
    /// actual usage pattern.
    ///
    /// The desktop client embeds the Lore SDK in its Electron main process
    /// and dispatches operations from React/Redux thunks. Investigation of
    /// `lore-ws/desktop-client` shows two dominant load shapes:
    ///
    /// - **File-tree expansion**: when the user expands a directory or
    ///   loads a 3-way conflict view, the client fires many `fileDiff` /
    ///   `fileHistory` reads in parallel via `Promise.all`. Concurrent
    ///   writes during this burst are rare to nonexistent.
    /// - **Post-commit refresh**: after a single commit, the client refreshes
    ///   `repositoryStatus`, `revisionHistory`, and `branchList` — a small
    ///   burst of reads following one *real* commit (multi-step write).
    ///
    /// Phase 2 here issues a real `stage + commit`, not a synthetic anchor
    /// write — that's the actual heavy write the client experiences.
    /// Correctness asserts only.
    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn desktop_client_burst_pattern() {
        let execution = setup_test_execution();
        let repository_id = RepositoryId::from(uuid::Uuid::now_v7());

        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let SeededRepo {
                    repository,
                    sig1,
                    sig2,
                    default_branch_id,
                    path,
                    _tempdir,
                } = Box::pin(setup_seeded_repo(repository_id)).await;

                // Phase 1: file-tree expansion. Many concurrent readers, no
                // concurrent writers. Anchor stays at sig2 (the natural
                // head from setup) throughout.
                let phase1_started = Instant::now();
                let mut readers: JoinSet<Vec<Duration>> = JoinSet::new();
                for _ in 0..BURST_READER_TASKS {
                    let repo = repository.clone();
                    lore_spawn!(readers, async move {
                        run_reader_loop(
                            repo,
                            BURST_READS_PER_TASK,
                            sig1,
                            sig2,
                            default_branch_id,
                        )
                        .await
                    });
                }
                let mut phase1_samples: Vec<Duration> = Vec::new();
                while let Some(joined) = readers.join_next().await {
                    phase1_samples.extend(joined.expect("reader task panicked"));
                }
                let phase1_wall = phase1_started.elapsed();

                let (rev_mid, branch_mid) = instance::load_current_anchor(&repository)
                    .await
                    .expect("mid load_current_anchor");
                assert_eq!(rev_mid, sig2, "phase 1 must not mutate anchor");
                assert_eq!(branch_mid, default_branch_id, "phase 1 branch");

                // Phase 2: post-commit refresh. A real `stage + commit`
                // (the heavy write the client actually does) advances the
                // anchor; then a small burst of follow-up reads models
                // status / history / branchList running after the commit.
                let phase2_started = Instant::now();
                let write_t = Instant::now();
                let new_sig = seed_commit(&repository, &path, 0xFF).await;
                instance::store_current_anchor(&repository, new_sig)
                    .await
                    .expect("post-commit store_current_anchor");
                let write_elapsed = write_t.elapsed();

                // Refresh readers may observe sig2 (the pre-commit head)
                // briefly or new_sig (the freshly committed head).
                let valid_for_refresh = (sig2, new_sig);

                let mut refresh_readers: JoinSet<Vec<Duration>> = JoinSet::new();
                for _ in 0..BURST_REFRESH_READER_TASKS {
                    let repo = repository.clone();
                    let (s1, s2) = valid_for_refresh;
                    lore_spawn!(refresh_readers, async move {
                        run_reader_loop(
                            repo,
                            BURST_REFRESH_READS_PER_TASK,
                            s1,
                            s2,
                            default_branch_id,
                        )
                        .await
                    });
                }
                let mut phase2_samples: Vec<Duration> = Vec::new();
                while let Some(joined) = refresh_readers.join_next().await {
                    phase2_samples.extend(joined.expect("reader task panicked"));
                }
                let phase2_wall = phase2_started.elapsed();

                let (rev_after, branch_after) =
                    instance::load_current_anchor(&repository)
                        .await
                        .expect("final load_current_anchor");
                assert_eq!(rev_after, new_sig, "phase 2 must observe the post-commit sig");
                assert_eq!(branch_after, default_branch_id, "final branch");

                summarize("burst.phase1 (file-tree expand)", &mut phase1_samples);
                summarize("burst.phase2 (post-commit refresh)", &mut phase2_samples);
                eprintln!("burst.phase1 wall-clock: {phase1_wall:?}");
                eprintln!("burst.phase2 wall-clock: {phase2_wall:?}");
                eprintln!("burst.phase2 stage+commit elapsed: {write_elapsed:?}");
                eprintln!(
                    "burst configuration: phase1={BURST_READER_TASKS}r×{BURST_READS_PER_TASK} phase2={BURST_REFRESH_READER_TASKS}r×{BURST_REFRESH_READS_PER_TASK} + 1 stage+commit"
                );
            }))
            .await
            .expect("Test task failed");
    }

    // Race-repro configuration. A wide working-directory delta (many files)
    // and a long-running switcher maximize the TOCTOU window between the
    // status scan's directory walk and its per-file metadata stat.
    const RACE_EXTRA_FILES: usize = 400;
    const RACE_SWITCH_ITERS: usize = 80;
    const RACE_SCANNER_TASKS: usize = 6;
    // Safety cap so scanners can never spin forever if the switcher wedges.
    const RACE_MAX_SCANS_PER_TASK: usize = 5000;

    /// Regression test for a TOCTOU race between `branch switch` and a
    /// concurrent `status --scan`.
    ///
    /// When a branch switch moves from a revision that *has* a set of files to
    /// one that does *not*, it deletes those files from the working directory
    /// (`sync::sync` → `realize_changes_delete`) *before* it updates the
    /// current anchor. A simultaneous `status --scan` walks the filesystem and
    /// then, for each detected change, performs an independent
    /// `tokio::fs::metadata` stat (`file_size_from_node_change_path`). If the
    /// switch unlinks a file between the walk and that stat, the stat fails
    /// with `NotFound` — which previously surfaced as an `Internal` error and
    /// failed the whole `status` command.
    ///
    /// The scanner runs on a **read-only** repository context: it shares the
    /// process-local repository flock with the switching writer (so the two
    /// interleave at filesystem granularity) but holds no write token, so its
    /// scan never serializes on the per-path write mutex. This mirrors an
    /// embedded SDK host (e.g. the desktop client) displaying status while
    /// another in-process context performs a branch switch.
    ///
    /// Before the fix this test fails: at least one concurrent scan observes a
    /// file mid-delete and returns an error. After the fix, a `NotFound` during
    /// the size stat is treated as a benign concurrent deletion and every scan
    /// succeeds.
    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn status_scan_survives_concurrent_branch_switch_delete() {
        let execution = setup_test_execution();
        let repository_id = RepositoryId::from(uuid::Uuid::now_v7());

        runtime()
            .spawn(LORE_CONTEXT.scope(execution.clone(), async move {
                let SeededRepo {
                    repository,
                    sig2,
                    path,
                    _tempdir,
                    ..
                } = Box::pin(setup_seeded_repo(repository_id)).await;

                // Commit a large set of extra files on top of sig2. `sig_full`
                // contains them; `sig2` does not. Switching the anchor
                // sig_full → sig2 realizes a delete of all RACE_EXTRA_FILES from
                // the working directory; sig2 → sig_full restores them. Both
                // revisions are on the default branch, so `branch_switch`
                // accepts either via its `signature` option.
                let sig_full =
                    commit_n_files(&repository, &path, "race", RACE_EXTRA_FILES, 0x33).await;

                // Read-only status context (see the doc comment): shares the
                // flock, holds no write token.
                let status_repo =
                    repository::load_and_connect(path.as_path(), RepositoryAccess::ReadOnly)
                        .await
                        .expect("read-only status context");

                let main_branch = branch::DEFAULT_DEFAULT_NAME.to_string();
                let done = Arc::new(AtomicBool::new(false));

                // Switcher: alternate the working directory between the
                // file-rich and file-poor revisions, deleting/restoring
                // RACE_EXTRA_FILES on each switch.
                let mut switcher: JoinSet<()> = JoinSet::new();
                {
                    let repo = repository.clone();
                    let main_branch = main_branch.clone();
                    let done = done.clone();
                    lore_spawn!(switcher, async move {
                        let token = repo
                            .try_write_token()
                            .expect("switcher requires write-mode repository context");
                        for i in 0..RACE_SWITCH_ITERS {
                            let signature = if i % 2 == 0 { sig2 } else { sig_full };
                            Box::pin(repository::branch_switch(
                                repo.clone(),
                                token,
                                main_branch.clone(),
                                repository::BranchSwitchOptions {
                                    signature: Some(signature.to_string()),
                                    local: true,
                                    ..Default::default()
                                },
                            ))
                            .await
                            .expect("branch switch");
                        }
                        done.store(true, Ordering::Relaxed);
                    });
                }

                // Scanners: hammer `status --scan` for as long as the switcher
                // runs, collecting any errors instead of unwrapping inline.
                let mut scanners: JoinSet<Vec<String>> = JoinSet::new();
                for _ in 0..RACE_SCANNER_TASKS {
                    let repo = status_repo.clone();
                    let done = done.clone();
                    lore_spawn!(scanners, async move {
                        let mut errors: Vec<String> = Vec::new();
                        let mut scans = 0usize;
                        while !done.load(Ordering::Relaxed) && scans < RACE_MAX_SCANS_PER_TASK {
                            scans += 1;
                            let res = repository::status::status(
                                repo.clone(),
                                None,
                                repository::status::StatusOptions {
                                    staged: false,
                                    scan: true,
                                    check_dirty: false,
                                    reset: false,
                                    sync_point: false,
                                    revision_only: false,
                                    count: false,
                                },
                            )
                            .await;
                            if let Err(err) = res {
                                errors.push(format!("{err:?}"));
                            }
                        }
                        errors
                    });
                }

                switcher
                    .join_next()
                    .await
                    .expect("switcher panicked")
                    .expect("switcher task");

                let mut all_errors: Vec<String> = Vec::new();
                while let Some(joined) = scanners.join_next().await {
                    all_errors.extend(joined.expect("scanner task panicked"));
                }

                assert!(
                    all_errors.is_empty(),
                    "status --scan must not error during a concurrent branch switch \
                     delete; got {} error(s), first: {}",
                    all_errors.len(),
                    all_errors.first().map_or("", String::as_str),
                );
            }))
            .await
            .expect("Test task failed");
    }

    /// Setup state shared by both tests: a fresh on-disk repository with
    /// two seed commits, built via the production `create_local` →
    /// `load_and_connect_with_token` path. The `_tempdir` field keeps the
    /// on-disk repo alive for the duration of the test; drop it at the end
    /// and the directory is removed.
    struct SeededRepo {
        repository: Arc<RepositoryContext>,
        sig1: Hash,
        sig2: Hash,
        default_branch_id: BranchId,
        path: PathBuf,
        _tempdir: TempDir,
    }

    async fn setup_seeded_repo(repository_id: RepositoryId) -> SeededRepo {
        let tempdir = generate_tempdir();
        let path = tempdir.to_path_buf();

        let default_branch_id = BranchId::from(uuid::Uuid::now_v7());
        let write_token = repository::RepositoryWriteToken::acquire(path.as_path()).await;
        let _created = repository::create_local(
            path.as_path(),
            &write_token,
            repository_id,
            default_branch_id,
            branch::DEFAULT_DEFAULT_NAME.to_string(),
            repository::RepositoryConfig::default(),
            false,
        )
        .await
        .expect("Failed to initialize repository");

        let repository = repository::load_and_connect_with_token(
            path.as_path(),
            RepositoryAccess::ReadWrite,
            Some(write_token),
        )
        .await
        .expect("Failed to load_and_connect_with_token");

        instance::store_current_anchor_branch(&repository, default_branch_id)
            .await
            .expect("Failed to store anchor branch");

        // Two seed commits. Each `commit::commit` advances the branch
        // head, so after both calls the branch is at sig2 and the anchor
        // points at sig2 (the latest commit). Tests that want to alternate
        // the anchor between two valid signatures (the stress test) call
        // `instance::store_current_anchor(&repo, sig1)` themselves; the
        // burst test leaves the anchor at sig2 so its phase-2 real commit
        // can advance the branch normally.
        let sig1 = seed_commit(&repository, &path, 0xA5).await;
        let sig2 = seed_commit(&repository, &path, 0x5A).await;

        SeededRepo {
            repository,
            sig1,
            sig2,
            default_branch_id,
            path,
            _tempdir: tempdir,
        }
    }

    /// Run `iters` reader iterations against `repo`, each performing the
    /// realistic read sequence (`load_current_anchor` → `State::deserialize`
    /// → `state.tree` → `Metadata::deserialize`). Asserts the anchor it
    /// observes is one of two valid signatures.
    async fn run_reader_loop(
        repo: Arc<RepositoryContext>,
        iters: usize,
        sig1: Hash,
        sig2: Hash,
        default_branch_id: BranchId,
    ) -> Vec<Duration> {
        let mut samples = Vec::with_capacity(iters);
        for _ in 0..iters {
            let t = Instant::now();
            let (rev, branch) = instance::load_current_anchor(&repo)
                .await
                .expect("load_current_anchor");
            assert!(
                rev == sig1 || rev == sig2,
                "anchor revision was neither valid signature"
            );
            assert_eq!(branch, default_branch_id, "branch must be intact");

            let state = state::State::deserialize(repo.clone(), rev)
                .await
                .expect("State::deserialize");
            let _tree = state.tree(repo.clone()).await.expect("state.tree");
            let _metadata = Metadata::deserialize(repo.clone(), state.metadata_hash())
                .await
                .expect("Metadata::deserialize");
            samples.push(t.elapsed());
        }
        samples
    }

    /// Stage `SEED_FILE_COUNT` files filled with `byte` and commit them.
    /// Returns the commit signature.
    async fn seed_commit(
        repository: &Arc<RepositoryContext>,
        path: &std::path::Path,
        byte: u8,
    ) -> Hash {
        for i in 0..SEED_FILE_COUNT {
            let file_path = path.join(format!("seed_{i:02}_{byte:02x}.bin"));
            let mut f = std::fs::File::options()
                .create(true)
                .truncate(true)
                .read(true)
                .write(true)
                .open(&file_path)
                .expect("Failed to create seed file");
            f.write_all(&vec![byte; SEED_FILE_BYTES])
                .expect("Failed to write seed file");
        }
        let token = repository
            .try_write_token()
            .expect("seed_commit requires write-mode repository context");
        file::stage::stage(
            repository.clone(),
            token,
            LoreArray::from_vec(vec![LoreString::from(path)]),
            StageOptions {
                case_change: stage::StageCaseChange::Error,
                node_flags: NodeFlags::NoFlags,
                file_id: None,
                no_children: false,
                scan: true,
            },
        )
        .await
        .expect("Failed to stage seed files");
        Box::pin(commit::commit(
            repository.clone(),
            token,
            CommitOptions {
                message: String::new(),
                link_messages: std::collections::HashMap::new(),
                link: None,
                layer_messages: std::collections::HashMap::new(),
                layer: None,
            },
        ))
        .await
        .expect("Failed to commit seed revision")
    }

    /// Write `count` files named `{prefix}_{i}.bin` filled with `byte` under
    /// `path`, stage them with scan enabled, and commit. Returns the commit
    /// signature. Used to create a wide, switchable working-directory delta.
    async fn commit_n_files(
        repository: &Arc<RepositoryContext>,
        path: &std::path::Path,
        prefix: &str,
        count: usize,
        byte: u8,
    ) -> Hash {
        for i in 0..count {
            let file_path = path.join(format!("{prefix}_{i:04}.bin"));
            let mut f = std::fs::File::options()
                .create(true)
                .truncate(true)
                .read(true)
                .write(true)
                .open(&file_path)
                .expect("Failed to create race file");
            f.write_all(&vec![byte; SEED_FILE_BYTES])
                .expect("Failed to write race file");
        }
        let token = repository
            .try_write_token()
            .expect("commit_n_files requires write-mode repository context");
        file::stage::stage(
            repository.clone(),
            token,
            LoreArray::from_vec(vec![LoreString::from(path)]),
            StageOptions {
                case_change: stage::StageCaseChange::Error,
                node_flags: NodeFlags::NoFlags,
                file_id: None,
                no_children: false,
                scan: true,
            },
        )
        .await
        .expect("Failed to stage race files");
        Box::pin(commit::commit(
            repository.clone(),
            token,
            CommitOptions {
                message: String::new(),
                link_messages: std::collections::HashMap::new(),
                link: None,
                layer_messages: std::collections::HashMap::new(),
                layer: None,
            },
        ))
        .await
        .expect("Failed to commit race revision")
    }

    fn summarize(label: &str, samples: &mut [Duration]) {
        if samples.is_empty() {
            eprintln!("{label}: no samples");
            return;
        }
        samples.sort();
        let n = samples.len();
        let min = samples[0];
        let median = samples[n / 2];
        let p95 = samples[((n * 95) / 100).min(n - 1)];
        let max = samples[n - 1];
        let total: Duration = samples.iter().sum();
        let mean = total / n as u32;
        eprintln!(
            "{label}: n={n} min={min:?} median={median:?} p95={p95:?} max={max:?} mean={mean:?}"
        );
    }
}
