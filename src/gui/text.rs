//! Every piece of text the window shows that is built from data, as plain
//! functions so it can be tested without a window.

use std::time::Duration;

use memory_tester::engine::{Phase, Progress};
use memory_tester::types::{MemError, Slot, Stick, StickStatus, TestSummary};
use memory_tester::verdict::format_gb;

const MIB: u64 = 1024 * 1024;

/// What one slot shows, which decides its colour and its status word.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shown {
    /// Nothing in the slot.
    Empty,
    /// A run is in progress.
    Testing,
    /// A result (or the lack of one).
    Status(StickStatus),
}

/// The status in words, so the window never relies on colour alone.
pub fn status_word(shown: Shown) -> &'static str {
    match shown {
        Shown::Empty => "Empty",
        Shown::Testing => "Testing…",
        Shown::Status(StickStatus::Untested) => "Not tested",
        Shown::Status(StickStatus::Passed) => "Passed",
        Shown::Status(StickStatus::Failed) => "Failed",
        Shown::Status(StickStatus::Suspect) => "Suspect",
    }
}

/// The slot's name as printed on the board, e.g. "DIMM_A2"; the bank name
/// when the firmware gives no slot name; otherwise "Slot 3" (counting from 1).
pub fn slot_label(slot: &Slot, index: usize) -> String {
    [slot.locator.trim(), slot.bank.trim()]
        .into_iter()
        .find(|s| !s.is_empty())
        .map_or_else(|| format!("Slot {}", index + 1), str::to_string)
}

/// The stick's capacity, e.g. "16 GB"; "Size unknown" when it reports none.
pub fn size_text(stick: &Stick) -> String {
    if stick.size_mb == 0 {
        return "Size unknown".to_string();
    }
    format_gb(stick.size_mb.saturating_mul(MIB))
}

/// Manufacturer and part number, e.g. "Corsair CMK32GX4M2E3200C16", or
/// `None` when neither is known.
pub fn name_text(stick: &Stick) -> Option<String> {
    join_known(&[stick.manufacturer.trim(), stick.part_number.trim()], " ")
}

/// Memory type and speed, e.g. "DDR4 · 3200 MHz", or `None` when neither is
/// known.
pub fn kind_text(stick: &Stick) -> Option<String> {
    let speed = stick
        .speed_mts
        .filter(|&s| s > 0)
        .map(|s| format!("{s} MHz"))
        .unwrap_or_default();
    join_known(&[stick.memory_type.trim(), &speed], " · ")
}

fn join_known(parts: &[&str], separator: &str) -> Option<String> {
    let known: Vec<&str> = parts.iter().copied().filter(|p| !p.is_empty()).collect();
    (!known.is_empty()).then(|| known.join(separator))
}

/// One line under the title saying what was found, e.g.
/// "2 memory sticks found, 32 GB in total."
pub fn installed_summary(slots: &[Slot]) -> String {
    let sticks: Vec<&Stick> = slots.iter().filter_map(|s| s.stick.as_ref()).collect();
    if sticks.is_empty() {
        return "This computer did not say which memory sticks it has, \
                but the test still checks its memory."
            .to_string();
    }
    let total = sticks
        .iter()
        .map(|s| s.size_mb.saturating_mul(MIB))
        .fold(0, u64::saturating_add);
    let count = match sticks.len() {
        1 => "1 memory stick".to_string(),
        n => format!("{n} memory sticks"),
    };
    if total == 0 {
        format!("{count} found.")
    } else {
        format!("{count} found, {} in total.", format_gb(total))
    }
}

/// The caption for a result shown from an earlier run.
pub fn remembered_note(when_unix: u64) -> String {
    format!(
        "Remembered from {}",
        memory_tester::history::format_date(when_unix)
    )
}

/// The caption for a stick whose earlier failure is still on record while
/// this run gave it a different status.
pub fn failed_earlier_note(when_unix: u64) -> String {
    format!(
        "Failed earlier, on {}",
        memory_tester::history::format_date(when_unix)
    )
}

/// Elapsed time as "MM:SS", or "H:MM:SS" from one hour on.
pub fn format_elapsed(elapsed: Duration) -> String {
    let secs = elapsed.as_secs();
    let (h, m, s) = (secs / 3600, secs / 60 % 60, secs % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m:02}:{s:02}")
    }
}

