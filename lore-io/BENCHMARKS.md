# lore-io benchmarks

`lore-io/examples/bench.rs` compares four ways of doing file I/O from async Rust on identical
workloads: what does routing an operation through the dedicated syscall pool cost or save against
the alternatives. The fourth engine is whichever completion backend the platform has — `uring` on
Linux, `iocp` on Windows — and elsewhere there are three.

## What it compares

| Engine | What it does |
| --- | --- |
| `blocking` | `tokio::task::spawn_blocking` around a positional syscall (`pread`/`pwrite` on Unix, `seek_read`/`seek_write` on Windows), or `std::fs::read` for whole files. This is what `lore_spawn_blocking!` plus `FileExt::read_at` does in `lore-storage/src/chunker.rs` today — the strongest form of the current path. |
| `tokiofs` | `tokio::fs`. Reads and writes seek then transfer; whole-file work uses `tokio::fs::read`/`write`. |
| `loreio` | The `psync` backend: the same syscalls issued through `IoDriver` onto the bounded `lore-io` syscall pool. |
| `uring` | The `io_uring` backend, through the same `IoDriver` API. Linux only, and skipped with a printed reason where a ring cannot be created. |
| `iocp` | The completion-port backend, through the same `IoDriver` API. Windows only. Positional reads and writes are issued on the calling thread and completed by one reaper draining the port; everything else runs on the syscall pool, as it does on the ring. |

`tokio::fs::File` is `AsyncSeek` + `AsyncRead`, so there is no positional read: an offset is reached
by seeking, and the file offset belongs to the file description. `try_clone` is a `dup`, which
shares that offset, so concurrent reads at different offsets cannot share a handle and each must
open the file. That open is structural, not a benchmark artefact, and it is what the `tokiofs`
numbers below measure.

Every engine allocates a fresh buffer per read, on the thread that receives the copy.

## Workloads

| Workload | Operation | Size | Ops | Concurrency |
| --- | --- | --- | --- | --- |
| `read-64KiB-warm-c64` | Random positional reads over 256 MiB, spread across 64 files | 64 KiB | 1,572,864 | 64 |
| `read-4KiB-warm-c128` | Random positional reads over 256 MiB, spread across 64 files | 4 KiB | 6,291,456 | 128 |
| `write-256KiB-seq-c32+sync` | Sequential positional writes, then one `sync_data` | 256 KiB | 16,384 | 32 |
| `small-files-4KiB-wr+rd-c64` | Whole-file create then whole-file read, 65,536 files | 4 KiB | 131,072 | 64 |
| `commit-read-mixed-scattered` | Scattered reads over a heterogeneous set of 2048 files; whole-file below 32 KiB | 32–256 KiB, normal | ~410,000 | 20×4 |
| `sync-write-mixed-scattered` | The same set and shape, written rather than read | 32–256 KiB, normal | ~410,000 | 20×4 |
| `read-64KiB-cold-c64` | Permuted reads over one file, each block read once | 64 KiB | derived | 64 |
| `read-4KiB-cold-rec-c128` | Permuted reads over one file, one block per 128 KiB region | 4 KiB | derived | 128 |
| `small-files-4KiB-rd-cold-c64` | Whole-file reads of 16,384 evicted files | 4 KiB | 16,384 | 64 |

The two mixed phases are the repository-shaped ones and the most representative of real work: a
commit reads a working tree of highly uneven file sizes from every core at once, and a sync writes
one. Their file sizes are 60% below 32 KiB drawn log-uniformly from 64 bytes — handled whole, in
one operation, as the storage layer handles a small object — then 30% to 1 MiB, 9% to 8 MiB and 1%
to 32 MiB. Larger
files are reached at scattered offsets in chunks drawn from a normal distribution clamped to
32–256 KiB, which is how a fragment store reaches its contents. The operation list is generated
from fixed seeds and shuffled, so every engine does identical work in identical order. Concurrency
is one driver task per core, at least 16, with four operations in flight each.

Warm phases are sized to run about a second each; a phase of tens of milliseconds measures
frequency ramp and cache state instead. Cold phase sizes are derived rather than fixed, because a
cold phase reads each block once and its duration is therefore the data set size divided by what
the device gives: `prepare-cold` measures that rate and sizes both large-file phases for about a
second, recording the result in a manifest every engine then reads. The harness warns when a phase
falls below 250 ms, and again when one reads faster than the rate that sized it, which means it was
not cold.

The cold phases read a single file each and ignore `LORE_BENCH_READ_SPREAD`, which applies to the
warm read phases only. Spreading a cold phase would read copies made from the data file, and those
are in cache as soon as they exist.

The 4 KiB cold reads are strided 128 KiB apart because ZFS caches whole records: a second 4 KiB
read inside a record already in the ARC is a cache hit. The spacing also defeats readahead on
page-cache filesystems.

Each engine writes its own warm data file immediately before reading it. Output columns are `ops`,
`MiB`, `secs`, `ops/s`, `MiB/s`; each `loreio` read phase also prints a `pool_stats()` line with
live threads against the cap and the thread and queue high-water marks.

## Running the benchmarks

| Mode | Effect |
| --- | --- |
| `warm` | All six warm phases. No engine argument runs every engine, one child process each. |
| `read` | The two read phases only, for one engine. What to run under `perf`. |
| `prepare-cold` | Checks eviction, measures the device, sizes the cold phases from it, and writes the data sets and their manifest, clearing any previous ones. Bounded by `LORE_BENCH_COLD_BUDGET_GIB`. |
| `cold` | The cold phases, at the sizes the manifest records. No engine argument runs every engine, one process each. |
| `warm-suite` / `cold-suite` | That suite for N rounds (default 6), engine order alternated, then a median, a range and a position control. `cold-suite` also rotates which data set each engine reads and runs `cold-baseline`. Give it a round count divisible by the engine count or the rotation is unbalanced, which it reports. |
| `cold-baseline` | Each engine's cold data set read by plain threads, one per in-flight request with a handle each, no engine in the path. |
| `check-eviction` | Whether `evict_from_page_cache` works on this filesystem, without preparing anything. Exits non-zero when it does not. Run it before trusting a cold sitting. |

```sh
# Warm, the full protocol
cargo run --release -p lore-io --example bench -- warm-suite 6

# Cold, the full protocol
cargo run --release -p lore-io --example bench -- prepare-cold
sudo sh -c 'sync; echo 3 > /proc/sys/vm/drop_caches'   # ZFS only
cargo run --release -p lore-io --example bench -- cold-suite 6
```

Use the suite modes for anything a conclusion rests on. A single round cannot separate an engine
difference from where the round sat; *Experiments* records what that cost.

One process per engine is deliberate. Page cache, thread pools and allocator state carry between
engines in a shared process, which measured a 3× difference. Run a single engine when profiling.

### Cache eviction by platform

| Platform | Mechanism | Cache drop needed |
| --- | --- | --- |
| ext4, xfs | `posix_fadvise(POSIX_FADV_DONTNEED)` | No — verified within 1–2% on eight of nine cells against a `drop_caches` run |
| ZFS | none; the ARC ignores `posix_fadvise` | Yes, `drop_caches` as root |
| macOS | `msync(MS_INVALIDATE)` over a mapping of the file | No |
| Windows | open with `FILE_FLAG_NO_BUFFERING`, which flushes and purges the file's section | No, where it works — see below |

The macOS and Windows mechanisms were verified rather than assumed; see *Experiments*. Neither
needs privilege. On Windows there is also no `sync` between children, because `FlushFileBuffers`
on a volume handle wants administrator rights; the settle still runs.

The Windows mechanism is per-file and not every volume honours it: of the two measured on Host C's
machine, the 8 TB SATA volume evicts and the 4 TB one does not, on the same OS and filesystem
type. `check-eviction` reports which, and `prepare-cold` runs it before sizing anything, because a
probe against a file that was not evicted measures cache and would size every phase from it. Run it
on a volume before trusting a cold sitting there.

### Variables and scripts

| Variable | Effect |
| --- | --- |
| `LORE_BENCH_DIR` | Directory for benchmark files. Defaults to the system temp directory. Set it to put the data on the filesystem under test. |
| `LORE_BENCH_ALLOC_OUTSIDE` | Moves the `blocking` engine's buffer allocation from its closure to the calling task, isolating which thread allocates. |
| `LORE_BENCH_BLOCKING_THREADS` | The `blocking` engine's tokio pool, overriding `min(2 × (cores + 1), 128)`. The baseline's counterpart to `LORE_IO_POOL_THREADS`, which makes a cap sweep's control runnable. |
| `LORE_BENCH_DRIVERS` | Long-lived driver tasks for the two synthetic read phases, so submission runs on many worker threads rather than the one driving `buffer_unordered`. Default 16. At one it measures the driver rather than the backend, for any engine that completes on the submitting thread. |
| `LORE_BENCH_READ_SPREAD` | Files the two synthetic *warm* read phases spread over, each holding `1/N` of the data so the working set does not change with the count. Default 64, the larger phase's concurrency. At one it measures `io_uring`'s per-inode serialization and little else. The cold phases always read one file; see *Harness defects*. |
| `LORE_IO_BACKEND` | Backend the driver selects (`auto`, `psync`, `uring` on Linux, `iocp` on Windows). `auto` selects `psync` on every platform, so this is how a completion backend is chosen outside the benchmark. The benchmark selects backends by engine tag instead, so this only matters when profiling something else in the process. |
| `LORE_IO_POOL_THREADS` | Syscall pool cap, overriding `min(2 × cores, 16)`. Rejects zero and anything above 128. |
| `LORE_IO_IOCP_REAPERS` | Threads draining the Windows completion port, overriding the default of one. Accepts 1 to 64. For re-running the sweep below on another machine, not for sizing a deployment. |
| `LORE_BENCH_COLD_BUDGET_GIB` | Total disk the cold data sets may occupy across every engine and both large-file phases. Default 32. `prepare-cold` sizes each phase to run for about a second at the rate it measures, and this caps the result; where the cap binds, the projected phase duration is reported and warned about if it falls under the 0.25 s floor. |
| `LORE_ALLOCATOR` | Read by `lore-base`, which the benchmark links for its global allocator. `system` swaps rpmalloc for `std::alloc::System`. |

