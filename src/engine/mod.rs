//! The memory test engine: allocates as much RAM as is safe, writes known
//! patterns into it, reads them back and reports every word that changed.
//!
//! A run happens on a background thread started by [`start`]. The returned
//! [`TestHandle`] lets a GUI poll [`Progress`], ask the run to stop, and
//! collect the final [`TestSummary`].
//!
//! One pass runs each pattern in [`PATTERN_NAMES`] order. For each pattern the
//! whole of test memory is written first and only then verified, so every
//! word sits in RAM for a while (and is evicted from the CPU caches by the
//! rest of the writes) before it is read back.

mod alloc;
mod patterns;

use std::hash::BuildHasher;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::types::{RunEnd, TestSummary};
use alloc::{Arena, Chunk};
use patterns::{Block, ErrorLog, Pattern, verify_block, write_block};

pub use alloc::total_physical_bytes;
pub use patterns::MAX_RECORDED_ERRORS;

/// Names of the test patterns, in the order one pass runs them. These are the
/// values [`Progress::pattern`] and [`MemError::pattern`](crate::types::MemError::pattern)
/// take.
pub const PATTERN_NAMES: [&str; 9] = [
    "All zeros",
    "All ones",
    "Checkerboard",
    "Inverse checkerboard",
    "Walking ones",
    "Walking zeros",
    "Own address",
    "Random",
    "Inverse random",
];

/// What to run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestConfig {
    /// Number of passes; `None` means keep going until stopped. `Some(0)` is
    /// treated as one pass.
    pub passes: Option<u32>,
    /// Upper limit on bytes to test; `None` means "as much as is safe".
    /// Rounded down to whole 4 KiB pages, so a limit under 4 KiB allocates
    /// nothing and the run ends with [`RunEnd::AllocationFailed`].
    pub max_bytes: Option<u64>,
    /// Worker threads; `None` means one per available CPU. `Some(0)` is
    /// treated as one.
    pub threads: Option<usize>,
    /// When true, deliberately corrupt one word during the run so the failure
    /// path can be demonstrated and tested without faulty hardware.
    pub simulate_fault: bool,
}

impl Default for TestConfig {
    /// One pass over as much memory as is safe, one thread per CPU, no
    /// simulated fault.
    fn default() -> Self {
        TestConfig {
            passes: Some(1),
            max_bytes: None,
            threads: None,
            simulate_fault: false,
        }
    }
}

/// What a run is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Getting (and on Windows locking) memory. With tens of GiB this takes a
    /// while, because locking touches every page.
    Allocating,
    /// Writing and verifying patterns.
    Testing,
    /// The run is over and its memory has been given back; [`TestHandle::join`]
    /// returns without waiting.
    Finished,
}

/// A point-in-time view of a running test, cheap to take.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Progress {
    /// What the run is doing.
    pub phase: Phase,
    /// Current pass, starting at 1 (0 while allocating).
    pub pass: u32,
    /// Total passes requested, `None` when running until stopped.
    pub total_passes: Option<u32>,
    /// Name of the pattern currently running, e.g. "Checkerboard". Empty while allocating.
    pub pattern: &'static str,
    /// Fraction of the current pass done, 0.0..=1.0.
    pub pass_fraction: f32,
    /// Fraction of the whole run done, 0.0..=1.0; `None` when running until stopped.
    pub overall_fraction: Option<f32>,
    /// Bytes under test (0 until allocation finishes).
    pub tested_bytes: u64,
    /// Failing words seen so far.
    pub error_count: u64,
    /// Time since the run started.
    pub elapsed: Duration,
}

/// A running (or finished) test started by [`start`]. Dropping the handle
/// without calling [`join`](TestHandle::join) asks the run to stop, so its
/// memory is given back soon.
#[derive(Debug)]
pub struct TestHandle {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<TestSummary>>,
}

/// Starts a run on a background thread and returns immediately.
pub fn start(config: TestConfig) -> TestHandle {
    start_with(config, Hooks::default())
}

impl TestHandle {
    /// A snapshot of the run's progress.
    pub fn progress(&self) -> Progress {
        self.shared.progress()
    }