/// A fraction as a whole percentage, rounded down so 100% means really done.
/// Out-of-range or NaN input is clamped to 0..=100.
pub fn percent(fraction: f32) -> u32 {
    if fraction.is_nan() {
        return 0;
    }
    (fraction.clamp(0.0, 1.0) * 100.0).floor() as u32
}

/// "0 errors", "1 error", "2 errors".
pub fn error_count_text(count: u64) -> String {
    match count {
        1 => "1 error".to_string(),
        n => format!("{n} errors"),
    }
}

/// One piece of the status line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Part {
    /// The text of this piece.
    pub text: String,
    /// True for the error count once it is above zero, which the window
    /// highlights.
    pub alert: bool,
}

fn part(text: impl Into<String>) -> Part {
    Part {
        text: text.into(),
        alert: false,
    }
}

/// Separator between the parts of the status line.
pub const PART_SEPARATOR: &str = " · ";

/// The status line under the progress bar, in pieces, e.g.
/// "Pass 1 of 4 · Checkerboard · 52 GB under test · 0 errors · 01:23".
pub fn status_parts(progress: &Progress) -> Vec<Part> {
    let time = part(format_elapsed(progress.elapsed));
    let errors = Part {
        text: error_count_text(progress.error_count),
        alert: progress.error_count > 0,
    };
    match progress.phase {
        Phase::Allocating => {
            let mut parts = vec![part("Getting memory ready…")];
            if progress.error_count > 0 {
                parts.push(errors);
            }
            parts.push(time);
            parts
        }
        Phase::Testing => {
            let pass = match progress.total_passes {
                Some(total) => format!("Pass {} of {total}", progress.pass),
                None => format!("Pass {}", progress.pass),
            };
            let mut parts = vec![part(pass)];
            if !progress.pattern.is_empty() {
                parts.push(part(progress.pattern));
            }
            parts.push(part(format!(
                "{} under test",
                format_gb(progress.tested_bytes)
            )));
            parts.push(errors);
            parts.push(time);
            parts
        }
        Phase::Finished => vec![part("Finishing…"), errors, time],
    }
}

/// What the progress bar shows.
#[derive(Debug, Clone, PartialEq)]
pub enum Bar {
    /// Nothing measurable yet: show an activity indicator instead.
    Busy,
    /// A bar filled to `fraction` with `label` on it.
    Fill {
        /// 0.0..=1.0.
        fraction: f32,
        /// e.g. "52%" or "Pass 3 · 41%".
        label: String,
    },
}

/// The progress bar for a snapshot. Runs with a fixed number of passes show
/// overall progress; runs that go on until stopped show the current pass's
/// progress and its number. While memory is being allocated the bar shows
/// how much has been allocated, once that is known.
pub fn progress_bar(progress: &Progress) -> Bar {
    let fill = |fraction: f32, label: String| Bar::Fill {
        fraction: fraction.clamp(0.0, 1.0),
        label,
    };
    match progress.phase {
        Phase::Allocating if progress.pass_fraction > 0.0 => fill(
            progress.pass_fraction,
            format!("{}%", percent(progress.pass_fraction)),
        ),
        Phase::Allocating => Bar::Busy,
        Phase::Testing | Phase::Finished => match progress.overall_fraction {
            Some(overall) => fill(overall, format!("{}%", percent(overall))),
            None => fill(
                progress.pass_fraction,
                format!(
                    "Pass {} · {}%",
                    progress.pass.max(1),
                    percent(progress.pass_fraction)
                ),
            ),
        },
    }
}

/// The note shown when the tested memory could not be locked into RAM.
pub const NOT_LOCKED_NOTE: &str = "Windows did not let this program lock the memory it tested \
                                   into place, so this result is less reliable than usual.";

/// [`NOT_LOCKED_NOTE`] when it applies: something was tested, none of it locked.
pub fn lock_note(summary: &TestSummary) -> Option<&'static str> {
    (summary.tested_bytes > 0 && summary.locked_bytes == 0).then_some(NOT_LOCKED_NOTE)
}

