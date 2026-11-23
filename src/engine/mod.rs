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
use std::sync::{Arc, Mutex, MutexGuard, TryLockError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::types::{RunEnd, TestSummary};
use alloc::{AllocContext, Arena, Chunk};
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
    ///
    /// While the phase is [`Phase::Allocating`] (which can take tens of
    /// seconds when locking tens of GiB) this instead reports the fraction of
    /// the memory being aimed for that has been obtained so far. It never goes
    /// backwards within that phase, but may stop short of 1.0 when the system
    /// gives less than was aimed for. It starts again from 0.0 for pass 1.
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
///
/// Only one run in the process holds memory at a time. If an earlier run is
/// still going or still giving its memory back (for example its handle was
/// just dropped), the new run waits in [`Phase::Allocating`] until that has
/// finished, so it sees the memory as free and can lock it. [`TestHandle::stop`]
/// still works while it waits.
pub fn start(config: TestConfig) -> TestHandle {
    start_with(config, RunOptions::default())
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

/// Internal settings of a run that are not part of [`TestConfig`]: sizes
/// that tests shrink, and test-only fault injection.
#[derive(Debug, Clone, Copy)]
struct RunOptions {
    /// Largest allocation chunk.
    chunk_bytes: usize,
    /// Largest unit of work handed to a worker, in words.
    block_words: usize,
    /// Makes a worker thread panic on its first block.
    #[cfg(test)]
    panic_in_worker: bool,
    /// The system refuses every allocation after this many.
    #[cfg(test)]
    refuse_alloc_after: Option<usize>,
}

impl Default for RunOptions {
    fn default() -> Self {
        RunOptions {
            chunk_bytes: alloc::CHUNK_BYTES,
            block_words: BLOCK_WORDS,
            #[cfg(test)]
            panic_in_worker: false,
            #[cfg(test)]
            refuse_alloc_after: None,
        }
    }
}

/// Held by the controller thread of the one run that may hold memory; see
/// [`start`].
static RUN_SLOT: Mutex<()> = Mutex::new(());

/// Waits for [`RUN_SLOT`], giving up (returning `None`) if a stop is
/// requested first.
fn wait_for_run_slot(shared: &Shared) -> Option<MutexGuard<'static, ()>> {
    loop {
        match RUN_SLOT.try_lock() {
            Ok(slot) => return Some(slot),
            // A run that panicked still released its memory while unwinding.
            Err(TryLockError::Poisoned(p)) => return Some(p.into_inner()),
            Err(TryLockError::WouldBlock) => {}
        }
        if shared.stopping() {
            return None;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
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
    /// Bytes the allocation is aiming for (for progress while allocating).
    alloc_target: AtomicU64,
    /// Bytes obtained so far by the allocation. Only ever grows.
    alloc_obtained: AtomicU64,
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
            alloc_target: AtomicU64::new(0),
            alloc_obtained: AtomicU64::new(0),
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
            // Read the target first: it only changes while nothing has been
            // obtained, so the fraction cannot appear to go backwards.
            let target = self.alloc_target.load(Ordering::Relaxed);
            let obtained = self.alloc_obtained.load(Ordering::Relaxed);
            let fraction = if target == 0 {
                0.0
            } else {
                (obtained as f64 / target as f64).min(1.0) as f32
            };
            return Progress {
                phase,
                pass: 0,
                total_passes: self.total_passes,
                pattern: "",
                pass_fraction: fraction,
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

fn start_with(config: TestConfig, options: RunOptions) -> TestHandle {
    let shared = Arc::new(Shared::new(config.passes.map(|n| n.max(1))));
    let controller_shared = Arc::clone(&shared);
    let spawned = std::thread::Builder::new()
        .name("memory-test".into())
        .spawn(move || {
            let shared = &*controller_shared;
            // Locals drop in reverse order: the arena inside `run` first, then
            // `_finish` marks the run finished, and only then is the run slot
            // released for the next run.
            let slot = wait_for_run_slot(shared);
            let _finish = FinishGuard(shared);
            if slot.is_none() {
                // Stopped while waiting for an earlier run: nothing tested.
                return shared.summary(RunEnd::Stopped, 0);
            }
            run(&config, shared, options)
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
fn run(config: &TestConfig, shared: &Shared, options: RunOptions) -> TestSummary {
    let ctx = AllocContext {
        chunk_bytes: options.chunk_bytes,
        stop: &shared.stop,
        target: &shared.alloc_target,
        obtained: &shared.alloc_obtained,
        #[cfg(test)]
        refuse_after: options.refuse_alloc_after,
    };
    let arena = Arena::allocate(config.max_bytes, &ctx);
    test_arena(&arena, config, shared, options)
    // `arena` is freed here, before the run is marked finished.
}

fn test_arena(
    arena: &Arena,
    config: &TestConfig,
    shared: &Shared,
    options: RunOptions,
) -> TestSummary {
    let tested = arena.total_bytes();
    if shared.stopping() {
        // Stopped while allocating: nothing was tested.
        return shared.summary(RunEnd::Stopped, 0);
    }
    if tested == 0 {
        return shared.summary(RunEnd::AllocationFailed, 0);
    }
    let blocks = split_into_blocks(arena.chunks(), options.block_words);
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
            if !run_sweep(shared, &blocks, threads, &options, write) {
                break 'passes;
            }
            if config.simulate_fault && pass == 1 && pattern == Pattern::ALL[0] {
                flip_one_bit(&blocks);
            }
            let verify = |b: &Block| {
                // SAFETY: see above.
                unsafe { verify_block(pattern, b, pass, seed, &shared.errors) };
            };
            if !run_sweep(shared, &blocks, threads, &options, verify) {
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

/// Cuts the chunks into blocks of at most `block_words` words (at least 1),
/// numbering words and blocks across all of test memory. The last block of a
/// chunk is shorter when the chunk is not a whole number of blocks.
fn split_into_blocks(chunks: &[Chunk], block_words: usize) -> Vec<Block> {
    let block_words = block_words.max(1);
    let mut blocks = Vec::new();
    let mut first_word = 0u64;
    for chunk in chunks {
        let words = chunk.bytes() / 8;
        let mut offset = 0;
        while offset < words {
            let len = (words - offset).min(block_words);
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
    options: &RunOptions,
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
            if options.panic_in_worker {
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
    let _ = options;

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

    /// Longest any test waits for a run before failing.
    const PATIENCE: Duration = Duration::from_secs(30);

    /// Polls `handle` until `done` holds and returns that sample. Fails
    /// (instead of hanging) if the run finishes first or [`PATIENCE`] runs
    /// out.
    fn wait_until(handle: &TestHandle, what: &str, done: impl Fn(&Progress) -> bool) -> Progress {
        let deadline = Instant::now() + PATIENCE;
        loop {
            let p = handle.progress();
            if done(&p) {
                return p;
            }
            assert_ne!(p.phase, Phase::Finished, "run ended before {what}: {p:?}");
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {what}: {p:?}"
            );
            sleep(Duration::from_millis(1));
        }
    }

    /// Joins `handle`, failing instead of hanging if it does not finish
    /// within [`PATIENCE`].
    fn finish(handle: TestHandle) -> TestSummary {
        wait_until(&handle, "the end of the run", |p| {
            p.phase == Phase::Finished
        });
        handle.join()
    }

    /// Runs to the end while sampling progress, returning every sample (the
    /// last one taken after the run finished) and the summary.
    fn run_sampling(config: TestConfig) -> (Vec<Progress>, TestSummary) {
        let handle = start(config);
        let deadline = Instant::now() + PATIENCE;
        let mut samples = Vec::new();
        loop {
            let p = handle.progress();
            samples.push(p);
            if p.phase == Phase::Finished {
                break;
            }
            assert!(Instant::now() < deadline, "run did not finish: {p:?}");
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
        wait_until(&handle, "testing", |p| p.phase == Phase::Testing);
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
        let shared = Arc::clone(&handle.shared);
        wait_until(&handle, "pass 2", |p| p.pass >= 2);
        handle.stop();
        let summary = finish(handle);
        assert_eq!(summary.end, RunEnd::Stopped);
        // The pass that was interrupted is not counted: exactly the passes
        // whose work was all done are.
        let pass_work = shared.pass_work.load(Ordering::Relaxed);
        let whole_passes = shared.work_done.load(Ordering::Relaxed) / pass_work;
        assert!(whole_passes >= 1);
        assert_eq!(u64::from(summary.passes_completed), whole_passes);
    }

    #[test]
    fn simulated_fault_happens_only_in_the_first_pass() {
        let _serial = alloc::test_alloc_lock();
        let summary = finish(start(TestConfig {
            simulate_fault: true,
            ..small(Some(3))
        }));
        assert_eq!(summary.end, RunEnd::Completed);
        assert_eq!(summary.passes_completed, 3);
        assert_eq!(summary.error_count, 1);
        assert_eq!(summary.errors.len(), 1);
        assert_eq!(summary.errors[0].pass, 1);
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
        let options = RunOptions {
            panic_in_worker: true,
            ..RunOptions::default()
        };
        let handle = start_with(small(Some(1)), options);
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

    /// Small sizes for the multi-chunk tests: three 64 KiB chunks and a
    /// 12 KiB one, cut into 24 KiB blocks, so every 64 KiB chunk ends in a
    /// 16 KiB tail block and the last chunk is a single 12 KiB block.
    const SMALL_CHUNK: usize = 64 * 1024;
    const SMALL_BLOCK_WORDS: usize = 24 * 1024 / 8;
    const UNEVEN_BYTES: u64 = 3 * SMALL_CHUNK as u64 + 12 * 1024;

    fn small_sizes() -> RunOptions {
        RunOptions {
            chunk_bytes: SMALL_CHUNK,
            block_words: SMALL_BLOCK_WORDS,
            ..RunOptions::default()
        }
    }

    /// Allocates [`UNEVEN_BYTES`] in small chunks on the calling thread.
    fn uneven_arena() -> Arena {
        let (stop, target, obtained) =
            (AtomicBool::new(false), AtomicU64::new(0), AtomicU64::new(0));
        let ctx = AllocContext {
            chunk_bytes: SMALL_CHUNK,
            stop: &stop,
            target: &target,
            obtained: &obtained,
            refuse_after: None,
        };
        let arena = Arena::allocate(Some(UNEVEN_BYTES), &ctx);
        let sizes: Vec<usize> = arena.chunks().iter().map(|c| c.bytes()).collect();
        assert_eq!(sizes, [SMALL_CHUNK, SMALL_CHUNK, SMALL_CHUNK, 12 * 1024]);
        arena
    }

    #[test]
    fn blocks_tile_every_chunk_and_number_words_across_chunks() {
        let _serial = alloc::test_alloc_lock();
        let arena = uneven_arena();
        let blocks = split_into_blocks(arena.chunks(), SMALL_BLOCK_WORDS);
        let lens: Vec<usize> = blocks.iter().map(|b| b.words).collect();
        assert_eq!(
            lens,
            [3072, 3072, 2048, 3072, 3072, 2048, 3072, 3072, 2048, 1536]
        );
        let mut next = blocks.iter();
        let mut word = 0u64;
        for chunk in arena.chunks() {
            // The chunk's blocks start at its first word and follow each
            // other with no gap or overlap up to its end.
            let mut at = chunk.ptr();
            let end = chunk.ptr().wrapping_add(chunk.bytes() / 8);
            while at < end {
                let b = next.next().expect("chunk not fully covered");
                assert_eq!(b.ptr, at);
                assert_eq!(b.first_word, word);
                at = at.wrapping_add(b.words);
                word += b.words as u64;
            }
            assert_eq!(at, end);
        }
        assert!(next.next().is_none());
        assert_eq!(word, UNEVEN_BYTES / 8);
        for (i, b) in blocks.iter().enumerate() {
            assert_eq!(b.index, i as u64);
        }
    }

    /// What the spec says word `global` (at `address`) holds for `pattern`
    /// in `pass`, for the patterns that do not depend on the seed.
    fn spec_value(pattern: Pattern, global: u64, address: u64, pass: u32) -> Option<u64> {
        let walking = 1u64 << ((global + u64::from(pass)) % 64);
        match pattern {
            Pattern::AllZeros => Some(0),
            Pattern::AllOnes => Some(u64::MAX),
            Pattern::Checkerboard => Some(0xAAAA_AAAA_AAAA_AAAA),
            Pattern::InverseCheckerboard => Some(0x5555_5555_5555_5555),
            Pattern::WalkingOnes => Some(walking),
            Pattern::WalkingZeros => Some(!walking),
            Pattern::OwnAddress => Some(address),
            Pattern::Random | Pattern::InverseRandom => None,
        }
    }

    #[test]
    fn every_word_of_uneven_multi_chunk_memory_is_written_and_verified() {
        let _serial = alloc::test_alloc_lock();
        let arena = uneven_arena();
        let chunks = arena.chunks();
        let blocks = split_into_blocks(chunks, SMALL_BLOCK_WORDS);
        let options = small_sizes();
        let seed = 0x5EED_0000_1234_5678;
        let last_word = |c: &Chunk| c.ptr().wrapping_add(c.bytes() / 8 - 1);
        // Chunk edges on both sides, the middle of a tail block, and the
        // first and last word of all test memory.
        let targets = [
            chunks[0].ptr(),
            chunks[0].ptr().wrapping_add(6144 + 1000),
            last_word(&chunks[0]),
            chunks[1].ptr(),
            last_word(&chunks[2]),
            chunks[3].ptr(),
            last_word(&chunks[3]),
        ];

        for pattern in Pattern::ALL {
            for pass in [1, 2, 64] {
                let shared = Shared::new(Some(1));
                // SAFETY: the blocks lie in `arena`, alive for the whole
                // test, and `run_sweep` gives each block to one worker.
                let write = |b: &Block| unsafe { write_block(pattern, b, pass, seed) };
                assert!(run_sweep(&shared, &blocks, 3, &options, write));

                let mut global = 0u64;
                for c in chunks {
                    for i in 0..c.bytes() / 8 {
                        let p = c.ptr().wrapping_add(i);
                        // SAFETY: `p` is inside a live chunk; no sweep runs.
                        let got = unsafe { p.read_volatile() };
                        if let Some(want) = spec_value(pattern, global, p as u64, pass) {
                            assert_eq!(got, want, "{} pass {pass} word {global}", pattern.name());
                        }
                        global += 1;
                    }
                }

                let mut planted = Vec::new();
                for (n, &p) in targets.iter().enumerate() {
                    // SAFETY: as above.
                    let written = unsafe { p.read_volatile() };
                    let bad = written ^ (1u64 << (n * 9 % 64));
                    // SAFETY: as above.
                    unsafe { p.write_volatile(bad) };
                    planted.push(MemError {
                        address: p as usize,
                        expected: written,
                        actual: bad,
                        pattern: pattern.name(),
                        pass,
                    });
                }

                let verify = |b: &Block| {
                    // SAFETY: as for `write`.
                    unsafe { verify_block(pattern, b, pass, seed, &shared.errors) };
                };
                assert!(run_sweep(&shared, &blocks, 3, &options, verify));
                // Chunks need not lie in address order, so compare sorted.
                let mut found = shared.errors.errors();
                found.sort_by_key(|e| e.address);
                planted.sort_by_key(|e| e.address);
                assert_eq!(found, planted, "{} pass {pass}", pattern.name());
                assert_eq!(shared.errors.count(), planted.len() as u64);
            }
        }
    }

    #[test]
    fn uneven_multi_chunk_run_tests_all_its_memory_end_to_end() {
        let _serial = alloc::test_alloc_lock();
        let (handle, shared) = {
            let h = start_with(small(Some(2)), small_sizes());
            let s = Arc::clone(&h.shared);
            (h, s)
        };
        let summary = finish(handle);
        assert_eq!(summary.end, RunEnd::Completed);
        assert_eq!(summary.error_count, 0);
        assert_eq!(summary.tested_bytes, 16 * MIB);
        assert_eq!(
            shared.work_done.load(Ordering::Relaxed),
            2 * SWEEPS_PER_PASS * 16 * MIB
        );

        let config = TestConfig {
            max_bytes: Some(UNEVEN_BYTES),
            simulate_fault: true,
            ..small(Some(1))
        };
        let summary = finish(start_with(config, small_sizes()));
        assert_eq!(summary.end, RunEnd::Completed);
        assert_eq!(summary.tested_bytes, UNEVEN_BYTES);
        // The fault lands in a tail block (block 5 of 10), and is found.
        assert_eq!(summary.error_count, 1);
    }

    #[test]
    fn an_allocation_refused_part_way_tests_what_was_obtained() {
        let _serial = alloc::test_alloc_lock();
        let options = RunOptions {
            refuse_alloc_after: Some(3),
            ..small_sizes()
        };
        let summary = finish(start_with(small(Some(1)), options));
        assert_eq!(summary.end, RunEnd::Completed);
        assert_eq!(summary.tested_bytes, 3 * SMALL_CHUNK as u64);
        assert_eq!(summary.passes_completed, 1);
        assert_eq!(summary.error_count, 0);

        let options = RunOptions {
            refuse_alloc_after: Some(0),
            ..small_sizes()
        };
        let summary = finish(start_with(small(Some(1)), options));
        assert_eq!(summary.end, RunEnd::AllocationFailed);
    }

    #[test]
    fn allocation_progress_is_reported_in_pass_fraction() {
        let shared = Shared::new(Some(2));
        let p = shared.progress();
        assert_eq!(
            (p.phase, p.pass, p.pass_fraction),
            (Phase::Allocating, 0, 0.0)
        );
        shared.alloc_target.store(400, Ordering::Relaxed);
        shared.alloc_obtained.store(100, Ordering::Relaxed);
        let p = shared.progress();
        assert_eq!((p.phase, p.pass, p.pattern), (Phase::Allocating, 0, ""));
        assert_eq!(p.pass_fraction, 0.25);
        assert_eq!(p.overall_fraction, Some(0.0));
        assert_eq!(p.tested_bytes, 0);
        shared.alloc_obtained.store(400, Ordering::Relaxed);
        assert_eq!(shared.progress().pass_fraction, 1.0);

        // A real run feeds the same counters.
        let _serial = alloc::test_alloc_lock();
        let handle = start(small(Some(1)));
        let shared = Arc::clone(&handle.shared);
        assert_eq!(finish(handle).end, RunEnd::Completed);
        assert_eq!(shared.alloc_target.load(Ordering::Relaxed), 16 * MIB);
        assert_eq!(shared.alloc_obtained.load(Ordering::Relaxed), 16 * MIB);
    }

    #[test]
    fn runs_wait_for_earlier_runs_and_can_be_stopped_while_waiting() {
        let _serial = alloc::test_alloc_lock();
        let first = start(small(None));
        let first_shared = Arc::clone(&first.shared);
        wait_until(&first, "testing", |p| p.phase == Phase::Testing);

        // A second run waits without allocating, and stops while waiting.
        let second = start(small(Some(1)));
        sleep(Duration::from_millis(100));
        assert_eq!(second.progress().phase, Phase::Allocating);
        assert_eq!(second.shared.alloc_target.load(Ordering::Relaxed), 0);
        second.stop();
        let summary = finish(second);
        assert_eq!(summary.end, RunEnd::Stopped);
        assert_eq!(summary.tested_bytes, 0);
        assert_eq!(first.progress().phase, Phase::Testing);

        // A third run starts only once the first, dropped without joining,
        // has released all its memory.
        let third = start(small(Some(1)));
        sleep(Duration::from_millis(50));
        assert_eq!(third.progress().phase, Phase::Allocating);
        drop(first);
        wait_until(&third, "allocation", |p| p.phase != Phase::Allocating);
        assert_eq!(first_shared.phase(), Phase::Finished);
        assert_eq!(first_shared.bytes_left_allocated(), Some(0));
        let summary = finish(third);
        assert_eq!(summary.end, RunEnd::Completed);
        assert_eq!(summary.tested_bytes, 16 * MIB);
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
