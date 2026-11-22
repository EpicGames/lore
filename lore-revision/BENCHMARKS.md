# lore-revision benchmarks

The measurements behind the bounds and threads of the walk in `lore-revision/src/fs/walk/walker.rs`, which clones, copies, measures and searches a directory tree.

## The walk's bounds

`lore-revision/examples/walk.rs` times `lore_revision::fs::walk::clone_into` copying four trees with `Backend::Copy`. It builds only with the `walk-bench` feature, as `cargo build --release -p lore-revision --example walk --features walk-bench`, and `walk prepare` writes the trees under `LORE_BENCH_DIR`:

| Tree | What it holds |
| --- | --- |
| `deep` | 8,191 directories, 12 levels deep, with a 4 KiB file in each |
| `wide` | The root and 4 directories of 25,000 files of 1 KiB each |
| `flat` | The root and 1,000 directories of 64 files of 2 KiB each |
| `large` | 8 files of 256 MiB |

The example links `lore-base`, so it allocates through rpmalloc, the global allocator `lore` runs on; `LORE_ALLOCATOR=system` selects the system allocator, glibc malloc on Linux. The figures in this section were measured on the system allocator.

`walk run clone` copies each tree once with `clone_into`, and `walk run reference` with a walk on one thread through `std::fs::copy`, which is the same code in every build and is the run's noise floor. `LORE_BENCH_WALK_BOUNDS=<queue>,<continuations>,<batch>` sets the walk's bounds. Each copy goes to a new directory beside its tree, removed once it's timed. `lore-io/examples/build-ab.sh` compares two builds of the example, with `MODE=run ENGINES="clone reference"`.

### Linux

Host E: Intel Xeon Platinum 8559C, 192 threads, 1.5 TiB, xfs with reflink, kernel 6.8.0-1067-aws, measured 2026-11-19. The syscall pool's cap is 16, so a walk runs on the calling thread and up to 8 lent. The host is shared: its load average stood between 70 and 90 during the runs. Every build and bound ran in each round, the order rotated and reversed every other round, and each cell is the median of 6 rounds in files per second, with the range in thousands.

| Build | `deep` | `wide` | `flat` | `large` |
| --- | --- | --- | --- | --- |
| Unbounded walk, before the bounds | 42,730 [38–61k] | 55,976 [47–64k] | 85,628 [64–97k] | 6,736 [5–12k] |
| Queue 4,096, batch 256 | 48,633 [39–57k] | 56,082 [47–64k] | 88,320 [60–100k] | 6,608 [3–11k] |
| Queue 16,384, batch 1,024 | 53,322 [38–65k] | 50,070 [43–54k] | 84,618 [66–100k] | 8,822 [8–12k] |
| Queue 2,048, batch 1,024 | 38,380 [11–43k] | 53,614 [47–61k] | 86,162 [66–96k] | 9,192 [2–12k] |
| Queue 65,536, batch 4,096 | 45,569 [31–64k] | 54,247 [50–67k] | 81,838 [58–90k] | 7,564 [6–12k] |
| `reference` | 23,018 [20–24k] | 36,124 [32–39k] | 39,214 [33–42k] | 29,682 [20–37k] |

The walk takes a queue of 4,096 and a batch of 256: no bound tried is faster on any tree, and with these the copy is no slower than the unbounded walk on any of them. On xfs with reflink `copy_file_range` shares the large files' blocks, so `large` measures the cost of a file, about 1 ms per copy, rather than a copy of its bytes.

`walk run` also times `copy_faithfully`: `faithful` on the threads the syscall pool lends, at most half its cap, `faithful-alone` on the calling thread alone, and `faithful-all` on as many threads as the pool's whole cap, through a pool of twice the cap. In a later sitting on Host E, with the walk's bounds, median of 6 rounds in files per second:

| Build | `deep` | `wide` | `flat` | `large` |
| --- | --- | --- | --- | --- |
| Cloning mode, before the bounds | 46,094 [41–68k] | 59,144 [47–64k] | 89,804 [61–102k] | 8,772 [5–14k] |
| Cloning mode | 55,984 [48–67k] | 58,642 [50–66k] | 83,988 [62–98k] | 7,130 [6–12k] |
| `faithful-alone` | 14,244 [11–15k] | 27,052 [20–28k] | 26,706 [21–29k] | 65 [under 1k] |
| `faithful`, half the cap | 51,428 [31–53k] | 49,053 [41–57k] | 77,166 [57–89k] | 65 [0–2k] |
| `faithful-all`, the whole cap | 42,128 [32–52k] | 43,491 [42–55k] | 70,322 [53–73k] | 65 [under 1k] |
| `reference` | 23,090 [20–25k] | 36,039 [34–39k] | 35,734 [27–38k] | 31,459 [6–38k] |

The faithful copy takes half the cap, which is faster than the whole cap on every tree but `large`, where they tie. On `large` it copies each file's ranges of data as the file system maps them and then flushes the file system, which takes about 0.12 s for the 2 GiB the cloning mode shares in milliseconds. `flat`'s cloning-mode median sits below the unbounded walk's here; 40 rounds of `flat` alone, interleaved, measured 68,082 [45–102k] files per second for the cloning mode against 65,110 [48–94k] for the unbounded walk.

