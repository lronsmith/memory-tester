//! The test patterns and the loops that write them into memory and check them.
//!
//! Memory is handled as 64-bit words in [`Block`]s. For each block the pattern
//! is chosen once, outside the inner loop, and the loop body is a single
//! volatile write (or a volatile read and compare), so a release build is
//! limited by memory bandwidth rather than by per-word dispatch.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::types::MemError;

/// How many errors a run records in full detail. Errors beyond this are
/// still counted.
pub const MAX_RECORDED_ERRORS: usize = 64;

/// One step of a test pass. A pass runs every pattern in [`Pattern::ALL`]
/// order, each one written across all test memory and then verified.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Pattern {
    AllZeros,
    AllOnes,
    Checkerboard,
    InverseCheckerboard,
    WalkingOnes,
    WalkingZeros,
    OwnAddress,
    Random,
    InverseRandom,
}

impl Pattern {
    /// Every pattern, in the order a pass runs them.
    pub(crate) const ALL: [Pattern; 9] = [
        Pattern::AllZeros,
        Pattern::AllOnes,
        Pattern::Checkerboard,
        Pattern::InverseCheckerboard,
        Pattern::WalkingOnes,
        Pattern::WalkingZeros,
        Pattern::OwnAddress,
        Pattern::Random,
        Pattern::InverseRandom,
    ];

    /// The name shown to users and stored in [`MemError::pattern`].
    pub(crate) fn name(self) -> &'static str {
        match self {
            Pattern::AllZeros => "All zeros",
            Pattern::AllOnes => "All ones",
            Pattern::Checkerboard => "Checkerboard",
            Pattern::InverseCheckerboard => "Inverse checkerboard",
            Pattern::WalkingOnes => "Walking ones",
            Pattern::WalkingZeros => "Walking zeros",
            Pattern::OwnAddress => "Own address",
            Pattern::Random => "Random",
            Pattern::InverseRandom => "Inverse random",
        }
    }
}

/// A run of consecutive 64-bit words of test memory. It is the unit of work
/// handed to one worker thread at a time.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Block {
    /// First word of the block (8-byte aligned).
    pub ptr: *mut u64,
    /// Number of words in the block.
    pub words: usize,
    /// Index of the block's first word counted across all test memory; the
    /// walking patterns are defined in terms of it.
    pub first_word: u64,
    /// Position of the block in the list of all blocks; part of the seed of
    /// the random patterns so every block gets different data.
    pub index: u64,
}

// SAFETY: a `Block` is only an address range; holding or sharing one does not
// touch memory. Every access goes through `write_block`/`verify_block`, whose
// callers must guarantee exclusive access to the range for the duration.
unsafe impl Send for Block {}
// SAFETY: as above; `&Block` gives no more access than `Block`.
unsafe impl Sync for Block {}

/// Collects the errors found by [`verify_block`]: counts all of them and keeps
/// the first [`MAX_RECORDED_ERRORS`] in detail. Safe to share between threads.
#[derive(Debug, Default)]
pub(crate) struct ErrorLog {
    count: AtomicU64,
    full: AtomicBool,
    list: Mutex<Vec<MemError>>,
}

impl ErrorLog {
    /// Total failing words seen.
    pub(crate) fn count(&self) -> u64 {
        self.count.load(Ordering::Relaxed)
    }

