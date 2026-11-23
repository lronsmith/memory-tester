//! Shared data types. These are the contract between the modules, so change
//! them only when every user of the type is updated in the same change.

use std::time::Duration;

/// One physical memory slot on the motherboard, as reported by the firmware.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Slot {
    /// Slot label as printed on the board, e.g. `"DIMM_A2"`. May be empty.
    pub locator: String,
    /// Bank label, e.g. `"BANK 1"` or `"P0 CHANNEL A"`. May be empty.
    pub bank: String,
    /// The stick in this slot, or `None` when the slot is empty.
    pub stick: Option<Stick>,
}

/// One installed memory stick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stick {
    /// Capacity in mebibytes.
    pub size_mb: u64,
    /// Manufacturer name, trimmed. Empty when the firmware gives nothing usable.
    pub manufacturer: String,
    /// Part number, trimmed. Empty when the firmware gives nothing usable.
    pub part_number: String,
    /// Serial number, or `None` when the firmware gives nothing usable
    /// (missing, blank, all zeros, or a placeholder such as "Not Specified").
    pub serial: Option<String>,
    /// Speed the stick is currently running at, in MT/s (shown to users as MHz).
    pub speed_mts: Option<u32>,
    /// Memory technology, e.g. `"DDR4"`. Empty when unknown.
    pub memory_type: String,
}

/// What the UI shows for one stick.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StickStatus {
    /// No conclusive result for this stick (drawn grey).
    Untested,
    /// Tested with no errors (drawn green).
    Passed,
    /// Errors were found and this stick is definitely responsible (drawn red).
    Failed,
    /// Errors were found while several sticks were installed, so this stick is
    /// one of the candidates but cannot be singled out (drawn amber).
    Suspect,
}

/// One detected memory error: a word that did not read back as written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemError {
    /// Virtual address of the 64-bit word that failed.
    pub address: usize,
    /// The value that was written.
    pub expected: u64,
    /// The value that was read back.
    pub actual: u64,
    /// Name of the test pattern that caught it.
    pub pattern: &'static str,
    /// Pass number (starting at 1) in which it was caught.
    pub pass: u32,
}

impl MemError {
    /// Bit mask of the bits that differ between what was written and read.
    pub fn flipped_bits(&self) -> u64 {
        self.expected ^ self.actual
    }
}

/// How a test run ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunEnd {
    /// Every requested pass ran to the end.
    Completed,
    /// The user stopped the run before it finished.
    Stopped,
    /// No memory could be allocated, so nothing was tested.
    AllocationFailed,
}

/// The final result of one test run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestSummary {
    /// How the run ended.
    pub end: RunEnd,
    /// Bytes of memory that were under test.
    pub tested_bytes: u64,
    /// How many of those bytes were locked into physical RAM (not pageable).
    pub locked_bytes: u64,
    /// Number of full passes completed.
    pub passes_completed: u32,
    /// Total number of failing words seen (may exceed `errors.len()`).
    pub error_count: u64,
    /// Details of the first errors seen, capped to keep memory use bounded.
    pub errors: Vec<MemError>,
    /// Wall-clock time the run took.
    pub elapsed: Duration,
}