    /// Asks the run to stop soon. Returns immediately.
    pub fn stop(&self) {
        self.shared.stop.store(true, Ordering::Relaxed);
    }

    /// Whether the run is over (its memory freed and its result ready).
    pub fn is_finished(&self) -> bool {
        self.shared.phase() == Phase::Finished
    }

    /// Waits for the run to end and returns its result.
    ///
    /// If the run was cut short by an internal failure (a panic in the engine,
    /// which also prints a message to stderr) the summary reports
    /// [`RunEnd::Stopped`] with whatever had been found until then.
    pub fn join(mut self) -> TestSummary {
        match self.thread.take().map(JoinHandle::join) {
            Some(Ok(summary)) => summary,
            // The controller thread panicked or could not be started.
            _ => self.shared.fallback_summary(),
        }
    }
}

impl Drop for TestHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Test-only fault injection; empty in normal builds.
#[derive(Debug, Default, Clone, Copy)]
struct Hooks {
    /// Makes a worker thread panic on its first block.
    #[cfg(test)]
    panic_in_worker: bool,
}

/// State shared between the handle, the run's controller thread and its
/// workers.
///
/// Progress is derived from one counter, `work_done`, so a snapshot is always
/// self-consistent: each pass processes `pass_work` bytes (every pattern
/// writes and then verifies all test memory), and the pass, pattern and
/// fractions all follow from how much work is done.
#[derive(Debug)]
struct Shared {
    /// Set by [`TestHandle::stop`].
    stop: AtomicBool,
    /// One of the `PHASE_*` constants.
    phase: AtomicU8,
    started: Instant,
    /// Passes requested, `None` for "until stopped".
    total_passes: Option<u32>,
    /// Bytes processed per pass; 0 until allocation has finished.
    pass_work: AtomicU64,
    /// Bytes processed since testing began. Only ever grows.
    work_done: AtomicU64,
    tested_bytes: AtomicU64,
    locked_bytes: AtomicU64,
    errors: ErrorLog,
    /// Duration of the run, fixed once it is over.
    final_elapsed: Mutex<Option<Duration>>,
    /// The test memory as address ranges, set when allocation is done.
    #[cfg(test)]
    regions: Mutex<Option<Vec<std::ops::Range<usize>>>>,
    /// Bytes the controller thread still had allocated when it finished.
    #[cfg(test)]
    left_allocated: Mutex<Option<u64>>,
}

const PHASE_ALLOCATING: u8 = 0;
const PHASE_TESTING: u8 = 1;
const PHASE_FINISHED: u8 = 2;

/// Number of write-or-verify sweeps over test memory in one pass.
const SWEEPS_PER_PASS: u64 = 2 * PATTERN_NAMES.len() as u64;

/// Size of the unit of work handed to a worker. Small enough that workers
/// notice a stop request within a millisecond or so, large enough that
/// handing out blocks costs nothing measurable.
const BLOCK_WORDS: usize = (1 << 20) / 8;

/// Bit flipped by [`TestConfig::simulate_fault`].
const SIMULATED_FAULT_BIT: u32 = 0;

impl Shared {
    fn new(total_passes: Option<u32>) -> Shared {
        Shared {
            stop: AtomicBool::new(false),
            phase: AtomicU8::new(PHASE_ALLOCATING),
            started: Instant::now(),
            total_passes,
            pass_work: AtomicU64::new(0),
            work_done: AtomicU64::new(0),
            tested_bytes: AtomicU64::new(0),
            locked_bytes: AtomicU64::new(0),
            errors: ErrorLog::default(),
            final_elapsed: Mutex::new(None),
            #[cfg(test)]
            regions: Mutex::new(None),
            #[cfg(test)]
            left_allocated: Mutex::new(None),
        }
    }

    fn phase(&self) -> Phase {
        match self.phase.load(Ordering::Acquire) {
            PHASE_ALLOCATING => Phase::Allocating,
            PHASE_TESTING => Phase::Testing,
            _ => Phase::Finished,
        }
    }