/// The bits that differ in a failing word: "bit 0", "bits 3, 17", or from
/// three on just the count, "12 bits". Never longer than 11 characters, so
/// it fits its column in the error table.
pub fn flipped_bits_text(mask: u64) -> String {
    let bits: Vec<String> = (0..64)
        .filter(|b| mask & (1 << b) != 0)
        .map(|b: u32| b.to_string())
        .collect();
    match bits.len() {
        0 => "none".to_string(),
        1 => format!("bit {}", bits[0]),
        2 => format!("bits {}", bits.join(", ")),
        n => format!("{n} bits"),
    }
}

/// The heading row of the error table in the Details section. Values are
/// hexadecimal; the pattern comes last because its width varies.
pub fn error_table_header() -> String {
    format!(
        "{:<14}  {:<16}  {:<16}  {:<11}  {:<4}  {}",
        "Address", "Expected", "Actual", "Differs", "Pass", "Pattern"
    )
}

/// One row of the error table, aligned with [`error_table_header`].
/// User-mode addresses fit in 12 hex digits; a longer one only shifts the
/// rest of its row.
pub fn error_row(error: &MemError) -> String {
    format!(
        "{:<14}  {:016x}  {:016x}  {:<11}  {:<4}  {}",
        format!("{:#014x}", error.address),
        error.expected,
        error.actual,
        flipped_bits_text(error.flipped_bits()),
        error.pass,
        error.pattern,
    )
}

/// The summary lines at the top of the Details section.
pub fn details_summary(summary: &TestSummary) -> Vec<String> {
    let locked = if summary.locked_bytes > 0 {
        format!(" ({} locked into RAM)", format_gb(summary.locked_bytes))
    } else {
        String::new()
    };
    vec![
        format!("Memory tested: {}{locked}", format_gb(summary.tested_bytes)),
        format!("Passes completed: {}", summary.passes_completed),
        format!("Time taken: {}", format_elapsed(summary.elapsed)),
        format!("Errors found: {}", summary.error_count),
    ]
}

/// The line after the error table when more errors were seen than recorded.
pub fn unlisted_errors_note(summary: &TestSummary) -> Option<String> {
    let listed = summary.errors.len() as u64;
    (summary.error_count > listed).then(|| {
        format!(
            "…and {} more not listed.",
            summary.error_count.saturating_sub(listed)
        )
    })
}