    /// The recorded errors, in the order they were recorded.
    pub(crate) fn errors(&self) -> Vec<MemError> {
        self.lock().clone()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<MemError>> {
        // A poisoned lock only means another thread panicked while pushing;
        // the list itself is still a valid list of errors.
        self.list.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Stores `error` unless the detailed list is already full. Does not
    /// touch the count, which `verify_block` adds once per block.
    #[cold]
    #[inline(never)]
    fn record(&self, error: MemError) {
        // Checked without the lock so a badly failing stick (every word
        // wrong) does not serialise all workers on the mutex.
        if self.full.load(Ordering::Relaxed) {
            return;
        }
        let mut list = self.lock();
        if list.len() < MAX_RECORDED_ERRORS {
            list.push(error);
        }
        if list.len() >= MAX_RECORDED_ERRORS {
            self.full.store(true, Ordering::Relaxed);
        }
    }
}

/// Writes `pattern` into every word of `block`.
///
/// # Safety
///
/// `block.ptr` must be valid for reads and writes of `block.words` 8-byte
/// aligned `u64` values, and no other thread may access those words during the
/// call.
pub(crate) unsafe fn write_block(pattern: Pattern, block: &Block, pass: u32, seed: u64) {
    with_generator(pattern, block, pass, seed, Writer { block })
}

/// Reads every word of `block` back, compares it with what `write_block`
/// wrote for the same `pattern`, `pass` and `seed`, and logs each mismatch in
/// `log`. Returns the number of mismatching words.
///
/// # Safety
///
/// Same as [`write_block`].
pub(crate) unsafe fn verify_block(
    pattern: Pattern,
    block: &Block,
    pass: u32,
    seed: u64,
    log: &ErrorLog,
) -> u64 {
    let checker = Checker {
        block,
        pattern,
        pass,
        log,
    };
    let bad = with_generator(pattern, block, pass, seed, checker);
    log.count.fetch_add(bad, Ordering::Relaxed);
    bad
}

/// Something that consumes the words of one block in order, given a
/// generator that returns the expected value of each successive word.
trait WordSink {
    type Output;
    fn run(self, next: impl FnMut() -> u64) -> Self::Output;
}

/// Picks the generator for `pattern` once and hands it to `sink`, so the
/// sink's loop is compiled separately for each pattern with no branching on
/// the pattern inside it.
fn with_generator<S: WordSink>(
    pattern: Pattern,
    block: &Block,
    pass: u32,
    seed: u64,
    sink: S,
) -> S::Output {
    // Walking patterns: word i (counted across all test memory) has bit
    // (i + pass) % 64 set. The first word's bit is computed once and then
    // moved along by one for each following word.
    let mut walk_bit = ((block.first_word % 64 + u64::from(pass) % 64) % 64) as u32;
    let mut walk = move || {
        let w = 1u64 << walk_bit;
        walk_bit = (walk_bit + 1) % 64;
        w
    };
    let mut rng = block_seed(seed, pass, block.index);
    let mut address = block.ptr as u64;

    match pattern {
        Pattern::AllZeros => sink.run(|| 0),
        Pattern::AllOnes => sink.run(|| u64::MAX),
        Pattern::Checkerboard => sink.run(|| 0xAAAA_AAAA_AAAA_AAAA),
        Pattern::InverseCheckerboard => sink.run(|| 0x5555_5555_5555_5555),
        Pattern::WalkingOnes => sink.run(walk),
        Pattern::WalkingZeros => sink.run(move || !walk()),
        Pattern::OwnAddress => sink.run(move || {
            let w = address;
            address = address.wrapping_add(8);
            w
        }),
        Pattern::Random => sink.run(move || splitmix64(&mut rng)),
        Pattern::InverseRandom => sink.run(move || !splitmix64(&mut rng)),
    }
}

/// Writes each generated value into successive words of a block.
struct Writer<'a> {
    block: &'a Block,
}

impl WordSink for Writer<'_> {
    type Output = ();

    fn run(self, mut next: impl FnMut() -> u64) {
        let Block { ptr, words, .. } = *self.block;
        for i in 0..words {
            // SAFETY: a `Writer` is only built by `write_block`, whose caller
            // guarantees `ptr` is valid, aligned and exclusively ours for
            // `words` words, and `i < words`. Volatile keeps the compiler from
            // dropping or merging the store, since the program never reads it
            // back through an ordinary load.
            unsafe { ptr.add(i).write_volatile(next()) }
        }
    }
}

/// Compares successive words of a block with the generated values.
struct Checker<'a> {
    block: &'a Block,
    pattern: Pattern,
    pass: u32,
    log: &'a ErrorLog,
}

impl WordSink for Checker<'_> {
    type Output = u64;

    fn run(self, mut next: impl FnMut() -> u64) -> u64 {
        let Block { ptr, words, .. } = *self.block;
        let mut bad = 0;
        for i in 0..words {
            let expected = next();
            // SAFETY: as in `Writer::run` (a `Checker` is only built by
            // `verify_block`, which has the same contract). Volatile forces a
            // real load instead of reusing the value the compiler knows was
            // written.
            let actual = unsafe { ptr.add(i).read_volatile() };
            if actual != expected {
                bad += 1;
                self.log.record(MemError {
                    address: ptr as usize + i * 8,
                    expected,
                    actual,
                    pattern: self.pattern.name(),
                    pass: self.pass,
                });
            }
        }
        bad
    }
}