    fn stopping(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }

    fn progress(&self) -> Progress {
        // Read the clock before the frozen value, so a run finishing between
        // the two reads cannot make elapsed time jump backwards.
        let now = self.started.elapsed();
        let elapsed = lock(&self.final_elapsed).unwrap_or(now);
        let phase = self.phase();
        let error_count = self.errors.count();
        let pass_work = self.pass_work.load(Ordering::Acquire);
        if phase == Phase::Allocating || pass_work == 0 {
            return Progress {
                phase,
                pass: 0,
                total_passes: self.total_passes,
                pattern: "",
                pass_fraction: 0.0,
                overall_fraction: self.total_passes.map(|_| 0.0),
                tested_bytes: 0,
                error_count,
                elapsed,
            };
        }
        let done = self.work_done.load(Ordering::Relaxed);
        let (mut passes_done, mut within) = (done / pass_work, done % pass_work);
        if within == 0 && passes_done > 0 {
            // Exactly at the end of a pass: show it as finished rather than
            // as the start of a pass that may never run.
            passes_done -= 1;
            within = pass_work;
        }
        let pattern_work = pass_work / PATTERN_NAMES.len() as u64;
        let pattern_index = usize::try_from(within / pattern_work)
            .unwrap_or(usize::MAX)
            .min(PATTERN_NAMES.len() - 1);
        Progress {
            phase,
            pass: u32::try_from(passes_done + 1).unwrap_or(u32::MAX),
            total_passes: self.total_passes,
            pattern: PATTERN_NAMES[pattern_index],
            pass_fraction: (within as f64 / pass_work as f64) as f32,
            overall_fraction: self
                .total_passes
                .map(|n| (done as f64 / (pass_work as f64 * f64::from(n))).min(1.0) as f32),
            tested_bytes: self.tested_bytes.load(Ordering::Relaxed),
            error_count,
            elapsed,
        }
    }

    /// Fixes the run's duration (first call wins) and returns it.
    fn freeze_elapsed(&self) -> Duration {
        *lock(&self.final_elapsed).get_or_insert_with(|| self.started.elapsed())
    }

    fn summary(&self, end: RunEnd, passes_completed: u32) -> TestSummary {
        TestSummary {
            end,
            tested_bytes: self.tested_bytes.load(Ordering::Relaxed),
            locked_bytes: self.locked_bytes.load(Ordering::Relaxed),
            passes_completed,
            error_count: self.errors.count(),
            errors: self.errors.errors(),
            elapsed: self.freeze_elapsed(),
        }
    }

    /// The result when the controller thread did not return one.
    fn fallback_summary(&self) -> TestSummary {
        let pass_work = self.pass_work.load(Ordering::Relaxed);
        if pass_work == 0 {
            return self.summary(RunEnd::AllocationFailed, 0);
        }
        let passes = self.work_done.load(Ordering::Relaxed) / pass_work;
        self.summary(RunEnd::Stopped, u32::try_from(passes).unwrap_or(u32::MAX))
    }
}

/// Locks a mutex, ignoring poisoning: the values guarded here stay valid
/// even if a thread panicked while holding the lock.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
impl Shared {
    /// Waits until allocation is done and returns the test memory ranges.
    fn wait_for_regions(&self) -> Vec<std::ops::Range<usize>> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(regions) = lock(&self.regions).clone() {
                return regions;
            }
            assert!(Instant::now() < deadline, "allocation never finished");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// Bytes still allocated when the run ended; `None` until it has ended.
    fn bytes_left_allocated(&self) -> Option<u64> {
        *lock(&self.left_allocated)
    }
}

/// Marks the run finished when the controller thread exits, normally or by
/// panicking. Created before the run's memory, so it is dropped after it.
struct FinishGuard<'a>(&'a Shared);

impl Drop for FinishGuard<'_> {
    fn drop(&mut self) {
        self.0.freeze_elapsed();
        #[cfg(test)]
        {
            *lock(&self.0.left_allocated) = Some(alloc::live_bytes_on_this_thread());
        }
        self.0.phase.store(PHASE_FINISHED, Ordering::Release);
    }
}