| Script | What it runs |
| --- | --- |
| `examples/pool-sweep.sh <warm\|cold> [passes] [caps]` | The pool cap sweep: one cap per process, cap order reversed on even passes, median per workload with ratios against 32. `BASELINE_CAP` moves that reference. |
| `examples/build-ab.sh <rounds> <binary-a> <binary-b>` | An A/B of two *builds*, which the suite modes cannot vary: both binaries interleaved in one sitting, order alternated, with an engine whose code is identical in both as the noise floor. |
| `examples/handle-fanout.rs` | Diagnostic, not a benchmark: spreads one workload over a varying number of handles to the same file, separating handle contention from dispatch cost. |

## Hosts

| | Host A | Host B | Host C | Host D |
| --- | --- | --- | --- | --- |
| OS | Linux | Linux | Windows 11 Pro 26100 | macOS 26.5.2 (25F84) |
| CPU | 20 cores | AMD Threadripper PRO 3995WX, 64 cores / 128 threads | same as Host B | Apple M2, 8 cores — 4 performance + 4 efficiency |
| RAM | 30 GiB | 251 GiB | 256 GiB | 16 GiB |
| Filesystem | ZFS (`rpool/tmp`) | ext4, Samsung 870 QVO 2 TB SATA SSD | NTFS, Samsung 870 QVO 8 TB SATA SSD | APFS, internal NVMe |
| Kernel | 7.0.0-28-generic | 6.8.0-136-generic | 10.0.26100.8893 | Darwin 25.5.0, xnu-12377.121.10 |
| Syscall pool | 32 | 32 | 32 | 16 |
| `blocking` tokio pool | 42 | 128 | 128 | 18 |
| Allocator | glibc malloc | rpmalloc | rpmalloc | rpmalloc |
| Protocol | median of 4 rounds | median of 6, order alternated | median of 6, order alternated | median of 6, order alternated |

All release builds, all measured 2026-08-01.

Host C is Host B's machine running Windows: CPU and RAM fixed, OS and filesystem the variable. The
drive is not fixed — the 2 TB device holds the Linux install and is not mounted under Windows — so
compare the two hosts' read ratios and not their write rows.

Host D is the only host measured at the shipping pool default: `min(2 × cores, 16)` yields exactly
16 there, against 32 for the other three. It is also the only host whose cores are not
interchangeable.

Three caveats on absolute figures:

- Host A predates the benchmark linking `lore-base`, so it ran on glibc malloc while the design
  under test assumes rpmalloc's per-thread span cache. Read Host A's ratios, not its ops/s.
- Host C runs CrowdStrike `csagent.sys`, which every file operation traverses. The build tree is
  excluded from scanning by policy and everything was run from inside that exclusion. The size of
  that term is not established: probes from outside measured 66% lower on open-bound work and
  0–20% lower on read-bound work, but were not interleaved, and unpaired runs on this host drift
  1.23–1.36×.
- Host D runs CrowdStrike Falcon 7.39 as an Endpoint Security extension, with no exclusion
  boundary in play — an open costs 12.8 µs repeated on one file and 15.5 µs across 4096 distinct
  ones, and the build tree and benchmark directory measure the same. All three engines pay it
  equally, so ratios stand; open-bound absolutes are this environment's rather than APFS's.

Governor: Hosts A and B pinned to `performance`, Host C on `High performance` with processor
minimum state 100%. macOS exposes no governor, so Host D ran on AC power with Low Power Mode off
under `caffeinate -i`, with `pmset -g therm` reporting no thermal or CPU-power warning.

## Results

### Warm suites

| Workload | `blocking` | `tokiofs` | `loreio` | vs `blocking` | vs `tokiofs` |
| --- | --- | --- | --- | --- | --- |
| **Host A**, median of 4 | | | | | |
| `read-64KiB-warm-c64` | 558,790 | 116,090 | 613,207 | 1.10× | 5.28× |
| `read-4KiB-warm-c128` | 369,336 | 174,798 | 364,958 | 0.99× | 2.09× |
| `write-256KiB-seq-c32+sync` | 39,386 | 37,330 | 42,881 | 1.09× | 1.15× |
| `small-files-4KiB-wr+rd-c64` | 111,904 | 115,432 | 114,446 | 1.02× | 0.99× |
| **Host B**, median of 6 | | | | | |
| `read-64KiB-warm-c64` | 319,734 | 61,834 | 371,042 | 1.16× | 6.00× |
| `read-4KiB-warm-c128` | 332,214 | 92,860 | 389,104 | 1.17× | 4.19× |
| `write-256KiB-seq-c32+sync` | 552 | 548 | 544 | 0.99× | 0.99× |
| `small-files-4KiB-wr+rd-c64` | 58,182 | 57,135 | 67,137 | 1.15× | 1.18× |
| **Host C**, median of 6 | | | | | |
| `read-64KiB-warm-c64` | 28,308 | 14,950 | 81,290 | 2.87× | 5.44× |
| `read-4KiB-warm-c128` | 57,978 | 17,945 | 141,799 | 2.45× | 7.90× |
| `write-256KiB-seq-c32+sync` | 1,280 | 1,161 | 1,458 | 1.14× | 1.26× |
| `small-files-4KiB-wr+rd-c64` | 9,412 | 9,380 | 9,621 | 1.02× | 1.03× |
| **Host D**, median of 6 | | | | | |
| `read-64KiB-warm-c64` | 288,128 | 58,166 | 284,888 | 0.99× | 4.90× |
| `read-4KiB-warm-c128` | 407,851 | 75,738 | 483,532 | 1.19× | 6.38× |
| `write-256KiB-seq-c32+sync` | 5,775 | 5,664 | 5,801 | 1.00× | 1.02× |
| `small-files-4KiB-wr+rd-c64` | 29,618 | 29,356 | 29,760 | 1.00× | 1.01× |

Round-to-round spread, which is what says whether a difference is readable:

| Workload | `blocking` | `tokiofs` | `loreio` |
| --- | --- | --- | --- |
| **Host A** | | | |
| `read-64KiB-warm-c64` | 556k–572k | 114k–117k | 603k–624k |
| `read-4KiB-warm-c128` | 293k–823k | 172k–177k | 328k–455k |
| `write-256KiB-seq-c32+sync` | 36.1k–41.3k | 33.6k–39.2k | 40.8k–44.4k |
| `small-files-4KiB-wr+rd-c64` | 109k–117k | 114k–118k | 114k–117k |
| **Host B** | | | |
| `read-64KiB-warm-c64` | 316k–321k | 61.4k–62.6k | 361k–377k |
| `read-4KiB-warm-c128` | 324k–339k | 92.1k–93.5k | 383k–396k |
| `write-256KiB-seq-c32+sync` | 545–1329 | 542–636 | 538–550 |
| `small-files-4KiB-wr+rd-c64` | 56.9k–60.7k | 56.5k–58.4k | 67.0k–67.5k |
| **Host C** | | | |
| `read-64KiB-warm-c64` | 27.8k–29.3k | 14.8k–15.7k | 80.1k–84.1k |
| `read-4KiB-warm-c128` | 56.3k–59.4k | 17.3k–18.6k | 139k–145k |
| `write-256KiB-seq-c32+sync` | 1276–1450 | 1144–1162 | 1416–1538 |
| `small-files-4KiB-wr+rd-c64` | 9214–9611 | 8959–9557 | 9186–10403 |
| **Host D** | | | |
| `read-64KiB-warm-c64` | 283k–293k | 58.0k–58.3k | 266k–288k |
| `read-4KiB-warm-c128` | 395k–446k | 75.6k–76.0k | 459k–499k |
| `write-256KiB-seq-c32+sync` | 5701–5866 | 3684–5975 | 5593–5910 |
| `small-files-4KiB-wr+rd-c64` | 28.6k–29.9k | 26.4k–30.0k | 27.3k–29.9k |

Position controls — reverse-order median over forward-order median — are 0.98×–1.02× on Host B,
0.95×–1.03× on Host C and 0.99×–1.03× on Host D, except `tokiofs` writes at 0.87× on Host B and
`blocking` writes at 0.91× on Host C, both the SLC artefact described under *Experiments*.

### Cold suites

| Workload | `blocking` | `tokiofs` | `loreio` | vs `blocking` | vs `tokiofs` |
| --- | --- | --- | --- | --- | --- |
| **Host A**, one run | | | | | |
| `read-64KiB-cold-c64` | 65,989 | 44,042 | 67,572 | 1.02× | 1.53× |
| `read-4KiB-cold-rec-c128` | 40,434 | 46,487 | 45,868 | 1.13× | 0.99× |
| `small-files-4KiB-rd-cold-c64` | 180,024 | 192,032 | 191,535 | 1.06× | 1.00× |
| **Host B**, median of 6 | | | | | |
| `read-64KiB-cold-c64` | 6,787 | 7,951 | 7,984 | 1.18× | 1.00× |
| `read-4KiB-cold-rec-c128` | 22,511 | 18,082 | 21,296 | 0.95× | 1.18× |
| `small-files-4KiB-rd-cold-c64` | 73,982 | 76,994 | 56,373 | 0.76× | 0.73× |
| **Host C**, median of 6 | | | | | |
| `read-64KiB-cold-c64` | 2,576 | 4,580 | 7,794 | 3.03× | 1.70× |
| `read-4KiB-cold-rec-c128` | 5,077 | 8,782 | 19,667 | 3.87× | 2.24× |
| `small-files-4KiB-rd-cold-c64` | 55,264 | 55,404 | 54,800 | 0.99× | 0.99× |
| **Host D**, median of 6, rotated | | | | | |
| `read-64KiB-cold-c64` | 39,058 | 16,397 | 38,652 | 0.99× | 2.36× |
| `read-4KiB-cold-rec-c128` | 38,476 | 21,267 | 38,650 | 1.00× | 1.82× |
| `small-files-4KiB-rd-cold-c64` | 44,356 | 45,076 | 45,834 | 1.03× | 1.02× |