### Across volumes

`LORE_BENCH_COPY_DIR` puts the copies in another directory, here on another file system: the trees on xfs, the copies on the host's overlay root, so `copy_file_range` can't share blocks and each file's bytes are copied. On Host E, a later sitting, every build and bound in each round, the order rotated and reversed every other round, median of 10 rounds, in files per second, and in MiB per second for `large`, with the range:

| Build | `deep` | `wide` | `flat` | `large` |
| --- | --- | --- | --- | --- |
| Unbounded walk, before the bounds | 61,160 [38–76k] | 53,336 [40–57k] | 124,864 [76–145k] | 1,852 [1,223–2,161] MiB/s |
| Queue 4,096, batch 256 | 59,147 [42–80k] | 54,798 [36–69k] | 125,277 [37–148k] | 2,036 [1,819–2,263] MiB/s |
| `reference` | 16,696 [14–22k] | 28,888 [22–37k] | 32,085 [27–34k] | 2,046 [1,814–2,138] MiB/s |

The cloning mode is no slower than the unbounded walk across volumes either. `faithful`, half the cap, measured 13,786 files per second on `deep`, 16,124 on `wide` and 20,100 on `flat` in an earlier sitting of 8 rounds under heavier load, where the cloning mode measured 52,890, 43,168 and 94,970. Its `syncfs` of the target's file system after the last file, which the move needs before it places the copy, takes most of that: three runs of `flat` took 3.4 to 3.8 s with it and 1.1 to 1.7 s without it.

A walk that handed a wide directory to other threads while its batch was still being copied measured 32,668 files per second on `wide` against the unbounded walk's 49,823, in a sitting of its own: threads that create entries in one directory at once wait for each other on its lock. A continuation goes back on the queue only once its batch is done.

The queue holds at most 64 continuations, each with its directory's one descriptor, and with that many queued a thread lists the rest of its directory itself. In a later sitting on Host E, under a load average in the thousands, 20 rounds measured the cloning mode at 31,866 [17–45k] files per second on `deep`, 33,946 [20–45k] on `wide` and 58,098 [42–72k] on `flat`, against the unbounded walk's 31,402 [18–55k], 34,940 [29–47k] and 49,863 [37–77k]; 30 rounds of `wide` alone measured 26,452 [3–38k] against 26,924 [3–36k].

### Windows

Host W: a Windows Server 2025 Datacenter container that reports 64 logical processors, NTFS, measured 2026-11-19. The syscall pool's cap is 16, so a walk runs on the calling thread and up to 8 lent. The copy backend copies each file with `CopyFileExW`. Every build and variant ran in each round, the order reversed every other round, and each cell is the median of 4 rounds in files per second, with the range; `large` is in MiB per second, as 8 files make files per second meaningless.

| Build | `deep` | `wide` | `flat` | `large`, MiB/s |
| --- | --- | --- | --- | --- |
| Cloning mode, before the bounds | 3,676 [3.6–3.8k] | 3,466 [3.3–3.6k] | 4,625 [4.1–4.8k] | 698 [264–1,661] |
| Cloning mode | 3,702 [3.4–3.8k] | 4,824 [4.6–5.1k] | 4,226 [4.0–4.7k] | 764 [701–1,780] |
| `faithful-alone` | 774 [549–821] | 1,058 [1.0–1.1k] | 1,034 [1.0–1.1k] | 547 [388–770] |
| `faithful`, half the cap | 2,372 [1.5–2.6k] | 3,066 [3.0–3.1k] | 2,838 [2.6–3.1k] | 633 [222–920] |
| `faithful-all`, the whole cap | 2,462 [2.4–2.6k] | 3,026 [2.8–3.3k] | 3,456 [2.9–3.7k] | 407 [109–830] |
| `reference` | 1,666 [1.2–1.7k] | 2,134 [2.0–2.3k] | 2,168 [2.0–2.3k] | 878 [230–1,109] |

The cloning mode is no slower than before the bounds on any tree, and faster on `wide`, whose 25,000-entry directories are listed a batch at a time. Its `flat` median sits below the unbounded walk's here; 12 rounds of `flat` alone, interleaved, in a later and busier sitting, measured 2,554 [1.1–4.6k] files per second for the cloning mode against 2,518 [0.9–4.8k] for the unbounded walk, with the cloning mode ahead in 8 rounds and a median ratio of 1.02 between the two in a round, while `reference` measured 976 [0.7–1.8k].

The faithful copy costs about a third of the cloning mode's rate on the trees of small files: each file takes `BackupRead` and `BackupWrite`, its security, attributes and times, and a `FlushFileBuffers` before its handle closes. Half the cap and the whole cap are within each other's range on `deep` and `wide`; the whole cap is ahead on `flat` and behind on `large`, so the faithful copy keeps half the cap on Windows as on Linux.