fn start_with(config: TestConfig, hooks: Hooks) -> TestHandle {
    let shared = Arc::new(Shared::new(config.passes.map(|n| n.max(1))));
    let controller_shared = Arc::clone(&shared);
    let spawned = std::thread::Builder::new()
        .name("memory-test".into())
        .spawn(move || {
            let _finish = FinishGuard(&controller_shared);
            run(&config, &controller_shared, hooks)
        });
    let thread = match spawned {
        Ok(thread) => Some(thread),
        Err(_) => {
            // No thread, so nothing was allocated; `join` reports that.
            drop(FinishGuard(&shared));
            None
        }
    };
    TestHandle { shared, thread }
}

/// The body of the controller thread: allocates, tests, frees.
fn run(config: &TestConfig, shared: &Shared, hooks: Hooks) -> TestSummary {
    let arena = Arena::allocate(config.max_bytes, &shared.stop);
    test_arena(&arena, config, shared, hooks)
    // `arena` is freed here, before the run is marked finished.
}

fn test_arena(arena: &Arena, config: &TestConfig, shared: &Shared, hooks: Hooks) -> TestSummary {
    let tested = arena.total_bytes();
    if shared.stopping() {
        // Stopped while allocating: nothing was tested.
        return shared.summary(RunEnd::Stopped, 0);
    }
    if tested == 0 {
        return shared.summary(RunEnd::AllocationFailed, 0);
    }
    let blocks = split_into_blocks(arena.chunks());
    #[cfg(test)]
    {
        let ranges = arena.chunks().iter().map(|c| {
            let start = c.ptr() as usize;
            start..start + c.bytes()
        });
        *lock(&shared.regions) = Some(ranges.collect());
    }
    shared.tested_bytes.store(tested, Ordering::Relaxed);
    shared
        .locked_bytes
        .store(arena.locked_bytes(), Ordering::Relaxed);
    shared
        .pass_work
        .store(tested * SWEEPS_PER_PASS, Ordering::Release);
    shared.phase.store(PHASE_TESTING, Ordering::Release);

    let seed = run_seed();
    let threads = config
        .threads
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(1, |n| n.get()))
        .clamp(1, blocks.len());

    let mut passes_completed = 0u32;
    'passes: while shared.total_passes != Some(passes_completed) {
        let Some(pass) = passes_completed.checked_add(1) else {
            break;
        };
        for pattern in Pattern::ALL {
            // SAFETY (this closure and `verify` below): every block lies
            // inside `arena`, which stays borrowed, and so allocated, for the
            // whole of this function; `run_sweep` hands each block to exactly
            // one worker per sweep, so no two threads touch the same words at
            // once.
            let write = |b: &Block| unsafe { write_block(pattern, b, pass, seed) };
            if !run_sweep(shared, &blocks, threads, hooks, write) {
                break 'passes;
            }
            if config.simulate_fault && pass == 1 && pattern == Pattern::ALL[0] {
                flip_one_bit(&blocks);
            }
            let verify = |b: &Block| {
                // SAFETY: see above.
                unsafe { verify_block(pattern, b, pass, seed, &shared.errors) };
            };
            if !run_sweep(shared, &blocks, threads, hooks, verify) {
                break 'passes;
            }
        }
        passes_completed = pass;
    }

    let end = if shared.total_passes == Some(passes_completed) {
        RunEnd::Completed
    } else {
        RunEnd::Stopped
    };
    shared.summary(end, passes_completed)
}

/// Cuts the chunks into blocks of at most [`BLOCK_WORDS`] words, numbering
/// words and blocks across all of test memory.
fn split_into_blocks(chunks: &[Chunk]) -> Vec<Block> {
    let mut blocks = Vec::new();
    let mut first_word = 0u64;
    for chunk in chunks {
        let words = chunk.bytes() / 8;
        let mut offset = 0;
        while offset < words {
            let len = (words - offset).min(BLOCK_WORDS);
            blocks.push(Block {
                ptr: chunk.ptr().wrapping_add(offset),
                words: len,
                first_word,
                index: blocks.len() as u64,
            });
            first_word += len as u64;
            offset += len;
        }
    }
    blocks
}