Host C's spreads: 2559–2593, 4254–4776 and 7778–7821 on the 64 KiB phase; 4781–5237, 8119–8900 and
19.0k–20.3k on the 4 KiB phase; 55.2k–55.7k, 55.1k–55.6k and 54.7k–55.0k on the small files.
Position control 0.98×–1.03× throughout. These are the tightest ranges in this file; the device
sets them.

Host D's cold ranges are wide by construction — the rotation makes each engine read all three data
sets, so a range spans the copies as well as the noise, and the median rather than the range is
what carries. `blocking` spans 36.5k–46.2k and `loreio` 36.2k–45.0k on the 64 KiB phase. Position
control is 0.99×–1.08× except the small-file row at 1.05×–1.14×.

On Host D each engine's result is also divided by what plain threads get from the same data sets,
which measures distance to the hardware rather than to another engine:

| Workload | `blocking` | `tokiofs` | `loreio` | spread across copies |
| --- | --- | --- | --- | --- |
| `read-64KiB-cold-c64` | 0.96 | 0.40 | 0.95 | 1.21× |
| `read-4KiB-cold-rec-c128` | 0.86 | 0.48 | 0.87 | 1.49× |
| `small-files-4KiB-rd-cold-c64` | 0.80 | 0.82 | 0.83 | 1.32× |

### Four engines, both filesystems

Host A, `warm-suite 6`: six rounds, a discarded warm-up round first, engine order alternated, one
process per engine, medians. The synthetic read phases spread over 64 files and are driven by 16
tasks — the same shape for every engine — because a single-file, single-driver phase measures the
harness rather than the backend.

**ext4** (loopback image):

| Workload | `blocking` | `tokiofs` | `loreio` | `uring` | uring / loreio |
| --- | --- | --- | --- | --- | --- |
| `read-64KiB-warm-c64` | 547,310 | 172,640 | 994,288 | **1,213,316** | 1.22× |
| `read-4KiB-warm-c128` | 609,968 | 192,534 | 791,862 | **5,163,049** | 6.52× |
| `write-256KiB-seq-c32+sync` | 14,112 | 13,778 | 14,420 | **16,850** | 1.17× |
| `small-files-4KiB-wr+rd-c64` | 160,164 | 159,062 | 162,376 | **166,414** | 1.02× |
| `commit-read-mixed-scattered` | 226,394 | 112,448 | 261,949 | **418,090** | 1.60× |
| `sync-write-mixed-scattered` | 180,341 | 111,160 | **274,479** | 192,714 | 0.70× |

**ZFS**:

| Workload | `blocking` | `tokiofs` | `loreio` | `uring` | uring / loreio |
| --- | --- | --- | --- | --- | --- |
| `read-64KiB-warm-c64` | 518,554 | 160,040 | **917,846** | 457,970 | 0.50× |
| `read-4KiB-warm-c128` | 568,566 | 174,082 | 668,450 | **952,172** | 1.42× |
| `write-256KiB-seq-c32+sync` | **52,022** | 39,482 | 49,231 | 28,046 | 0.57× |
| `small-files-4KiB-wr+rd-c64` | 115,190 | 117,688 | 125,071 | **127,416** | 1.02× |
| `commit-read-mixed-scattered` | 107,250 | 69,292 | **119,967** | 107,838 | 0.90× |
| `sync-write-mixed-scattered` | 81,624 | 61,749 | **75,761** | 66,824 | 0.88× |

Position control is 0.97–1.04 throughout except ZFS's `uring` 4 KiB reads at 0.89, which also has
the widest range in either table (822,641–1,271,400) and is the one row not to quote closely.
Everything else sits within a few percent of its median.

**The repository-shaped phases are the most representative, and they run opposite to the synthetic
ones.** `uring` measures 1.60× on commit-shaped reads on ext4 and is behind on sync-shaped writes on
both filesystems, 0.70× and 0.88×; on ZFS it is behind on both phases. The synthetic 6.52× at 4 KiB
does not carry over to a repository-shaped workload: opens, whole-file operations on small objects
and a spread of sizes reduce a dispatch advantage to approximately 1.6× at best, and to a deficit on
writes.

**Scattered writes are the ring's weakest workload, consistently.** It is behind at every size and
on every filesystem measured here, while ahead on the sequential 256 KiB write phase on ext4.

## Conclusions

**`lore-io` matches or beats the path it replaces on every warm phase, on all four hosts**, while
running fewer threads than the pool it replaces — 32 against 42 on Host A, 32 against 128 on
Host B, 16 against 18 on Host D. Host A: 1.10× on 64 KiB reads, 1.09× on writes, 1.02× on
whole-file work, parity at 4 KiB. Host B: 1.16×, 1.17× and 1.15×. Host D: 1.19× at 4 KiB and
parity elsewhere.

**The `uring` backend's advantage depends on the filesystem and on the workload shape.** It
measures 1.60× the syscall pool on commit-shaped reads on ext4 and 0.70× to 0.90× on the other
three repository-shaped cases, so the case for it rests on read-heavy work on a filesystem that
completes cached reads during the submit syscall. Where completion is inline the synthetic margins
reach 6.5× at 4 KiB; where the filesystem punts to a kernel worker, as ZFS does, no configuration
recovers them. Scattered writes are behind on every filesystem measured.

**The `iocp` backend's advantage is primarily in thread count.** It runs two threads against a
syscall pool saturated at thirty-two and measures 1.76× and 4.80× on the synthetic warm read
phases, because a positional operation on Windows defers and `psync` holds a thread across the
wait. It measures 0.72× on the commit-shaped read phase, where the driver-task count exceeds the
point at which this backend peaks. Cold, it takes the 4 KiB phase by 2.27× and ties the 64 KiB
phase, where both backends saturate the device. See *Windows: the completion-port backend*.

**`auto` selects `psync` on every platform, against everything above.** The phases here are
synthetic, and the smoke suite is not: driving the real call sites end to end, it measured a
regression under the completion backends and recovered under `psync`. A whole-workload result
outranks a per-operation one, so the completion backends became opt-in. The disagreement between the
two measurements is unexplained and nothing here explains it — these numbers say what the mechanism
can do per operation, not what a repository operation costs.

**No single warm phase wins on every host.** The 64 KiB read is the best result on Hosts A, B and
C — 1.10×, 1.16×, 2.87× — and 0.99× on Host D with overlapping ranges. The 4 KiB read runs the
other way: unreadable on Host A, 1.17× on Host B, and Host D's only readable win at 1.19×, where
395k–446k against 459k–499k does not overlap over six rounds. What generalizes is the direction,
not the per-phase margin.

**`tokio::fs` is the outlier, and the penalty tracks the cost of an `open` rather than the size of
the machine.** 5.3× behind on 64 KiB reads on Host A, 6.0× on Host B, 5.4× on Host C, 4.9× on
Host D — one `open` per concurrent operation. Host D has 8 cores against Host B's 128 threads and
posts the same 4.9–6.4×, because an open there costs 12.8–15.5 µs. Core count sets how many opens
collide; what they cost is the filesystem and whatever filters the platform puts in front of it,
and that term dominates on both non-Linux hosts. `tokio::fs` reaches parity only on the whole-file
phase, where there is one open regardless.

**Cold, the engines converge**, on Hosts A and D, which is the expected result and a check that
the warm figures are not a harness artefact: when a read waits on the device, what dispatched it
stops mattering. On Host D `blocking` and `loreio` land within 0.03 of each other against what
plain threads extract from the same data, on all three phases — 0.95 and 0.96 on the 64 KiB reads
— so the engine is not the bottleneck there, the device is. `tokio::fs` closes most of its gap for
the same reason.

**Cold engine ratios are not interpretable without a per-file control.** Copies of the cold data
set written seconds apart by the same code, with byte-identical contents, read back up to 1.78×
apart under concurrent random access, because of where the drive mapped their blocks. A fixed
assignment of engine to copy is invisible to round alternation and to the position control.
`cold-suite` rotates the assignment and reports the spread; see *Experiments*.

**Host B's write row measures the SSD, not the engines.** All three land within 2% of each other
at about 137 MiB/s, the sustained QLC write speed of an 870 QVO once its SLC cache is exhausted.
The phase cannot distinguish engines on a device this close to saturation, but it is still worth
running as a check that nothing pathological happens under `sync_data`.

**Host B's cold small-file phase is where `lore-io` loses, at 0.76×, and the pool cap is why.**
The deficit disappears at 64 threads, which is the workload's own concurrency:

| Pool cap | 8 | 16 | 24 | 32 | 48 | 64 |
| --- | --- | --- | --- | --- | --- | --- |
| `small-files-4KiB-rd-cold-c64` | 56,374 | 56,271 | 56,325 | 56,429 | 68,432 | 75,720 |

`blocking` and `tokiofs` never meet this ceiling because they dispatch onto the tokio blocking
pool, 128 threads on that host. `pool_stats()` reports the same thing directly: every Host B read
phase logged `threads 32/32` with a queue high-water up to 97.

**Host A's cold small-file row measures the ARC, not the device.** 16,384 files in 86 ms is 5 µs
each. Host B takes 216 ms, 13 µs each, and responds to I/O concurrency, which a cache-resident
phase would not.

**Host C's margins are the largest recorded here and measure a Windows property** rather than an
engine one; see below.

## Windows: handle mode and the single-file ceiling

A Windows file handle opened without `FILE_FLAG_OVERLAPPED` is a synchronous file object, and the
I/O manager serializes every operation issued on it. This API is built on the opposite: one shared
handle carrying concurrent operations at disjoint offsets, which costs nothing on Linux because
`pread` on a shared descriptor takes no such lock.

Measured outside the engine, 64 KiB cached reads on one 256 MiB file:

| Threads | 1 | 8 | 32 | 64 |
| --- | --- | --- | --- | --- |
| Shared synchronous handle | 53,550 | 45,032 | 44,712 | 44,961 |
| One synchronous handle per thread | 52,058 | 110,529 | 87,814 | 84,202 |
| Shared `FILE_FLAG_OVERLAPPED` handle | — | — | 86,772 | 82,630 |
| Reopened per operation | — | — | 18,784 | 19,256 |

Concurrency on a shared synchronous handle costs 16% against a single thread. An overlapped handle
reaches what separate handles reach, placing the lock on the file object rather than the file.

A second ceiling sits behind it — per-thread handles to one file plateau, per-thread *files* scale:

| Threads | 1 | 2 | 4 | 8 | 16 | 32 | 64 |
| --- | --- | --- | --- | --- | --- | --- | --- |
| One file, one handle per thread | 58,099 | 82,521 | 110,646 | 108,032 | 90,963 | 90,978 | 89,651 |
| One file per thread | 54,902 | 102,391 | 188,344 | 265,972 | 374,882 | 435,208 | 477,357 |

Windows is not slow at file I/O in general — 477k ops/s and 29 GiB/s across distinct files. It is
slow at concurrent I/O against a single file, which is the shape of the read phases. It is not the
shape of the storage layer's reads, which hash-distribute over 256 pack-file groups whose files
roll at 3 GiB. macOS has no equivalent ceiling; see below.

**The data path therefore owns its `OVERLAPPED`.** `std`'s `seek_read`/`seek_write` cannot be used
on an overlapped handle: they pass no event and report `ERROR_IO_PENDING` as an ordinary error,
after which the caller frees a buffer the kernel is still writing into. `src/overlapped.rs` issues
`ReadFile`/`WriteFile` with its own `OVERLAPPED` and a per-thread event, and waits out a pending
operation before returning. That wait is taken on every operation; see *Windows: the completion-port
backend*. Handle mode is worth 2.36× and 2.10× on the two warm read phases against an otherwise
identical build, measured under *Experiments*, so Host C's margins against `blocking` are the handle
mode rather than the dispatch architecture. The baseline remains the right comparison, because
`lore-storage` does what `blocking` does today.

**An open of a cached file costs about 29 µs on Host C**, whether of the same file or of 4096
distinct ones. Per-open cost is not what sinks `tokiofs` there: 29 µs against 17,945 ops/s at
128-way concurrency leaves two orders of magnitude unaccounted for. Concurrent opens of the *same*
file contending is the term that fits — reopening one file per operation measured 18,784 ops/s at
32 threads against 35,139 sequentially, within 5% of what `tokiofs` reports for the phase whose
every operation is exactly that.

## Windows: the completion-port backend

This section is one sitting on Host C's machine, 2026-08-02. Its absolutes are not comparable with
the rest of this file: the benchmark directory is on the 4 TB NTFS volume inside the scanning
exclusion rather than the 8 TB SATA SSD the 2026-08-01 rows used, and `blocking` measures 148,339 on
the 64 KiB read phase here against 28,308 there. Read this section's ratios rather than its ops/s,
and do not combine its figures with Host C's.

Protocol as elsewhere: `warm-suite 6`, one process per engine, engine order alternated, a discarded
warm-up round first, pool cap 32, `blocking` pool 128.

### Deferred completion

The `iocp` counters report 0% inline on every round: all 7.86 million reads returned
`ERROR_IO_PENDING` and completed through the port. The behaviour is not a property of the port. A
probe outside the engine, issuing the same `ReadFile` against an overlapped handle with a per-thread
event and no completion port — `overlapped.rs` reduced to a counter — reports the same:

| Reads | Threads | ops/s | inline | pending |
| --- | --- | --- | --- | --- |
| 64 KiB | 1 | 43,252 | 0 | 100% |
| 64 KiB | 32 | 84,591 | 0 | 100% |
| 4 KiB | 128 | 130,019 | 0 | 100% |

A buffered read on an overlapped handle therefore defers on this platform in every case measured,
warm and single-threaded included. Each backend has one consequence. On `psync` the
`GetOverlappedResult` wait runs on every operation and parks a pool thread for its duration, so the
pool cap bounds how many reads are in flight. On `iocp` every completion arrives through the port
and crosses a thread. Pool occupancy during the read phases:

| Engine | Read-phase pool occupancy |
| --- | --- |
| `loreio` (psync) | `threads 32/32 peak 32`, queue peak 39–49 at 64 KiB and 97–99 at 4 KiB |
| `iocp` | `threads 1/32 peak 1`, queue peak 1 — the pool serves the open only |

### Warm suite

| Workload | `blocking` | `tokiofs` | `loreio` | `iocp` | iocp / loreio |
| --- | --- | --- | --- | --- | --- |
| `read-64KiB-warm-c64` | 148,339 | 48,316 | 233,156 | **411,460** | 1.76× |
| `read-4KiB-warm-c128` | 153,140 | 49,766 | 217,134 | **1,042,119** | 4.80× |
| `write-256KiB-seq-c32+sync` | **3,137** | 2,277 | 2,668 | 3,000 | 1.12× |
| `small-files-4KiB-wr+rd-c64` | 9,253 | 9,233 | 9,610 | **9,994** | 1.04× |
| `commit-read-mixed-scattered` | 184,886 | 60,034 | **208,700** | 149,472 | **0.72×** |
| `sync-write-mixed-scattered` | 55,933 | 8,194 | **62,170** | 58,126 | 0.93× |

Round-to-round ranges, which determine whether a difference is readable:

| Workload | `blocking` | `tokiofs` | `loreio` | `iocp` |
| --- | --- | --- | --- | --- |
| `read-64KiB-warm-c64` | 143k–153k | 47.0k–50.4k | 218k–237k | 395k–423k |
| `read-4KiB-warm-c128` | 150k–160k | 47.5k–52.2k | 207k–223k | 1021k–1062k |
| `write-256KiB-seq-c32+sync` | 2772–3423 | 1918–2379 | 2506–2826 | 2847–3118 |
| `small-files-4KiB-wr+rd-c64` | 7764–9338 | 6667–9425 | 9102–10251 | 9272–10242 |
| `commit-read-mixed-scattered` | 169k–204k | 55.0k–61.9k | 203k–215k | 139k–158k |
| `sync-write-mixed-scattered` | 52.8k–61.2k | 7185–8268 | 61.8k–64.3k | 54.8k–62.7k |

Position control is 0.94×–1.08× throughout except `blocking`'s write row at 0.87×, the SLC artefact
described under *Experiments*.

**The two synthetic read phases carry the thread ceiling, and they are where the difference is.**
1.76× and 4.80×, with ranges that do not overlap, against a `psync` saturated at 32 threads with a
queue up to 99 deep. Writes and whole-file work are within a few percent, which is the expected
null: the write phase is device-bound, and the small-file phase runs whole-file composites that do
not reach either backend's data path.

**`commit-read-mixed-scattered` measures 0.72×, and it is the repository-shaped phase.** Its range,
139k–158k against 203k–215k, does not overlap. The direction matches the ring's result on Linux:
the synthetic phases favour a completion backend and the repository-shaped ones do not.

### Sensitivity to driver-task count

The two synthetic read phases are driven by `LORE_BENCH_DRIVERS` tasks; the repository-shaped phases
use one per core, 64 on this host. Sweeping the driver count separates dispatch cost from fan-out
cost. Single runs, so the shape carries rather than the individual figures:

| Driver tasks | 1 | 16 | 64 |
| --- | --- | --- | --- |
| `psync`, 64 KiB | 139,418 | 251,500 | 235,242 |
| `psync`, 4 KiB | 157,040 | 223,699 | 236,465 |
| `iocp`, 64 KiB | 34,649 | **394,961** | 257,884 |
| `iocp`, 4 KiB | 76,284 | **1,039,862** | 773,792 |

`psync` is flat from 16 tasks on, which serves as the control: its parallelism comes from the pool
rather than from the caller, so the number of submitting tasks has little effect. `iocp` peaks at 16
and loses approximately a third by 64, and one task per core is what the commit-shaped phase runs.
At one driver task it measures 4× below `psync`, which is the same property observed from the other
end: this backend's concurrency is the caller's task count, because nothing else holds an operation
in flight.

The direction matches the ring's result that spreading submission over many tasks reduces its
throughput. It is not a property of the dispatch path: at 16 tasks the same code measures 1.6×
`psync` on the same phase shape. Reducing the sensitivity is outstanding work.

### Reaper count

Adding reaper threads to carry the completions reduces throughput. `read` mode, one run per cell:

| Reapers | 1 | 2 | 4 | 8 | 16 |
| --- | --- | --- | --- | --- | --- |
| 16 drivers, 64 KiB | **421,663** | 384,491 | 246,400 | 244,404 | — |
| 16 drivers, 4 KiB | **1,029,659** | 973,404 | 272,045 | 248,401 | — |
| 64 drivers, 64 KiB | **309,383** | 278,753 | 205,357 | 188,760 | 194,922 |
| 64 drivers, 4 KiB | 784,453 | **899,041** | 267,440 | 219,692 | 227,592 |

Two reapers measure inside the noise of one; four cost 42% and 74%. The mechanism is the batched
drain: `GetQueuedCompletionStatusEx` takes up to 64 packets per syscall, and threads sharing a port
divide the arriving packets between them, so each wakes for a partial batch and the syscall count
rises with the thread count while the total work does not. One reaper is therefore both the fastest
and the cheapest configuration, and the default does not scale with the machine.

### Cold suite

`cold-suite 8` with the data-set rotation, 2026-08-02, on the machine's 8 TB SATA volume: the other
volume does not honour the eviction, which `check-eviction` reports. This one does, 74,970 ops/s
cached against 7,810 after the purge, and 7,810 × 64 KiB is 488 MiB/s, the drive's sequential rate.
It sits outside the scanning exclusion where the warm figures above are inside it, so the two tables
are not comparable to each other; every engine pays the same terms within each.