/// The splitmix64 generator: advances `state` and returns the next output.
fn splitmix64(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    mix64(*state)
}

/// The splitmix64 output function, a good 64-bit bit mixer.
fn mix64(mut z: u64) -> u64 {
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Starting state of the random generator for one block in one pass of one
/// run, so random data differs between runs, passes and blocks but can be
/// regenerated exactly for verification.
fn block_seed(run_seed: u64, pass: u32, block_index: u64) -> u64 {
    mix64(run_seed ^ mix64(u64::from(pass) ^ mix64(block_index)))
}

#[cfg(test)]
mod tests {
    use super::*;

    const WORDS: usize = 1000;
    const SEED: u64 = 0x1234_5678_9ABC_DEF0;
    /// Deliberately not a multiple of 64, so walking patterns start mid-cycle.
    const FIRST_WORD: u64 = 37;

    fn block_of(buf: &mut [u64], index: u64) -> Block {
        Block {
            ptr: buf.as_mut_ptr(),
            words: buf.len(),
            first_word: FIRST_WORD,
            index,
        }
    }

    fn write(p: Pattern, buf: &mut [u64], pass: u32, seed: u64, index: u64) {
        let b = block_of(buf, index);
        // SAFETY: `b` covers exactly `buf`, which is borrowed mutably here, so
        // nothing else can touch it during the call.
        unsafe { write_block(p, &b, pass, seed) }
    }

    fn verify(p: Pattern, buf: &mut [u64], pass: u32, seed: u64, log: &ErrorLog) -> u64 {
        let b = block_of(buf, 5);
        // SAFETY: as in `write`.
        unsafe { verify_block(p, &b, pass, seed, log) }
    }

    #[test]
    fn names_and_order_match_the_spec() {
        let names: Vec<&str> = Pattern::ALL.iter().map(|p| p.name()).collect();
        assert_eq!(
            names,
            [
                "All zeros",
                "All ones",
                "Checkerboard",
                "Inverse checkerboard",
                "Walking ones",
                "Walking zeros",
                "Own address",
                "Random",
                "Inverse random",
            ]
        );
    }

    #[test]
    fn written_values_match_the_spec() {
        let pass = 3;
        for p in Pattern::ALL {
            let mut buf = vec![0x0123_4567_89AB_CDEFu64; WORDS];
            write(p, &mut buf, pass, SEED, 5);
            for (i, &w) in buf.iter().enumerate() {
                let global = FIRST_WORD + i as u64;
                let walking = 1u64 << ((global + u64::from(pass)) % 64);
                let addr = buf.as_ptr() as u64 + 8 * i as u64;
                match p {
                    Pattern::AllZeros => assert_eq!(w, 0),
                    Pattern::AllOnes => assert_eq!(w, u64::MAX),
                    Pattern::Checkerboard => assert_eq!(w, 0xAAAA_AAAA_AAAA_AAAA),
                    Pattern::InverseCheckerboard => assert_eq!(w, 0x5555_5555_5555_5555),
                    Pattern::WalkingOnes => assert_eq!(w, walking, "word {i}"),
                    Pattern::WalkingZeros => assert_eq!(w, !walking, "word {i}"),
                    Pattern::OwnAddress => assert_eq!(w, addr, "word {i}"),
                    // Checked against each other below.
                    Pattern::Random | Pattern::InverseRandom => {}
                }
            }
        }
    }

    #[test]
    fn inverse_random_is_the_not_of_random() {
        let mut a = vec![0u64; WORDS];
        let mut b = vec![0u64; WORDS];
        write(Pattern::Random, &mut a, 1, SEED, 5);
        write(Pattern::InverseRandom, &mut b, 1, SEED, 5);
        for i in 0..WORDS {
            assert_eq!(a[i], !b[i], "word {i}");
        }
    }

    #[test]
    fn every_pattern_verifies_clean_after_writing() {
        for p in Pattern::ALL {
            let mut buf = vec![0u64; WORDS];
            write(p, &mut buf, 2, SEED, 5);
            let log = ErrorLog::default();
            assert_eq!(verify(p, &mut buf, 2, SEED, &log), 0, "{}", p.name());
            assert_eq!(log.count(), 0);
            assert!(log.errors().is_empty());
        }
    }

    #[test]
    fn every_single_bit_flip_is_reported_exactly() {
        let pass = 1;
        for p in Pattern::ALL {
            for word in [0, 1, WORDS / 2, WORDS - 2, WORDS - 1] {
                for bit in [0u32, 1, 7, 31, 32, 62, 63] {
                    let mut buf = vec![0u64; WORDS];
                    write(p, &mut buf, pass, SEED, 5);
                    let written = buf[word];
                    buf[word] ^= 1 << bit;
                    let address = buf.as_ptr() as usize + 8 * word;

                    let log = ErrorLog::default();
                    let found = verify(p, &mut buf, pass, SEED, &log);
                    assert_eq!(found, 1, "{} word {word} bit {bit}", p.name());
                    assert_eq!(log.count(), 1);
                    assert_eq!(
                        log.errors(),
                        vec![MemError {
                            address,
                            expected: written,
                            actual: written ^ (1 << bit),
                            pattern: p.name(),
                            pass,
                        }]
                    );
                }
            }
        }
    }

    #[test]
    fn walking_patterns_change_with_the_pass_number() {
        for p in [Pattern::WalkingOnes, Pattern::WalkingZeros] {
            let mut first = vec![0u64; WORDS];
            let mut second = vec![0u64; WORDS];
            write(p, &mut first, 1, SEED, 5);
            write(p, &mut second, 2, SEED, 5);
            for i in 0..WORDS {
                assert_ne!(first[i], second[i], "{} word {i}", p.name());
            }
            // Data written for pass 1 must not pass as pass 2's data.
            let log = ErrorLog::default();
            assert_eq!(verify(p, &mut first, 2, SEED, &log), WORDS as u64);
        }
    }

    #[test]
    fn random_data_depends_on_seed_pass_and_block() {
        let mut base = vec![0u64; WORDS];
        write(Pattern::Random, &mut base, 1, SEED, 5);
        let variants = [(1, SEED ^ 1, 5), (2, SEED, 5), (1, SEED, 6)];
        for (pass, seed, index) in variants {
            let mut other = vec![0u64; WORDS];
            write(Pattern::Random, &mut other, pass, seed, index);
            let same = base.iter().zip(&other).filter(|(a, b)| a == b).count();
            assert!(
                same < 5,
                "pass {pass} seed {seed:#x} block {index}: {same} equal words"
            );
        }
        // Not degenerate: the words within one block are not all alike.
        let ones: u32 = base.iter().map(|w| w.count_ones()).sum();
        let mean = f64::from(ones) / WORDS as f64;
        assert!((28.0..36.0).contains(&mean), "mean set bits {mean}");
    }

    #[test]
    fn same_seed_random_verifies_and_other_seed_does_not() {
        for p in [Pattern::Random, Pattern::InverseRandom] {
            let mut buf = vec![0u64; WORDS];
            write(p, &mut buf, 1, SEED, 5);
            let log = ErrorLog::default();
            assert_eq!(verify(p, &mut buf, 1, SEED, &log), 0);
            let log = ErrorLog::default();
            assert!(verify(p, &mut buf, 1, SEED + 1, &log) > (WORDS as u64) - 5);
        }
    }

    #[test]
    fn recording_stops_at_the_cap_but_counting_does_not() {
        let mut buf = vec![0u64; WORDS];
        write(Pattern::AllOnes, &mut buf, 1, SEED, 5);
        for w in buf.iter_mut().take(100) {
            *w = 0;
        }
        let log = ErrorLog::default();
        assert_eq!(verify(Pattern::AllOnes, &mut buf, 1, SEED, &log), 100);
        // A second block's worth of errors keeps adding to the same log.
        assert_eq!(verify(Pattern::AllOnes, &mut buf, 1, SEED, &log), 100);
        assert_eq!(log.count(), 200);
        let errors = log.errors();
        assert_eq!(errors.len(), MAX_RECORDED_ERRORS);
        // The recorded ones are the first ones found, in order.
        for (i, e) in errors.iter().enumerate() {
            assert_eq!(e.address, buf.as_ptr() as usize + 8 * i);
            assert_eq!((e.expected, e.actual), (u64::MAX, 0));
        }
    }
}