/// Applies `work` to every block, sharing the blocks between `threads`
/// threads (the calling thread is one of them). Returns `true` only if every
/// block was done: `false` means a stop was requested or a worker panicked.
fn run_sweep(
    shared: &Shared,
    blocks: &[Block],
    threads: usize,
    hooks: Hooks,
    work: impl Fn(&Block) + Sync,
) -> bool {
    let next = AtomicUsize::new(0);
    let done = AtomicUsize::new(0);
    let worker = || {
        while !shared.stopping() {
            let Some(block) = blocks.get(next.fetch_add(1, Ordering::Relaxed)) else {
                break;
            };
            #[cfg(test)]
            if hooks.panic_in_worker {
                panic!("simulated worker panic");
            }
            work(block);
            done.fetch_add(1, Ordering::Relaxed);
            shared
                .work_done
                .fetch_add(block.words as u64 * 8, Ordering::Relaxed);
        }
    };
    #[cfg(not(test))]
    let _ = hooks;

    let no_panic = std::thread::scope(|scope| {
        // Threads that cannot be created are simply missing; the others,
        // including this one, share their blocks.
        let helpers: Vec<_> = (1..threads)
            .filter_map(|_| {
                std::thread::Builder::new()
                    .name("memory-test-worker".into())
                    .spawn_scoped(scope, worker)
                    .ok()
            })
            .collect();
        let mut no_panic = catch_unwind(AssertUnwindSafe(worker)).is_ok();
        for helper in helpers {
            no_panic &= helper.join().is_ok();
        }
        no_panic
    });
    no_panic && done.load(Ordering::Relaxed) == blocks.len()
}

/// Flips one bit of one word in the middle of test memory, for
/// [`TestConfig::simulate_fault`]. Must only run between sweeps, while no
/// worker is touching memory.
fn flip_one_bit(blocks: &[Block]) {
    let Some(block) = blocks.get(blocks.len() / 2) else {
        return;
    };
    // SAFETY: blocks are never empty, so `words / 2` is inside the block,
    // which lies in live test memory; no sweep is running, so nothing else
    // accesses it.
    unsafe {
        let word = block.ptr.add(block.words / 2);
        word.write_volatile(word.read_volatile() ^ (1 << SIMULATED_FAULT_BIT));
    }
}