One limitation belongs with these figures: the plain-threads baseline is bounded on Windows by the
platform's behaviour for concurrent access to a single file, so it is not the device ceiling there
and an engine may exceed it without reading cache.

| Workload | `blocking` | `tokiofs` | `loreio` | `iocp` | iocp / loreio |
| --- | --- | --- | --- | --- | --- |
| `read-64KiB-cold-c64` | 2,626 | 4,619 | 7,787 | 7,790 | 1.00× |
| `read-4KiB-cold-rec-c128` | 4,824 | 9,620 | 18,265 | **41,453** | **2.27×** |
| `small-files-4KiB-rd-cold-c64` | 44,886 | 45,489 | 45,644 | 46,610 | 1.02× |

Each engine over what plain threads get from the same data sets, with the rotation cancelling which
copy it read:

| Workload | `blocking` | `tokiofs` | `loreio` | `iocp` | spread across copies |
| --- | --- | --- | --- | --- | --- |
| `read-64KiB-cold-c64` | 0.36 | 0.63 | 1.07 | 1.07 | 1.37× |
| `read-4KiB-cold-rec-c128` | 0.09 | 0.17 | 0.33 | 0.74 | 1.09× |
| `small-files-4KiB-rd-cold-c64` | 1.06 | 1.07 | 1.07 | 1.10 | 1.02× |

**The 64 KiB phase is the device and nothing else.** Both backends reach 488 MiB/s, this drive's
sequential rate, and their medians differ by 3 ops/s. That is the expected result and a check on the
warm figures: when a read waits on a SATA device, what dispatched it stops mattering. `blocking`
reaches 0.36 of the same ceiling and `tokiofs` 0.63, so the phase does discriminate — it is the two
`lore-io` backends that have stopped being the bottleneck.

**The 4 KiB phase is not bandwidth-bound, and `iocp` takes it by 2.27×**, 40,569–41,996 against
18,130–18,391 over eight rounds with no overlap. 140 MiB moved in 0.87 s is 161 MiB/s, a third of
what the drive gives sequentially, so the phase is bound by requests in flight rather than by bytes.
That is the bound `psync` meets when every operation parks a pool thread and the one a completion
port removes, and it is the same property the warm 4 KiB phase reports at 4.80× — at a third the
margin here, because the device now costs something.

**Whole-file reads are a null at 1.02×**, as they are warm: that phase runs whole-file composites,
which both backends forward to the syscall pool.

Round-to-round ranges on the 64 KiB row are wide by construction — 5,714–7,837 for `loreio` and
5,698–7,827 for `iocp` — because the rotation makes each engine read all four copies and the copies
span 1.37×. The median is what carries there, and the baseline-normalized table is the cleaner
reading. The 4 KiB and small-file rows are tight.

## macOS: heterogeneous cores

`available_parallelism()` reports 8 on Host D; `hw.perflevel0.logicalcpu` reports 4 performance
cores and `hw.perflevel1.logicalcpu` 4 efficiency cores. Cached reads there peak at four threads
and lose more than half beyond it. The warm cap sweep, `loreio` only, median of 4 passes, ratios
against 16:

| Workload | 4 | 8 | 16 | 24 | 32 | 48 | 64 |
| --- | --- | --- | --- | --- | --- | --- | --- |
| `read-64KiB-warm-c64` | **1.34×** | 1.10× | 1.00× | 1.00× | 0.91× | 0.89× | 0.99× |
| `read-4KiB-warm-c128` | **2.90×** | 1.81× | 1.00× | 0.91× | 0.86× | 0.88× | 0.91× |
| `write-256KiB-seq-c32+sync` | 1.06× | 1.00× | 1.00× | 0.99× | 0.98× | 0.99× | 0.98× |
| `small-files-4KiB-wr+rd-c64` | 1.02× | 1.04× | 1.00× | 0.98× | 0.96× | 0.92× | 0.90× |

That 2.90× is the host, not the engine. Plain threads, no engine and no async dispatch anywhere,
on cached 4 KiB reads:

| Plain threads, warm | 1 | 2 | 4 | 8 | 16 | 32 |
| --- | --- | --- | --- | --- | --- | --- |
| One file, shared descriptor | 1,624k | 1,952k | **2,746k** | 1,251k | 1,240k | — |
| One file per thread | 1,514k | 2,449k | **4,276k** | 1,675k | 1,657k | — |
| Shared descriptor, work-stealing | — | 1,762k | 2,685k | 1,247k | 1,279k | 1,271k |

The cause is the core split. Four threads confined to efficiency cores by `QOS_CLASS_BACKGROUND`
give 947k ops/s; the same four at `QOS_CLASS_USER_INTERACTIVE` give 2,727k, a 2.9× gap on identical
work. Past four threads the scheduler necessarily uses efficiency cores, and raising QoS does not
recover it — 1,277k against 1,279k at 16 threads. Both engines sit on this curve: `blocking` runs
18 threads and `loreio` 16, both past the peak, so the comparison between them is unaffected.

Two bounds on how far this generalizes. It is a small-read effect — at 64 KiB the same probe is
flat from 4 threads on, 738k shared and 660k–716k per-thread, because the phase is
memory-bandwidth bound at 43–46 GiB/s. And it is not a per-file ceiling: giving every thread its
own file raises the peak from 2,746k to 4,276k but collapses at eight threads just the same, so
nothing is serialized on the file.

**This governs the pools that run compute, not the syscall pool.** Sizing the syscall pool to the
performance-core count was tried and measured: at 8 threads warm reads go to 317k and 808k ops/s,
exactly the 1.10× and 1.81× the sweep predicts, while `loreio` drops to 0.74 of its data set's
plain-threads rate on 64 KiB cold reads against 0.95 at 16 threads. A thread parked in `pread` is
waiting on a device, not occupying a core, so a slow core still carries a request, and what sets
this pool's useful size is how many requests keep the device busy — 16 to 32 here. Only the cached
read is core-bound. The finding belongs to the `ThreadCounts` apportionment in
`lore-base/src/runtime.rs`, whose worker pool is core-bound and does size itself from
`available_parallelism()`.

## Pool size

### Host B, median of 2 passes, ratios against 32

| Workload | 8 | 16 | 24 | 32 | 48 | 64 |
| --- | --- | --- | --- | --- | --- | --- |
| `read-64KiB-warm-c64` | 2.16× | 1.04× | 1.00× | 1.00× | 0.98× | 1.02× |
| `read-4KiB-warm-c128` | 0.98× | 0.99× | 0.97× | 1.00× | 0.99× | 0.99× |
| `small-files-4KiB-wr+rd-c64` | 1.07× | 1.01× | 1.00× | 1.00× | 0.99× | 0.91× |
| `read-64KiB-cold-c64` | 0.91× | 0.97× | 0.98× | 1.00× | 0.99× | 1.00× |
| `read-4KiB-cold-rec-c128` | 1.00× | 1.00× | 1.00× | 1.00× | 1.00× | 1.01× |
| `small-files-4KiB-rd-cold-c64` | 1.00× | 1.00× | 1.00× | 1.00× | 1.21× | 1.34× |

The 2.16× at cap 8 is a property of the host: running the same reduction on the baseline's tokio
blocking pool reproduces it, `blocking` measuring 714k–796k at 8 threads against 331k–337k at 128.
Both engines sit on the same host curve, so a lower cap does not make `lore-io` faster than
`blocking` — it moves both.

### Host C, median of 2 passes warm and 3 cold, ratios against 32

| Workload | 8 | 16 | 24 | 32 | 48 | 64 |
| --- | --- | --- | --- | --- | --- | --- |
| `read-64KiB-warm-c64` | 0.97× | 0.96× | 1.00× | 1.00× | 1.00× | 0.98× |
| `read-4KiB-warm-c128` | 0.90× | 0.94× | 0.99× | 1.00× | 0.96× | 0.94× |
| `write-256KiB-seq-c32+sync` | 0.92× | 0.94× | 0.99× | 1.00× | 0.96× | 0.98× |
| `small-files-4KiB-wr+rd-c64` | 1.01× | 0.99× | 1.02× | 1.00× | 0.98× | 0.96× |

| Workload | 8 | 16 | 32 | 64 |
| --- | --- | --- | --- | --- |
| `read-64KiB-cold-c64` | 0.94× | 0.97× | 1.00× | 1.01× |
| `read-4KiB-cold-rec-c128` | 0.89× | 0.98× | 1.00× | 1.00× |
| `small-files-4KiB-rd-cold-c64` | **0.54×** | **0.92×** | 1.00× | 1.03× |

Raw per-pass cold values, since the ratios below 1.00× are what the floor rests on: 5327–5368,
5499–5609, 5672–5694 and 5701–5727 on the 64 KiB phase; 15.7k–16.4k, 17.5k–17.6k, 17.9k–17.9k and
18.0k–18.1k on the 4 KiB phase; 28.4k–29.3k, 49.2k–51.1k, 53.6k–53.7k and 55.3k–55.6k on the small
files. Monotone in the cap, tight per cap, non-overlapping between adjacent caps on the small-file
row.

### Host D, median of 6 passes, ratios against 16

Only `loreio` runs in a sweep, so every cap reads the same copy and the per-file effect does not
enter.

| Workload | 4 | 8 | 16 | 24 | 32 | 48 | 64 |
| --- | --- | --- | --- | --- | --- | --- | --- |
| `read-64KiB-cold-c64` | 0.66× | 0.88× | 1.00× | 1.02× | 1.05× | 1.07× | 1.06× |
| `read-4KiB-cold-rec-c128` | 0.65× | 0.93× | 1.00× | 0.94× | 1.00× | 1.00× | 1.00× |
| `small-files-4KiB-rd-cold-c64` | 0.76× | 1.03× | 1.00× | 1.14× | 1.11× | 0.94× | 1.08× |