/// `text` cut to fit `max_width`, ending in "…" if it had to be cut.
///
/// `measure` gives the drawn width of a string; the window passes the real
/// font measurement. Cuts fall on character boundaries. If not even the
/// ellipsis fits, the result is the ellipsis alone.
pub fn truncate_to_width(text: &str, max_width: f32, measure: impl Fn(&str) -> f32) -> String {
    if measure(text) <= max_width {
        return text.to_string();
    }
    // Byte offsets of every character boundary after the first character.
    let ends: Vec<usize> = text
        .char_indices()
        .map(|(i, _)| i)
        .skip(1)
        .chain(std::iter::once(text.len()))
        .collect();
    let candidate = |end: usize| format!("{}…", text[..end].trim_end());
    // Longest prefix that still fits, by binary search (width grows with length).
    let (mut lo, mut hi) = (0usize, ends.len());
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        if measure(&candidate(ends[mid - 1])) <= max_width {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    if lo == 0 {
        "…".to_string()
    } else {
        candidate(ends[lo - 1])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stick() -> Stick {
        Stick {
            size_mb: 16384,
            manufacturer: "Corsair".to_string(),
            part_number: "CMK32GX4M2E3200C16".to_string(),
            serial: Some("S".to_string()),
            speed_mts: Some(3200),
            memory_type: "DDR4".to_string(),
        }
    }

    fn bare_stick(size_mb: u64) -> Stick {
        Stick {
            size_mb,
            manufacturer: String::new(),
            part_number: String::new(),
            serial: None,
            speed_mts: None,
            memory_type: String::new(),
        }
    }

    fn slot(locator: &str, bank: &str, stick: Option<Stick>) -> Slot {
        Slot {
            locator: locator.to_string(),
            bank: bank.to_string(),
            stick,
        }
    }

    /// The status line as the window shows it, parts joined.
    fn status_line(progress: &Progress) -> String {
        status_parts(progress)
            .into_iter()
            .map(|p| p.text)
            .collect::<Vec<_>>()
            .join(PART_SEPARATOR)
    }

    fn progress(phase: Phase) -> Progress {
        Progress {
            phase,
            pass: 1,
            total_passes: Some(4),
            pattern: "Checkerboard",
            pass_fraction: 0.5,
            overall_fraction: Some(0.125),
            tested_bytes: 52 * 1024 * MIB,
            error_count: 0,
            elapsed: Duration::from_secs(83),
        }
    }

    #[test]
    fn status_words() {
        assert_eq!(status_word(Shown::Empty), "Empty");
        assert_eq!(status_word(Shown::Testing), "Testing…");
        assert_eq!(
            status_word(Shown::Status(StickStatus::Untested)),
            "Not tested"
        );
        assert_eq!(status_word(Shown::Status(StickStatus::Passed)), "Passed");
        assert_eq!(status_word(Shown::Status(StickStatus::Failed)), "Failed");
        assert_eq!(status_word(Shown::Status(StickStatus::Suspect)), "Suspect");
    }

    #[test]
    fn slot_label_falls_back_to_bank_then_number() {
        assert_eq!(slot_label(&slot("DIMM_A2", "BANK 1", None), 0), "DIMM_A2");
        assert_eq!(slot_label(&slot("  ", " BANK 1 ", None), 0), "BANK 1");
        assert_eq!(slot_label(&slot("", "", None), 2), "Slot 3");
    }

    #[test]
    fn stick_captions_with_everything_known() {
        let s = stick();
        assert_eq!(size_text(&s), "16 GB");
        assert_eq!(name_text(&s).as_deref(), Some("Corsair CMK32GX4M2E3200C16"));
        assert_eq!(kind_text(&s).as_deref(), Some("DDR4 · 3200 MHz"));
    }

    #[test]
    fn stick_captions_with_fields_missing() {
        // Like a real VM: a size and a manufacturer, nothing else.
        let mut s = bare_stick(15 * 1024);
        s.manufacturer = " QEMU ".to_string();
        assert_eq!(size_text(&s), "15 GB");
        assert_eq!(name_text(&s).as_deref(), Some("QEMU"));
        assert_eq!(kind_text(&s), None);

        let s = bare_stick(0);
        assert_eq!(size_text(&s), "Size unknown");
        assert_eq!(name_text(&s), None);

        let mut s = bare_stick(1024);
        s.part_number = "ABC".to_string();
        s.speed_mts = Some(2666);
        assert_eq!(name_text(&s).as_deref(), Some("ABC"));
        assert_eq!(kind_text(&s).as_deref(), Some("2666 MHz"));
        s.speed_mts = Some(0);
        s.memory_type = "DDR5".to_string();
        assert_eq!(kind_text(&s).as_deref(), Some("DDR5"));
        assert_eq!(size_text(&bare_stick(1536)), "1.5 GB");
    }

    #[test]
    fn installed_summary_counts_sticks_and_size() {
        let two = [
            slot("A", "", Some(stick())),
            slot("B", "", None),
            slot("C", "", Some(stick())),
        ];
        assert_eq!(
            installed_summary(&two),
            "2 memory sticks found, 32 GB in total."
        );
        assert_eq!(
            installed_summary(&two[..1]),
            "1 memory stick found, 16 GB in total."
        );
        assert_eq!(
            installed_summary(&[slot("A", "", Some(bare_stick(0)))]),
            "1 memory stick found."
        );
        assert!(installed_summary(&[]).contains("did not say"));
        assert!(installed_summary(&[slot("A", "", None)]).contains("did not say"));
    }

    #[test]
    fn notes_carry_the_date() {
        assert_eq!(remembered_note(1_795_392_000), "Remembered from 2026-11-23");
        assert_eq!(
            failed_earlier_note(1_795_392_000),
            "Failed earlier, on 2026-11-23"
        );
    }

    #[test]
    fn elapsed_formatting_boundaries() {
        let f = |s: u64| format_elapsed(Duration::from_secs(s));
        assert_eq!(f(0), "00:00");
        assert_eq!(format_elapsed(Duration::from_millis(59_999)), "00:59");
        assert_eq!(f(60), "01:00");
        assert_eq!(f(83), "01:23");
        assert_eq!(f(3599), "59:59");
        assert_eq!(f(3600), "1:00:00");
        assert_eq!(f(36_000 + 61), "10:01:01");
        // No panic or overflow at the top of the range.
        assert!(format_elapsed(Duration::MAX).ends_with(":15"));
    }

    #[test]
    fn percent_rounds_down_and_clamps() {
        assert_eq!(percent(0.0), 0);
        assert_eq!(percent(0.009), 0);
        assert_eq!(percent(0.52), 52);
        assert_eq!(percent(0.999), 99);
        assert_eq!(percent(1.0), 100);
        assert_eq!(percent(1.5), 100);
        assert_eq!(percent(-0.1), 0);
        assert_eq!(percent(f32::NAN), 0);
    }

    #[test]
    fn error_count_is_singular_for_one() {
        assert_eq!(error_count_text(0), "0 errors");
        assert_eq!(error_count_text(1), "1 error");
        assert_eq!(error_count_text(2), "2 errors");
        assert_eq!(error_count_text(u64::MAX), format!("{} errors", u64::MAX));
    }

    #[test]
    fn status_line_while_testing() {
        let p = progress(Phase::Testing);
        assert_eq!(
            status_line(&p),
            "Pass 1 of 4 · Checkerboard · 52 GB under test · 0 errors · 01:23"
        );
        assert!(status_parts(&p).iter().all(|part| !part.alert));

        let endless = Progress {
            total_passes: None,
            overall_fraction: None,
            pass: 7,
            ..p
        };
        assert!(status_line(&endless).starts_with("Pass 7 · Checkerboard · "));
    }

    #[test]
    fn errors_are_flagged_for_highlighting() {
        let p = Progress {
            error_count: 3,
            ..progress(Phase::Testing)
        };
        let alerts: Vec<Part> = status_parts(&p).into_iter().filter(|x| x.alert).collect();
        assert_eq!(
            alerts,
            vec![Part {
                text: "3 errors".to_string(),
                alert: true
            }]
        );
    }

    #[test]
    fn status_line_while_allocating_and_finishing() {
        let allocating = Progress {
            pass: 0,
            pattern: "",
            tested_bytes: 0,
            pass_fraction: 0.0,
            overall_fraction: Some(0.0),
            ..progress(Phase::Allocating)
        };
        assert_eq!(status_line(&allocating), "Getting memory ready… · 01:23");
        assert_eq!(
            status_line(&progress(Phase::Finished)),
            "Finishing… · 0 errors · 01:23"
        );
    }

    #[test]
    fn progress_bar_for_fixed_and_endless_runs() {
        assert_eq!(
            progress_bar(&progress(Phase::Testing)),
            Bar::Fill {
                fraction: 0.125,
                label: "12%".to_string()
            }
        );
        let endless = Progress {
            total_passes: None,
            overall_fraction: None,
            pass: 3,
            pass_fraction: 0.41,
            ..progress(Phase::Testing)
        };
        assert_eq!(
            progress_bar(&endless),
            Bar::Fill {
                fraction: 0.41,
                label: "Pass 3 · 41%".to_string()
            }
        );
    }

    #[test]
    fn progress_bar_while_allocating() {
        let mut p = progress(Phase::Allocating);
        p.pass_fraction = 0.0;
        assert_eq!(progress_bar(&p), Bar::Busy);
        p.pass_fraction = 0.4;
        assert_eq!(
            progress_bar(&p),
            Bar::Fill {
                fraction: 0.4,
                label: "40%".to_string()
            }
        );
        // An out-of-range value cannot overfill the bar.
        p.pass_fraction = 7.0;
        assert_eq!(
            progress_bar(&p),
            Bar::Fill {
                fraction: 1.0,
                label: "100%".to_string()
            }
        );
    }

    fn summary(tested: u64, locked: u64) -> TestSummary {
        TestSummary {
            end: memory_tester::types::RunEnd::Completed,
            tested_bytes: tested,
            locked_bytes: locked,
            passes_completed: 2,
            error_count: 0,
            errors: Vec::new(),
            elapsed: Duration::from_secs(3725),
        }
    }

    #[test]
    fn lock_note_only_when_something_tested_and_nothing_locked() {
        assert_eq!(lock_note(&summary(MIB, 0)), Some(NOT_LOCKED_NOTE));
        assert_eq!(lock_note(&summary(MIB, MIB)), None);
        assert_eq!(lock_note(&summary(0, 0)), None);
    }

    #[test]
    fn flipped_bits_wording() {
        assert_eq!(flipped_bits_text(0), "none");
        assert_eq!(flipped_bits_text(1), "bit 0");
        assert_eq!(flipped_bits_text(1 << 63), "bit 63");
        assert_eq!(flipped_bits_text(0b1000_1000), "bits 3, 7");
        assert_eq!(flipped_bits_text(3 << 62), "bits 62, 63");
        assert_eq!(flipped_bits_text(0b111), "3 bits");
        assert_eq!(flipped_bits_text(u64::MAX), "64 bits");
        // Every case fits the 11-character column.
        for mask in [1, 1 << 63, 3 << 62, 0b111, u64::MAX] {
            assert!(flipped_bits_text(mask).len() <= 11, "{mask:#x}");
        }
    }

    #[test]
    fn error_rows_line_up_with_the_header() {
        let e = MemError {
            address: 0x7f00_1234_5678,
            expected: 0,
            actual: 1,
            pattern: "Inverse checkerboard",
            pass: 3,
        };
        let row = error_row(&e);
        assert_eq!(
            row,
            "0x7f0012345678  0000000000000000  0000000000000001  bit 0        3     \
             Inverse checkerboard"
        );
        let header = error_table_header();
        assert_eq!(header.find("Expected"), row.find("0000000000000000"));
        assert_eq!(header.find("Actual"), row.find("0000000000000001"));
        assert_eq!(header.find("Differs"), row.find("bit 0"));
        assert_eq!(header.find("Pass"), row.find("3 "));
        assert_eq!(header.find("Pattern"), row.find("Inverse"));
        // A small address is padded to the same width.
        let low = error_row(&MemError {
            address: 0x1000,
            ..e
        });
        assert!(low.starts_with("0x000000001000  "), "{low}");
        assert_eq!(low.len(), row.len());
    }

    #[test]
    fn details_summary_and_unlisted_note() {
        let mut s = summary(2 * 1024 * MIB, 0);
        assert_eq!(
            details_summary(&s),
            vec![
                "Memory tested: 2 GB",
                "Passes completed: 2",
                "Time taken: 1:02:05",
                "Errors found: 0"
            ]
        );
        s.locked_bytes = 1024 * MIB;
        assert_eq!(
            details_summary(&s)[0],
            "Memory tested: 2 GB (1 GB locked into RAM)"
        );
        assert_eq!(unlisted_errors_note(&s), None);
        s.error_count = 105;
        assert_eq!(
            unlisted_errors_note(&s).as_deref(),
            Some("…and 105 more not listed.")
        );
    }

    /// Every character is 10 wide.
    fn by_chars(s: &str) -> f32 {
        s.chars().count() as f32 * 10.0
    }

    #[test]
    fn truncation_keeps_text_that_fits() {
        assert_eq!(truncate_to_width("DIMM_A2", 70.0, by_chars), "DIMM_A2");
        assert_eq!(truncate_to_width("", 0.0, by_chars), "");
    }

    #[test]
    fn truncation_cuts_to_the_widest_fit_with_an_ellipsis() {
        // 8 slots of width: 7 characters plus the ellipsis.
        assert_eq!(
            truncate_to_width("CMK32GX4M2E3200C16", 80.0, by_chars),
            "CMK32GX…"
        );
        // One less than the full text needs.
        assert_eq!(truncate_to_width("ABCDEFGH", 79.0, by_chars), "ABCDEF…");
        // Room for the ellipsis only, or not even that.
        assert_eq!(truncate_to_width("ABCDEFGH", 10.0, by_chars), "…");
        assert_eq!(truncate_to_width("ABCDEFGH", 0.0, by_chars), "…");
        // A cut never leaves a space before the ellipsis.
        assert_eq!(
            truncate_to_width("Corsair Vengeance", 90.0, by_chars),
            "Corsair…"
        );
    }

    #[test]
    fn truncation_respects_multibyte_characters() {
        let cut = truncate_to_width("Müller 内存模块", 70.0, by_chars);
        assert_eq!(cut, "Müller…");
        assert_eq!(
            truncate_to_width("Müller 内存模块", 60.0, by_chars),
            "Mülle…"
        );
        let cut = truncate_to_width("内存模块内存模块", 40.0, by_chars);
        assert_eq!(cut, "内存模…");
        assert!(by_chars(&cut) <= 40.0);
    }
}