/// A seed for the random patterns that differs from run to run.
fn run_seed() -> u64 {
    // `RandomState` is seeded from the operating system's random source.
    std::collections::hash_map::RandomState::new().hash_one(std::time::SystemTime::now())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::MemError;
    use std::thread::sleep;

    const MIB: u64 = 1 << 20;

    fn small(passes: Option<u32>) -> TestConfig {
        TestConfig {
            passes,
            max_bytes: Some(16 * MIB),
            ..TestConfig::default()
        }
    }

    /// Runs to the end while sampling progress, returning every sample (the
    /// last one taken after the run finished) and the summary.
    fn run_sampling(config: TestConfig) -> (Vec<Progress>, TestSummary) {
        let handle = start(config);
        let mut samples = Vec::new();
        loop {
            let p = handle.progress();
            samples.push(p);
            if p.phase == Phase::Finished {
                break;
            }
            sleep(Duration::from_millis(1));
        }
        (samples, handle.join())
    }

    #[test]
    fn default_config_is_one_pass_uncapped_auto_threads_no_fault() {
        assert_eq!(
            TestConfig::default(),
            TestConfig {
                passes: Some(1),
                max_bytes: None,
                threads: None,
                simulate_fault: false,
            }
        );
    }

    #[test]
    fn pattern_names_match_the_order_the_engine_runs() {
        let names: Vec<&str> = patterns::Pattern::ALL.iter().map(|p| p.name()).collect();
        assert_eq!(names, PATTERN_NAMES);
    }

    #[test]
    fn small_run_completes_clean_and_ends_at_full_progress() {
        let _serial = alloc::test_alloc_lock();
        let (samples, summary) = run_sampling(small(Some(1)));
        assert_eq!(summary.end, RunEnd::Completed);
        assert_eq!(summary.error_count, 0);
        assert!(summary.errors.is_empty());
        assert_eq!(summary.tested_bytes, 16 * MIB);
        assert_eq!(summary.passes_completed, 1);
        #[cfg(not(windows))]
        assert_eq!(summary.locked_bytes, 0);

        let last = samples.last().unwrap();
        assert_eq!(last.phase, Phase::Finished);
        assert_eq!(last.pass, 1);
        assert_eq!(last.total_passes, Some(1));
        assert_eq!(last.pass_fraction, 1.0);
        assert_eq!(last.overall_fraction, Some(1.0));
        assert_eq!(last.tested_bytes, 16 * MIB);
        assert_eq!(last.error_count, 0);
        assert_eq!(last.pattern, "Inverse random");
        assert_eq!(last.elapsed, summary.elapsed);
    }

    #[test]
    fn progress_never_goes_backwards() {
        let _serial = alloc::test_alloc_lock();
        let (samples, summary) = run_sampling(small(Some(3)));
        assert_eq!(summary.end, RunEnd::Completed);
        assert_eq!(summary.passes_completed, 3);
        for pair in samples.windows(2) {
            let (a, b) = (pair[0], pair[1]);
            assert!(b.pass >= a.pass, "{a:?} -> {b:?}");
            assert!(b.overall_fraction >= a.overall_fraction, "{a:?} -> {b:?}");
            assert!(b.elapsed >= a.elapsed, "{a:?} -> {b:?}");
            if a.pass == b.pass {
                assert!(b.pass_fraction >= a.pass_fraction, "{a:?} -> {b:?}");
            }
            let ia = PATTERN_NAMES.iter().position(|n| *n == a.pattern);
            let ib = PATTERN_NAMES.iter().position(|n| *n == b.pattern);
            if a.pass == b.pass && a.phase == Phase::Testing {
                assert!(ib >= ia, "{a:?} -> {b:?}");
            }
        }
        for p in &samples {
            assert!((0.0..=1.0).contains(&p.pass_fraction), "{p:?}");
            assert!((0.0..=1.0).contains(&p.overall_fraction.unwrap()), "{p:?}");
            if p.phase == Phase::Allocating {
                assert_eq!((p.pass, p.pattern, p.tested_bytes), (0, "", 0));
            } else {
                assert!((1..=3).contains(&p.pass), "{p:?}");
                assert!(PATTERN_NAMES.contains(&p.pattern), "{p:?}");
            }
        }
        // The sampling saw the run move through more than one pass.
        assert!(samples.iter().any(|p| p.pass == 2), "too few samples");
    }

    #[test]
    fn simulated_fault_is_reported_as_one_flipped_bit_inside_test_memory() {
        let _serial = alloc::test_alloc_lock();
        let config = TestConfig {
            simulate_fault: true,
            ..small(Some(1))
        };
        let handle = start(config);
        let regions = handle.shared.wait_for_regions();
        let summary = handle.join();

        assert_eq!(summary.end, RunEnd::Completed);
        assert_eq!(summary.error_count, 1);
        let [error]: [MemError; 1] = summary.errors.try_into().unwrap();
        assert_eq!(error.flipped_bits().count_ones(), 1);
        assert_eq!(error.pass, 1);
        assert_eq!(error.pattern, "All zeros");
        assert_eq!(error.expected, 0);
        assert_eq!(error.address % 8, 0);
        assert!(
            regions
                .iter()
                .any(|r| r.contains(&error.address) && r.contains(&(error.address + 7))),
            "{:#x} not in {regions:x?}",
            error.address
        );
    }

    #[test]
    fn without_simulate_fault_nothing_is_corrupted_over_several_passes() {
        let _serial = alloc::test_alloc_lock();
        let summary = start(TestConfig {
            threads: Some(1),
            ..small(Some(2))
        })
        .join();
        assert_eq!(summary.end, RunEnd::Completed);
        assert_eq!(summary.passes_completed, 2);
        assert_eq!(summary.error_count, 0);
    }

    #[test]
    fn endless_run_stops_promptly_when_asked() {
        let _serial = alloc::test_alloc_lock();
        let handle = start(small(None));
        // Let it get well into testing first.
        while handle.progress().phase != Phase::Testing {
            sleep(Duration::from_millis(1));
        }
        sleep(Duration::from_millis(200));
        let p = handle.progress();
        assert_eq!((p.total_passes, p.overall_fraction), (None, None));

        let asked = Instant::now();
        handle.stop();
        while !handle.is_finished() {
            assert!(asked.elapsed() < Duration::from_secs(1), "did not stop");
            sleep(Duration::from_millis(1));
        }
        let summary = handle.join();
        assert!(
            asked.elapsed() < Duration::from_millis(500),
            "took {:?}",
            asked.elapsed()
        );
        assert_eq!(summary.end, RunEnd::Stopped);
        assert_eq!(summary.tested_bytes, 16 * MIB);
    }

    #[test]
    fn passes_completed_counts_only_whole_passes_when_stopped() {
        let _serial = alloc::test_alloc_lock();
        let handle = start(small(Some(1000)));
        while handle.progress().pass < 2 {
            sleep(Duration::from_millis(1));
        }
        handle.stop();
        let summary = handle.join();
        assert_eq!(summary.end, RunEnd::Stopped);
        assert!(summary.passes_completed >= 1);
        assert!(summary.passes_completed < 1000);
    }

    #[test]
    fn stopping_straight_away_reports_stopped() {
        let _serial = alloc::test_alloc_lock();
        let handle = start(small(Some(1)));
        handle.stop();
        let summary = handle.join();
        assert_eq!(summary.end, RunEnd::Stopped);
        assert_eq!(summary.passes_completed, 0);
    }

    #[test]
    fn nothing_allocatable_ends_with_allocation_failed() {
        let _serial = alloc::test_alloc_lock();
        let (samples, summary) = run_sampling(TestConfig {
            max_bytes: Some(0),
            ..TestConfig::default()
        });
        assert_eq!(summary.end, RunEnd::AllocationFailed);
        assert_eq!((summary.tested_bytes, summary.locked_bytes), (0, 0));
        assert_eq!(summary.passes_completed, 0);
        assert_eq!(samples.last().unwrap().phase, Phase::Finished);
    }

    #[test]
    fn a_worker_panic_ends_the_run_without_hanging_or_leaking() {
        let _serial = alloc::test_alloc_lock();
        let hooks = Hooks {
            panic_in_worker: true,
        };
        let handle = start_with(small(Some(1)), hooks);
        let shared = Arc::clone(&handle.shared);
        let summary = handle.join();
        assert_eq!(summary.end, RunEnd::Stopped);
        assert_eq!(summary.passes_completed, 0);
        assert_eq!(shared.phase(), Phase::Finished);
        assert_eq!(shared.bytes_left_allocated(), Some(0));
    }

    #[test]
    fn a_clean_run_frees_all_its_memory() {
        let _serial = alloc::test_alloc_lock();
        let handle = start(small(Some(1)));
        let shared = Arc::clone(&handle.shared);
        assert_eq!(handle.join().end, RunEnd::Completed);
        assert_eq!(shared.bytes_left_allocated(), Some(0));
    }

    #[cfg(windows)]
    #[test]
    fn windows_run_locks_memory_and_can_run_again() {
        let _serial = alloc::test_alloc_lock();
        for attempt in 1..=2 {
            let summary = start(TestConfig {
                max_bytes: Some(64 * MIB),
                ..TestConfig::default()
            })
            .join();
            assert_eq!(summary.end, RunEnd::Completed, "run {attempt}");
            assert_eq!(summary.error_count, 0, "run {attempt}");
            assert_eq!(summary.tested_bytes, 64 * MIB, "run {attempt}");
            assert!(summary.locked_bytes > 0, "run {attempt}");
        }
    }
}