| Workload | 4 | 8 | 16 | 24 | 32 | 48 | 64 |
| --- | --- | --- | --- | --- | --- | --- | --- |
| `read-64KiB-cold-c64` | 24,982 | 33,314 | 37,957 | 38,593 | 40,031 | 40,725 | 40,338 |
| `read-4KiB-cold-rec-c128` | 39,820 | 57,248 | 61,328 | 57,578 | 61,310 | 61,283 | 61,291 |
| `small-files-4KiB-rd-cold-c64` | 39,329 | 53,527 | 51,898 | 59,107 | 57,454 | 48,667 | 55,824 |

### What the sweeps establish

**No single cap wins.** Warm work prefers fewer threads and cold work more, and on Host D the two
pull hardest: cap 4 is 2.90× on warm 4 KiB reads and 0.65× on cold ones, a 4.5× swing on the same
setting. A cap is a position on that curve.

**The ceiling of 16 costs single digits.** Against 32: 4–6% warm and 2–8% cold on Host C, within
±4% either way on Host B, and on Host D 5% on cold 64 KiB reads while gaining 10% and 16% on the
two warm read phases. The threads are the return — this pool shares a process-wide budget with the
core workers, the net runtime and the residual blocking pool. `LORE_IO_POOL_THREADS` recovers the
throughput where it matters. Above 32, the only cold phase on any host that gains is Host B's cold
small files (1.34× at 64).

**A floor is the part these sweeps most support, and is not in the formula today.** Falling well
below the workload's concurrency costs sharply — 0.54× at 8 threads on Host C's cold whole-file
reads, 0.66× and 0.65× at cap 4 on Host D's cold reads — and that is a ratio of pool size to
in-flight requests rather than to core count. Host D is where the formula itself reaches those
sizes: `min(2 × cores, 16)` gives 8 threads on a four-core machine and 4 on a dual-core one. What
argues against acting on the sweeps alone is that both figures are this benchmark's concurrency
rather than a measured client workload's.

**Fewer threads did not measurably reduce memory.** Peak RSS across Host B's sweep, 8 through 64
threads, moved between 18.8 and 26.7 MiB warm and 3.5 to 4.5 MiB cold, with no monotone trend. A
thread reduction should be argued on throughput or scheduler pressure, not footprint.

**What the sweep tables do not support.** Host B's and Host C's are two passes warm and three cold
against this file's standard of six, and report medians without spreads; Host D's are six cold and
four warm. Where spread was checked it sometimes exceeds the effect — Host C's warm `small-files`
row moves 9–13% between passes against cap-to-cap differences of 1–2%, so nothing rests on that
row. The warm read and write rows are tight: cap 16 at 133.1k–134.9k against cap 32 at
142.0k–142.3k on the 4 KiB phase.

## Design decisions

**One process per engine**, because a shared process leaks cache and warm-up state between them.

**Reads allocate their own buffer, inside the I/O job, and return `Bytes`.** The allocation lands
on the thread that receives the kernel's copy, and rpmalloc caches spans per thread, so buffer
reuse is thread-local without the engine managing it. Writes take a caller buffer, because the data
is the caller's.

**Read buffers are allocated uninitialised** and truncated to the byte count before becoming
`Bytes`. Zeroing first writes every byte twice, about a tenth of the benchmark's CPU time.

**Windows handles opened by the engine are overlapped**, and the data path issues its own
`ReadFile`/`WriteFile`. A correctness requirement before a performance one; adds `windows-sys` to
the library's dependencies on that platform only.

**The benchmark links `lore-base` so it allocates the way a Lore process does.** The read-path
design rests on the allocator caching spans per thread, which glibc malloc does not do, so a
benchmark on glibc malloc tests a program whose premise does not hold. Dev-only, so the library's
graph is unchanged. Host A predates this.

**The `blocking` engine is `spawn_blocking` plus a positional syscall**, not `tokio::fs`. It is the
strongest form of the path that exists today, so the comparison is not favourably biased.

**Phase sizes are fixed in the source** rather than tunable, so two people running the suite
compare the same thing.

**Each engine gets its own cold data set**, so no engine's child process can delete or warm what
another needs, and one cache drop serves all of them. The copies are not equivalent, so the
assignment rotates by round.

**Cold phase sizes are derived from a measured device rate, not fixed in the source.** They are the
one exception to the rule above, and the reason is that a cold phase reads each block once, so its
duration is the data set size divided by what the device gives. A fixed operation count fixes the
duration only for one device speed: 4096 permuted 64 KiB reads are about a second on a SATA SSD and
under 20 ms on a fast NVMe volume, far below the floor at which a phase measures anything.
`prepare-cold` measures the rate, sizes the phases for about a second, and records the result in a
manifest beside the data. Every engine in the suite then reads the sizes the manifest names, so a
suite is internally comparable even though two machines' suites are not — which was already true of
cold absolutes.

**The cold baseline opens a handle per thread.** A shared handle is a serialization point on
Windows, where the I/O manager runs one operation at a time on a synchronous file object, so
sharing one measured 3,174 ops/s against 53,428 for a handle each and made every ratio taken
against it meaningless.

## Experiments

### Verifying cache eviction

macOS has no `posix_fadvise` and `purge` refuses without root. `msync(MS_INVALIDATE)` over a
mapping of the file evicts, and needs no privilege: a 256 MiB file re-read at 17–18 GiB/s before
the call and 3.3 GiB/s after it across three rounds, which is the SSD's sequential rate, and 4 KiB
whole files at 16 µs before and 43 µs after. `F_NOCACHE` and `F_GLOBAL_NOCACHE` measured no change
at all — they change how a descriptor reads, not what the cache holds. `mincore` confirms 0.0% of
a file resident after eviction.

Windows: a 1 GiB file re-read at 4.6 GiB/s before `FILE_FLAG_NO_BUFFERING` and 469 MiB/s after,
which is the drive's sequential rate. ext4: a cold run with no `drop_caches` reproduced a
cache-dropped run within 1–2% on eight of nine cells.

### The cold data files are not interchangeable

Each engine reads its own copy of the cold data set. On APFS the copies do not read back at the
same rate:

| Plain threads, permuted reads | `-blocking` | `-tokiofs` | `-loreio` |
| --- | --- | --- | --- |
| 64 KiB, 32 threads, generation 1 | 47,522 | 50,502 | 39,889 |
| 64 KiB, 32 threads, generation 2 | 36,516 | 42,869 | 39,277 |
| 4 KiB strided, 128 threads, generation 1 | 37,749 | 37,503 | 60,809 |
| 4 KiB strided, 128 threads, generation 2 | 38,600 | 58,972 | 37,879 |

Two cold suites run before the rotation existed, each engine on a fixed copy, show what that does:

| Workload | `blocking` | `tokiofs` | `loreio` | vs `blocking` |
| --- | --- | --- | --- | --- |
| `read-64KiB-cold-c64`, generation 1 | 47,280 | 17,368 | 37,898 | **0.80×** |
| `read-64KiB-cold-c64`, generation 2 | 37,057 | 16,321 | 37,140 | **1.00×** |
| `read-4KiB-cold-rec-c128`, generation 1 | 37,956 | 22,254 | 61,348 | **1.62×** |
| `read-4KiB-cold-rec-c128`, generation 2 | 38,781 | 24,946 | 38,369 | **0.99×** |

The generation-1 figures pass every check the suite applies: non-overlapping ranges over six
rounds — 47.0k–47.6k against 37.7k–38.0k, and 37.9k–38.0k against 61.3k–61.4k — and a position
control of 1.00×. Both are artefacts of which copy each engine held. Dividing each engine by its
own copy's rate collapses them: `blocking` 0.99, 1.01, 1.01, 1.00 and `loreio` 0.95, 0.95, 1.01,
1.01 across the four cells, with `tokiofs` at 0.34–0.59.

Five checks locate the mechanism below the filesystem. Not content: the copies are byte-identical,
seeded from the file size, and hash the same. Not cache: `mincore` reports 0.0% resident after
eviction. Not bandwidth: read sequentially all six files return 3316–3346 MiB/s, inside 1%. Not
fragmentation: twelve files written identically on this volume came back with the same extent map
to two decimals, 64 extents of 4.00 MiB, and spanned 1.78× on the permuted read, 29,044 to 51,652
ops/s. Not run-to-run noise: one file re-measured three times gives 35,532, 34,750 and 35,064.

What remains is a concurrency ceiling:

| Threads | 1 | 4 | 8 | 32 |
| --- | --- | --- | --- | --- |
| Fast copy | 10,433 | 25,805 | 38,279 | 52,060 |
| Slow copy | 9,979 | 24,064 | 33,847 | 35,206 |
| Ratio | 1.05× | 1.07× | 1.13× | **1.48×** |

Per-request latency is the same and the gap opens only with requests in flight, the slow copy
flattening near 35k. That is a file whose blocks landed on fewer independently addressable units of
the drive: same service time each, fewer served at once. The mapping is fixed when the blocks are
written and no filesystem-level property exposes it. Sequential reads do not show it because
readahead turns them into few large requests.

**The fix is rotation.** `cold-suite` offsets which copy each engine reads by the round number, so
over a round count divisible by three each engine reads each copy equally often. It rotates by the
engine's own index rather than by position in the round's order, keeping the assignment a
bijection: the three engines read three different copies within any round, so no engine warms the
file the next reads and one cache drop still serves all three. The data set appears on each child's
header line.

After rotation the two ways of measuring agree — `loreio` reads 0.99× of `blocking` directly and
0.95 ÷ 0.96 = 0.99 through the baseline. On generation 1 they disagreed, 0.80× against 0.96, and
the gap between the two answers was the bias.

This is very likely what Host C's withdrawn result was made of: the same probe measured 31k before
`prepare-cold` rewrote the file and 17.5k after, a 1.77× shift with no code change.

### The ordering trap

The warm suite used to write one data file up front and let both engines read it in one process.
The write and small-file phases move gigabytes through the page cache, so the second engine read a
file the first had evicted — a cold read scored against a warm one. Worth about 3×, and it followed
position, not engine: reversing the order moved the deficit wholesale. Every "lore-io is 2× slower
on warm reads" result this file used to carry was this. It survived a long investigation because it
is stable across runs, reproduces on demand, is unaffected by anything in the engine, and corrupts
the within-run ratio. It was caught by comparing a whole-suite result against a run of the read
phase alone.

Six candidate causes were tested against that phantom gap and eliminated, each with an interleaved,
order-alternated A/B using the identical engine in both builds as a noise-floor control:

- **Buffer size classes**, 4 KiB to 1 MiB against 64 KiB upward. Throughput-neutral.
- **The buffer pool's lock**, a `Mutex` per class replaced with a lock-free `ArrayQueue`.
  Throughput-neutral.
- **Allocation count**, two heap allocations per operation collapsed into one. Throughput-neutral,
  which also says per-operation allocation is not what limits this path.
- **Pooled versus fresh buffers.** Fresh per read was *slower*.
- **Thread count**, cap 32 raised to the blocking pool's 42. No improvement, more variance.
- **Forced `mmap` allocation** (`MALLOC_MMAP_THRESHOLD_`). Made every engine several times slower
  and swamped the variable it was meant to isolate.

### Handle mode on Windows

An interleaved A/B of two builds of the same engine, one opening synchronous handles and one
opening overlapped handles, with `blocking` as the noise floor:

| Workload | Synchronous handle | Overlapped | Ratio | `blocking` control |
| --- | --- | --- | --- | --- |
| `read-64KiB-warm-c64` | 33,987 | 80,332 | **2.36×** | 0.99× |
| `read-4KiB-warm-c128` | 66,726 | 139,832 | **2.10×** | 1.01× |
| `write-256KiB-seq-c32+sync` | 1,259 | 1,476 | 1.17× | 1.00× |
| `small-files-4KiB-wr+rd-c64` | 9,636 | 9,560 | 0.99× | 1.00× |

Small files at 0.99× is the expected null: that phase runs whole-file composites, which open their
own handle per file and never share one. The effect is largest cold, because a serialized handle
cannot keep requests in flight.

### Withdrawn results

**A `perf` profile.** One pair of `perf stat` runs showed `lore-io` at half the IPC, double the
cache misses and 300× fewer page faults, supporting a detailed story about cache-line ownership on
ZFS's `copyout`. A second pair inverted the CPU signature. A single unreplicated profile is worth
no more than a single unreplicated benchmark run.

**A cap step on Windows.** An earlier pass reported the cold 4 KiB phase jumping 1.62× between cap
48 and cap 64 across three passes with non-overlapping ranges, with a plain-threads control at a
flat ~31k ops/s from 16 to 128 threads read as the device's ceiling. Re-running both on a rewritten
cold data set, interleaved, in one sitting:

| Configuration | ops/s |
| --- | --- |
| Plain threads, own synchronous handle, 16 / 128 threads | 17,649 / 17,354 |
| Plain threads, own overlapped handle, 16 / 128 threads | 17,584 / 17,421 |
| Engine, one shared handle, cap 32 | 17,575 / 17,718 |
| Engine, one shared handle, cap 64 | 17,763 / 17,715 |

Everything within 2%: no cap effect, no handle-mode effect, no gap between the engine and plain
threads. The 1.62× step, the 31k ceiling and the latency gap inferred from them are withdrawn. What
survives is narrower still than it was written: handle mode costs nothing cold. The other half of
that sentence — that cold buffered reads complete inline on an overlapped handle — is itself now
withdrawn, by a counter rather than an inference; see *Windows: the completion-port backend*.

**A cap sweep that measured a bug.** Host C's warm sweep was run before and after the overlapped
fix. Before it, submissions piled onto a serializing handle only add queueing, so the sweep
reported 8 and 16 threads beating 32 — read as an argument for a smaller cap; it was a symptom.

| Workload | 8 | 16 | 24 | 32 | 48 | 64 |
| --- | --- | --- | --- | --- | --- | --- |
| `read-64KiB-warm-c64`, synchronous | 1.08× | 1.07× | 1.04× | 1.00× | 0.99× | 0.76× |
| `read-64KiB-warm-c64`, overlapped | 0.97× | 0.96× | 1.00× | 1.00× | 1.00× | 0.98× |
| `read-4KiB-warm-c128`, synchronous | 1.09× | 1.11× | 1.09× | 1.00× | 0.96× | 0.97× |
| `read-4KiB-warm-c128`, overlapped | 0.90× | 0.94× | 0.99× | 1.00× | 0.96× | 0.94× |

A cap sweep only measures the cap when nothing upstream of it is the bottleneck.

### Discarded designs

A size-classed buffer pool with per-class retention derived from a memory budget, and lock-free
per-class queues: removed in favour of allocating in the I/O thread, since the allocator already
caches per thread. Handing the driver a caller-allocated buffer for reads: the owned-buffer
contract only requires that whoever owns the buffer holds it until completion, which allocating
inside the job satisfies more directly. The one capability lost is reading into memory the caller
already owns; a chunker port that wants to fill a window in place would need it back.

### Protocol notes

**The noise floor.** Absolute ops/s across batches means nothing: the same binary has moved 38%
between batches an hour apart, and the same workload has measured 0.43× and 0.79× in different
sessions. Take at least six runs and use the median. For an A/B of two builds, build both, keep
both binaries, interleave them in one sitting, alternate which runs first, and put `sync; sleep 3`
between runs — then compare the *unchanged* engine's medians between the two groups. If that
difference exceeds the effect, the experiment did not answer the question.

**Position controls do not catch a per-engine input.** Rounds, alternation and the position control
all vary *when* an engine runs. A fixed assignment of engine to data file is identical in every
round and every order, so those controls hold it still rather than exposing it. Anything the
harness gives one engine and not another — a file, a directory, a handle — is that kind of bias
until something measures it.

**Cold absolutes belong to a data-set generation.** A cold sweep must run against one generation,
with its control in the sweep's own sitting. A within-sitting alternation control does not license
cross-sitting comparison.

**Know what the device does under sustained writes.** Host B's write phase measures a QLC SSD's
sliding SLC cache: 332 MiB/s for the first writer and about 137 MiB/s after. The tell is a first
round several times faster than every round after it, decaying by position rather than by engine.
Recovery during idle means the blip can reappear mid-suite.

**Environment.** Pin the CPU governor with `sudo cpupower frequency-set -g performance`; under
`intel_pstate` the `powersave` governor still ramps under load, but idle clocks of 400 MHz cost the
start of every phase. The same applies to `schedutil` on `acpi-cpufreq`, which sat Host B's cores
at 2200 MHz — pinning was worth 14% on `loreio`'s 64 KiB warm reads. On ZFS the cold suite needs
root, small files may still not go cold — 16,384 files in 54 ms is cache — and transaction groups
couple consecutive runs. On macOS check AC power and Low Power Mode, run under `caffeinate -i`, and
confirm with `pmset -g therm` afterwards. Check what else inspects file operations: Host D's
Endpoint Security extension puts 12.8–15.5 µs into every `open`.

**Build and run in one `&&` chain.** A probe rebuilt with `rustc ... | grep error; ./probe` fell
through a compile error to the previous build and produced plausible numbers.

### Harness defects

**The cold phases spread their reads, which made them warm.** `read_phase` is shared by the warm
and cold phases and read `LORE_BENCH_READ_SPREAD` itself, so a cold phase evicted `data64k-<tag>`
and then read 64 `.spread64-N` copies of it — files `spread_files` creates by copying the data file,
and therefore in cache the moment they exist. The offset remapping that accompanies a spread is the
second half of it: offsets are folded into one part's span, so a phase sized to read each block once
instead read a fraction of the data set several times over, which is a cache hit whatever the
eviction did.

Every cold figure this file carried before 2026-08-02 was produced that way and none of them is a
cold measurement. The phase measured 124,000 ops/s where the same phase measures 7,800 after the
fix, on a device whose ceiling is 488 MiB/s. `read_phase` now takes the spread as an argument and
the cold callers pass one; a phase that reads faster than the probe rate that sized it now says so
on its own line, which is what caught this.

Two smaller ones the same change surfaced. `write_random_file` wrote whole 1 MiB chunks and dropped
the remainder, which no fixed 256 MiB data set ever exposed and every derived size does — a short
file ends a phase in `UnexpectedEof`. And it filled every byte from the generator, which costs more
than the write once a data set is tens of gigabytes.

**The pending-completion path was reported as untested, and the report was wrong.** This entry
used to read that every Windows measurement recorded zero `ERROR_IO_PENDING`, so `overlapped.rs`'s
`GetOverlappedResult` wait had never executed. Counting instead of inferring says the opposite:
100% of buffered reads on an overlapped handle defer, so the wait runs on every operation and
always has. The cancel-and-wait-again recovery inside it is still untested, since that needs the
wait itself to fail.

Worth keeping as a method note rather than deleting. The claim was plausible, was repeated in three
places, and was never measured — it came from reading throughput figures that a fully-inline path
would also have produced. `lore-io/src/iocp.rs` grew a counter for its own reasons and contradicted
it immediately.

**Two that only Windows could surface.** The `tokiofs` write phase opened the file read-only to
`sync_data` it, which `fdatasync` accepts and `FlushFileBuffers` refuses, taking the engine child
down mid-suite. And cache eviction was a no-op on every non-Unix platform, so a cold suite there
measured cache at full speed and reported it as cold without failing.

**One that only macOS could surface.** `evict_from_page_cache` called `posix_fadvise`, which does
not exist on Darwin, so the example failed to compile on the platform where `psync` is the
permanent backend. `examples/pool-sweep.sh` had the mirror image, defaulting `BENCH_BIN` to
`bench.exe`. Both come from writing a `#[cfg(unix)]` arm against Linux.

### The io_uring investigation

Preliminary: single runs of the `read` mode rather than medians of the full suite, so the direction
carries and the magnitude does not.

| Workload | Filesystem | `loreio` (psync) | `uring` |
| --- | --- | --- | --- |
| `read-64KiB-warm-c64` | ZFS | 645,569 | 197,668 |
| `read-4KiB-warm-c128` | ZFS | 697,681 | 247,091 |
| `read-64KiB-warm-c64` | tmpfs | 630,420 | 225,679 |
| `read-4KiB-warm-c128` | tmpfs | 859,352 | 831,505 |

**On ZFS the kernel cannot complete a ring read inline, so it punts every one to an `io-wq`
worker.** Sampling the process's threads during a run shows 55 `iou-wrk-*` kernel workers on ZFS
against 2 on tmpfs. That is the extra thread hop `psync` does not pay: its pool thread performs
the ARC copy itself and returns. The 4 KiB row is the clean demonstration — 0.35× on ZFS, parity
on tmpfs, same code.

**The 64 KiB deficit has a different cause, and it is the important one: the ring serializes
concurrent reads of a single file.** `io_uring` hashes buffered work on a regular file by inode,
and io-wq runs only one worker per hash at a time, so 64 concurrent reads of one file execute one
after another on one kernel thread. Three independent signals agree:

- Thread sampling during the 64 KiB phase shows a single `iou-wrk` worker at 101% CPU, where
  `psync` has 16 pool threads at ~108% each.
- `perf stat` over the read phases: 1.94 CPUs utilized for `uring` against 7.8 for `psync`.
- The throughput is single-core `memcpy` bandwidth. 7.7 GB/s is what one core copies; `psync`'s
  48–54 GB/s is seven or eight cores of it.

Spreading the same workload over more files confirms it directly — only the inode count changes:

| Files the reads are spread over | `uring` ops/s |
| --- | --- |
| 1 | 107,534 |
| 4 | 547,428 |
| 8 | 419,534 |

**One file is a worst case, and not the one the storage layer presents.** Addresses are
hash-distributed over 256 pack-file groups — `group_index` is the first byte of the content hash —
and each group's pack files roll at 3 GiB, so a server holds thousands of them and concurrent
reads scatter across inodes. `LORE_BENCH_READ_SPREAD` measures that shape. Holding the working set
constant at 256 MiB so that a wider spread does not also mean a colder one:

| Reads spread over | `uring` ops/s | `psync` ops/s | ratio |
| --- | --- | --- | --- |
| 1 file | 181,810 | 915,773 | 0.20× |
| 4 files | 511,946 | 796,551 | 0.64× |
| 16 files | 554,136 | 785,898 | 0.71× |
| 64 files | 611,581 | 803,281 | 0.76× |

`psync` is flat across the sweep, which is the control: it has no inode sensitivity, so the
movement is entirely the ring's. The serialization penalty is severe at one inode and mostly gone
by four. So the 5× figure is an artefact of a benchmark that reads one file, while a real deficit
survives at realistic spread.

That deficit does not close with more files — it turns over. Two passes with the engine order
reversed between them, so the shape is not position bias:

| Spread | `uring` / `psync` pass 1 | pass 2 |
| --- | --- | --- |
| 64 | 0.77× | 0.81× |
| 128 | 0.64× | 0.56× |
| 256 | 0.65× | 0.60× |

The peak sits at a spread equal to the workload's concurrency, which is where every in-flight read
has an inode to itself; past that, extra files buy nothing and something starts costing. One
confound to name: holding the working set constant means spread 128 is 2 MiB per file and spread
256 is 1 MiB, so "more files" and "smaller files" are entangled at the top end and this experiment
cannot separate them.

Holding the working set constant matters more than it sounds. An earlier version of this sweep
grew it with the file count, and both engines converged on ~285,000 ops/s at spread 32 — which
looks like parity and is really both of them becoming device-bound.

`O_DIRECT` is the kernel escape from the hash, since work on an `O_DIRECT` file is not hashed, but
it bypasses the cache and so trades this for a worse problem on warm reads. Untested here.

**Registered files do not explain the residual.** `IORING_REGISTER_FILES` with `IOSQE_FIXED_FILE`
removes the per-operation `fget`/`fput` on the descriptor, which is the one per-op kernel cost
`psync` does not pay — it holds an `Arc<File>`. Three rounds at spread 64 with the order
alternated: plain 610,278 / 496,408 / 529,469, fixed 557,566 / 523,562 / 591,972. Medians 529,469
against 557,566 with the ranges overlapping across most of their width, so there is no effect to
report. The descriptor path is not where the deficit lives.

**On ext4 the ring wins, and the deficit above is a ZFS story.** ext4 implements `IOCB_NOWAIT`, so
a cached read completes during the submit syscall: zero `iou-wrk` workers appear and the ring
reports 100% of completions drained by submitting threads. That changes which resource is scarce.
The copy now runs on the submitting thread, so throughput is bounded by how many threads can be
inside a ring — the driver count and the shard count — rather than by kernel workers.

Both bounds had to be lifted, and each looks like a dead end until the other moves:

| ext4, 64 KiB, spread 64 | `uring` | `psync` |
| --- | --- | --- |
| 1 driver, 5 shards | 200,753 | 980,121 |
| 16 drivers, 5 shards | 538,988 | 816,894 |
| 16 drivers, 16 shards | 1,017,672 | — |
| 16 drivers, 32 shards | 1,029,863 | — |

Interleaved with the order alternated, each engine at its best configuration: `uring` 1,006,508 /
1,035,959 / 1,054,981 against `psync` 935,842 / 911,086 / 954,033 — medians 1,035,959 and 935,842,
**1.11×**, with the ranges not overlapping.

The shard count was the larger lever and it was mis-tuned, because it was tuned here. On ZFS the
submitting thread copies nothing — a kernel worker does — so shards barely register, and
`clamp(cores / 4, 2, 8)` looked fine. On ext4 it costs half the throughput. The default is now one
shard per core, and on ZFS the wider count measures slightly better rather than worse, so nothing
trades against it.

These are the measurements that drove the shard count to one per core; the current figures for
both filesystems are in *Results* above, and are lower than the 1.11× here because the suite now
includes the repository-shaped phases. What remains true of ZFS is that with the punt in place no
configuration moves the ring, because the ceiling is kernel-side. Per-thread sampling of that
case:


during a spread-64 run accounts for it:

| Thread | CPU% |
| --- | --- |
| the thread driving the reads | 86.0 |
| 5 × `iou-wrk` | ~78 each |
| `lore-io-uring` (reaper) | 46.5 |
| 20 × `tokio-rt-worker` | 0.0 |

Five io-wq workers do all the copying — about four cores against the pool's sixteen threads — and
that is the ceiling. The default bounded worker cap is roughly the core count, so at five the cap
is not what binds: io-wq grows its pool when a worker *blocks*, and a cache hit never blocks, so
for warm reads it stays at a handful however deep the queue gets. `psync` has sixteen threads
standing by because this crate sized them.

Two things follow that are easy to get backwards. The ring is not inefficient — the same accounting
gives it roughly 9 µcore-seconds per operation against 19 for `psync`, so it does about half the
work per read and loses only because it will not use the machine. And the reaper is not the
problem either, at 46.5% of one core.

Neither is the submission side, though the sample above makes it look that way. The reads are
driven by `buffer_unordered` on the `block_on` thread, so every `io_uring_enter` is issued by that
one thread at 86% while the tokio workers sit idle. Spreading the drive over long-lived tasks so
submission lands on many worker threads makes `uring` *worse* — 594,334 ops/s at one driver,
423,467 at four, 363,401 at sixteen — while `psync` stays flat at 759,698 / 701,247 / 706,897.
More submitters contend on the per-shard submission and completion locks without adding copy
workers, which are what is scarce.

**Removing the ring's per-operation payload allocation changed nothing measurable**, which is
what the io-wq ceiling predicts and is recorded so the result is not rediscovered. Each operation
used to erase its payload behind `Box<dyn Any>` — a second heap allocation and a downcast on top of
the entry's own `Arc` — where the syscall pool had already been cut to one allocation and no type
erasure. Naming the completion function in a `repr(C)` header instead removes both. An interleaved
A/B of the two builds, four rounds with the order alternated: boxed 561,540 / 657,244 / 526,731 /
745,120, thin 534,328 / 579,761 / 676,373 / 629,050 — medians 609,392 and 604,406, ranges
overlapping across nearly their whole width. Kept because it is less work and less code per
operation, not because it is faster.

A cross-batch look at the same change said 656,708 against 529,469 and looked like a 24% win. It
was drift between sittings. This is the third time in this file that a plausible number came from
comparing runs that were not interleaved.

**Three further hypotheses that were tested and refuted**, kept because re-running them is not
free. The per-ring submit lock, *on ZFS*: raising the shard count from 5 to 20 and lowering it to 2
both measured worse than 5 — a reading that later failed to reproduce and that the ext4 result
contradicts outright. Those runs came from different batches, which is the one mistake this file
keeps recording; the shard count was in fact the single largest lever available, and dismissing it
here cost several rounds of looking elsewhere. Submission batching: an elimination protocol that lets one thread
enter on behalf of everything queued never fired, because on this workload submissions do not
overlap — each task awaits its own read, so the submit path is rarely occupied by two threads at
once, and the measured rate stayed at 1.0 operations per `io_uring_enter` with throughput
unchanged. The io-wq worker cap: `IORING_REGISTER_IOWQ_MAX_WORKERS` raised to 16 and 64 changed
nothing, because the limit that binds is the per-inode hash, not the worker ceiling.

Note that one syscall per operation is not itself the problem: `psync` also costs one syscall per
operation and is faster, so syscall count cannot be what separates them.

## See also

- `docs/developing/internals/file-io-engine.md` — the engine's architecture and the plan for
  replacing `std::fs` and `tokio::fs`.
- `docs/proposals/2026-07-24-tokio-runtime-split-and-async-io.md` — the enhancement proposal this
  crate implements, including the thread-budget arithmetic these benchmarks validate.
- `lore-io/tests/conformance.rs` — the semantic suite every backend passes.
