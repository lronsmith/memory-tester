//! Turns the result of a test run into a status for each memory slot, plus a
//! short headline and plain-English advice for the person running the test.
//!
//! A user-mode program can tell that memory is faulty but not which physical
//! stick a failing address belongs to. So a fault can be pinned on a stick only
//! when it is the only one installed; with several installed, every installed
//! stick becomes a suspect and the advice explains how to narrow it down.

use crate::history::History;
use crate::types::{RunEnd, Slot, StickStatus, TestSummary};

/// The overall outcome of a run, in terms the UI can act on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Overall {
    /// No errors, and at least one full pass ran: the run either completed,
    /// or was stopped after one or more full passes (which is how a run set
    /// to go on until stopped ends).
    Pass,
    /// Errors found and exactly one stick is installed: that stick is bad.
    FailSingle,
    /// Errors found with several sticks installed: cannot say which.
    FailMultiple,
    /// Errors found, but stick detection is evidently incomplete: no sticks
    /// were detected at all, or only one was detected while more memory was
    /// tested than it holds. Any detected sticks are `Suspect`.
    FailUnknownSticks,
    /// Stopped before the first full pass finished, with no errors seen:
    /// proves nothing.
    Inconclusive,
    /// Nothing was tested: no memory could be allocated, the run tested zero
    /// bytes, or it completed without running a single pass.
    CouldNotTest,
}

/// The judged result of one run: what happened overall, what each slot should
/// show, and the words to show the person.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verdict {
    /// The overall outcome.
    pub overall: Overall,
    /// One entry per slot, same order and length as the `slots` passed in.
    /// Empty slots are always `StickStatus::Untested`.
    pub per_slot: Vec<StickStatus>,
    /// One short line for the big result banner, e.g. "No errors found".
    pub headline: String,
    /// One to three plain sentences telling a non-technical person what the
    /// result means and what to do next.
    pub advice: String,
}

const GIB: u64 = 1024 * 1024 * 1024;
const MIB: u64 = 1024 * 1024;

/// Formats a byte count as gigabytes for people, e.g. `"16 GB"` or `"52.4 GB"`.
///
/// "GB" here means GiB (1024^3 bytes), which is what RAM sizes are sold as.
/// Exact multiples are shown as whole numbers; anything else is rounded to one
/// decimal place, except that an amount too small to show as 0.1 is written
/// "less than 0.1 GB" rather than as nothing.
pub fn format_gb(bytes: u64) -> String {
    if bytes.is_multiple_of(GIB) {
        return format!("{} GB", bytes / GIB);
    }
    let tenths = tenths_of_gb(bytes);
    if tenths == 0 {
        return "less than 0.1 GB".to_string();
    }
    format!("{}.{} GB", tenths / 10, tenths % 10)
}

/// `bytes` in tenths of a GiB, rounded to nearest, using integers so there is
/// no float error.
fn tenths_of_gb(bytes: u64) -> u128 {
    (u128::from(bytes) * 10 + u128::from(GIB) / 2) / u128::from(GIB)
}

/// How to find the faulty stick by testing one at a time; the start of a
/// clause that each caller finishes.
const ONE_AT_A_TIME: &str = "turn the computer off and unplug it, leave only one stick \
                             in, and test again, repeating for each stick";

/// After this many clean passes the advice may call a fault unlikely; after
/// fewer it only says that obvious faults would have shown.
const PASSES_FOR_CONFIDENCE: u32 = 4;

/// Judges a finished run. `history` says which sticks can have their result
/// remembered, which the advice for several suspects mentions.
///
/// - Errors always win, however the run ended: if any were seen, the
///   installed sticks are marked `Failed` (only one installed and it accounts
///   for all the memory tested) or `Suspect` (otherwise).
/// - Without errors, a run that tested something and finished at least one
///   full pass marks every installed stick `Passed`, whether it completed or
///   was stopped after that pass.
/// - A run stopped before its first full pass is `Inconclusive`, and any
///   other run is `CouldNotTest`; neither marks anything.
pub fn judge(slots: &[Slot], summary: &TestSummary, history: &History) -> Verdict {
    let installed: Vec<_> = slots.iter().filter_map(|s| s.stick.as_ref()).collect();
    let installed_bytes: u64 = installed
        .iter()
        .map(|stick| stick.size_mb.saturating_mul(MIB))
        .fold(0, u64::saturating_add);
    // Status for every installed slot; empty slots are always `Untested`.
    let mark = |status: StickStatus| -> Vec<StickStatus> {
        slots
            .iter()
            .map(|s| match s.stick {
                Some(_) => status,
                None => StickStatus::Untested,
            })
            .collect()
    };

    if summary.error_count > 0 {
        // More memory tested than one detected stick holds means detection
        // missed something, so that stick cannot be blamed on its own.
        let detection_incomplete = installed.is_empty()
            || (installed.len() == 1 && summary.tested_bytes > installed_bytes);
        if detection_incomplete {
            return Verdict {
                overall: Overall::FailUnknownSticks,
                per_slot: mark(StickStatus::Suspect),
                headline: "Memory errors found".to_string(),
                advice: format!(
                    "The test found errors, but could not detect every memory stick in this \
                     computer, so it cannot say which one is at fault. If there is only one \
                     stick, it is the likely cause. If there are several, {ONE_AT_A_TIME} \
                     and writing each result down."
                ),
            };
        }
        if installed.len() == 1 {
            return Verdict {
                overall: Overall::FailSingle,
                per_slot: mark(StickStatus::Failed),
                headline: "This memory stick failed the test".to_string(),
                advice: "If your other sticks pass when each is tested alone in this same \
                         slot, this one is faulty and should be replaced; memory often has a \
                         lifetime warranty. If every stick fails, the cause is more likely the \
                         motherboard or its memory speed setting than the sticks."
                    .to_string(),
            };
        }
        // Same predicate as `History::apply`, so the promise to remember each
        // stick holds exactly when the results will be stored.
        let all_remembered = slots
            .iter()
            .zip(history.rememberable_keys(slots))
            .all(|(slot, key)| slot.stick.is_none() || key.is_some());
        let ending = if all_remembered {
            "; this app remembers each stick's result."
        } else {
            "; write each result down, because this app cannot remember results for \
             some of these sticks."
        };
        return Verdict {
            overall: Overall::FailMultiple,
            per_slot: mark(StickStatus::Suspect),
            headline: "Memory errors found".to_string(),
            advice: format!(
                "The test found errors, but cannot tell which memory stick caused them \
                 while several are installed. To find out, {ONE_AT_A_TIME}{ending}"
            ),
        };
    }

    let passes = summary.passes_completed;
    let stopped = summary.end == RunEnd::Stopped;
    let full_pass_ran =
        summary.end != RunEnd::AllocationFailed && summary.tested_bytes > 0 && passes >= 1;
    match summary.end {
        _ if full_pass_ran => {
            let tested = format_gb(summary.tested_bytes);
            // Compare with the installed total only when the numbers are
            // consistent; more tested than detected means detection is off.
            let amount = if summary.tested_bytes > installed_bytes {
                format!("Tested {tested} of memory.")
            } else if tenths_of_gb(summary.tested_bytes) < tenths_of_gb(installed_bytes) {
                format!(
                    "Tested {tested} of {}; Windows and other programs were using some of the \
                     rest, and some was left free on purpose to keep the PC responsive.",
                    format_gb(installed_bytes)
                )
            } else {
                format!("Tested {tested} of {}.", format_gb(installed_bytes))
            };
            let count = match passes {
                1 => "1 full pass".to_string(),
                n => format!("{n} full passes"),
            };
            let ran = if stopped {
                format!("The test ran {count} before it was stopped.")
            } else {
                format!("The test ran {count}.")
            };
            let certainty = match passes {
                1 => {
                    "A quick test like this catches obvious faults, but the thorough test, \
                     or leaving it running for longer, is more certain."
                }
                n if n < PASSES_FOR_CONFIDENCE => {
                    "A short test like this catches obvious faults, but the thorough test, \
                     or leaving it running for longer, is more certain."
                }
                _ => {
                    "A clean result over several passes makes a memory fault unlikely, but \
                     faults that come and go can need a longer test."
                }
            };
            Verdict {
                overall: Overall::Pass,
                per_slot: mark(StickStatus::Passed),
                headline: "No errors found".to_string(),
                advice: format!("{amount} {ran} {certainty}"),
            }
        }
        RunEnd::Stopped => Verdict {
            overall: Overall::Inconclusive,
            per_slot: mark(StickStatus::Untested),
            headline: "Test stopped early".to_string(),
            advice: "No errors were seen, but the test was stopped before it finished one \
                     full pass, which is too little to show that the memory is healthy. \
                     Start it again and let at least one pass finish."
                .to_string(),
        },
        // No memory could be allocated, or nothing was tested.
        RunEnd::AllocationFailed | RunEnd::Completed => Verdict {
            overall: Overall::CouldNotTest,
            per_slot: mark(StickStatus::Untested),
            headline: "Could not run the test".to_string(),
            advice: "The test could not get any memory to check, so nothing was tested. \
                     Close other programs or restart the computer, then try again."
                .to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Stick;
    use std::time::Duration;

    fn stick(size_mb: u64, serial: Option<&str>) -> Stick {
        Stick {
            size_mb,
            manufacturer: "Corsair".to_string(),
            part_number: "CMK32GX4M2E3200C16".to_string(),
            serial: serial.map(str::to_string),
            speed_mts: Some(3200),
            memory_type: "DDR4".to_string(),
        }
    }

    fn slot(locator: &str, stick: Option<Stick>) -> Slot {
        Slot {
            locator: locator.to_string(),
            bank: String::new(),
            stick,
        }
    }

    fn summary(end: RunEnd, error_count: u64, tested_bytes: u64) -> TestSummary {
        TestSummary {
            end,
            tested_bytes,
            locked_bytes: tested_bytes,
            passes_completed: 1,
            error_count,
            errors: Vec::new(),
            elapsed: Duration::from_secs(60),
        }
    }

    /// `summary` with `passes` full passes completed instead of one.
    fn after(passes: u32, summary: TestSummary) -> TestSummary {
        TestSummary {
            passes_completed: passes,
            ..summary
        }
    }

    /// Judges with a history that has seen nothing.
    fn judge(slots: &[Slot], summary: &TestSummary) -> Verdict {
        super::judge(slots, summary, &History::in_memory())
    }

    /// Four slots: A1 empty, A2 16 GB, B1 empty, B2 16 GB.
    fn two_of_four() -> Vec<Slot> {
        vec![
            slot("A1", None),
            slot("A2", Some(stick(16384, Some("AAA")))),
            slot("B1", None),
            slot("B2", Some(stick(16384, Some("BBB")))),
        ]
    }

    use StickStatus::{Failed, Passed, Suspect, Untested};

    #[test]
    fn format_gb_whole_and_fractional() {
        assert_eq!(format_gb(16 * GIB), "16 GB");
        assert_eq!(format_gb(GIB / 2), "0.5 GB");
        // 52.4 GiB, a little under and a little over.
        assert_eq!(format_gb(52 * GIB + GIB * 4 / 10), "52.4 GB");
        assert_eq!(format_gb(52 * GIB + GIB * 4 / 10 + 1), "52.4 GB");
        assert_eq!(format_gb(0), "0 GB");
        // Just under a whole number rounds up to it but keeps the decimal.
        assert_eq!(format_gb(16 * GIB - 1), "16.0 GB");
        // No overflow at the top of the range.
        assert_eq!(format_gb(u64::MAX), "17179869184.0 GB");
        // A small non-zero amount is not shown as nothing.
        assert_eq!(format_gb(MIB), "less than 0.1 GB");
        assert_eq!(format_gb(1), "less than 0.1 GB");
    }

    #[test]
    fn pass_advice_does_not_compare_against_too_small_installed_total() {
        // More tested than the detected sticks hold: say only what was tested.
        let one = vec![slot("A1", Some(stick(16384, Some("S"))))];
        let v = judge(&one, &summary(RunEnd::Completed, 0, 24 * GIB));
        assert_eq!(v.overall, Overall::Pass);
        assert!(
            v.advice.starts_with("Tested 24 GB of memory."),
            "{}",
            v.advice
        );
        assert!(!v.advice.contains("of 16 GB"), "{}", v.advice);

        // A stick reported with size zero gives an installed total of zero.
        let zero = vec![slot("A1", Some(stick(0, Some("S"))))];
        let v = judge(&zero, &summary(RunEnd::Completed, 0, 6 * GIB));
        assert!(
            v.advice.starts_with("Tested 6 GB of memory."),
            "{}",
            v.advice
        );
    }

    #[test]
    fn pass_advice_mentions_the_rest_only_when_it_shows() {
        let one = vec![slot("A1", Some(stick(16384, Some("S"))))];
        // 15.97 GB rounds to 16.0, so "the rest" would point at nothing visible.
        let v = judge(&one, &summary(RunEnd::Completed, 0, 16 * GIB - 30 * MIB));
        assert!(
            v.advice.starts_with("Tested 16.0 GB of 16 GB."),
            "{}",
            v.advice
        );
        assert!(!v.advice.contains("the rest"), "{}", v.advice);

        let v = judge(&one, &summary(RunEnd::Completed, 0, MIB));
        assert!(
            v.advice
                .starts_with("Tested less than 0.1 GB of 16 GB; Windows and other programs"),
            "{}",
            v.advice
        );
    }

    #[test]
    fn completed_with_nothing_tested_is_not_a_pass() {
        let v = judge(&two_of_four(), &summary(RunEnd::Completed, 0, 0));
        assert_eq!(v.overall, Overall::CouldNotTest);
        assert_eq!(v.per_slot, vec![Untested; 4]);
    }

    #[test]
    fn errors_beat_allocation_failure() {
        let one = vec![slot("A1", Some(stick(8192, Some("S"))))];
        let v = judge(&one, &summary(RunEnd::AllocationFailed, 1, GIB));
        assert_eq!(v.overall, Overall::FailSingle);
        assert_eq!(v.per_slot, vec![Failed]);
    }

    #[test]
    fn errors_with_more_tested_than_one_detected_stick_holds_blame_no_one() {
        let slots = vec![slot("A1", None), slot("A2", Some(stick(16384, Some("S"))))];
        let v = judge(&slots, &summary(RunEnd::Completed, 2, 24 * GIB));
        assert_eq!(v.overall, Overall::FailUnknownSticks);
        assert_eq!(v.per_slot, vec![Untested, Suspect]);
        assert!(v.advice.contains("could not detect every"), "{}", v.advice);

        // Exactly the stick's size is still consistent with one stick.
        let v = judge(&slots, &summary(RunEnd::Completed, 2, 16 * GIB));
        assert_eq!(v.overall, Overall::FailSingle);
    }

    #[test]
    fn several_sticks_sharing_a_serial_are_not_promised_to_be_remembered() {
        let slots = vec![
            slot("A1", Some(stick(8192, Some("12345678")))),
            slot("A2", Some(stick(8192, Some("12345678")))),
        ];
        let v = judge(&slots, &summary(RunEnd::Completed, 1, 12 * GIB));
        assert_eq!(v.overall, Overall::FailMultiple);
        assert!(!v.advice.contains("remembers"), "{}", v.advice);
        assert!(v.advice.contains("write each result down"), "{}", v.advice);
    }

    #[test]
    fn completed_without_errors_passes_installed_slots_only() {
        let slots = two_of_four();
        let v = judge(&slots, &summary(RunEnd::Completed, 0, 28 * GIB));
        assert_eq!(v.overall, Overall::Pass);
        assert_eq!(v.per_slot, vec![Untested, Passed, Untested, Passed]);
        assert_eq!(v.headline, "No errors found");
    }

    #[test]
    fn pass_advice_states_tested_and_installed_amounts() {
        let slots = two_of_four();
        let v = judge(&slots, &summary(RunEnd::Completed, 0, 28 * GIB));
        assert_eq!(
            v.advice,
            "Tested 28 GB of 32 GB; Windows and other programs were using some of the rest, \
             and some was left free on purpose to keep the PC responsive. The test ran 1 \
             full pass. A quick test like this catches obvious faults, but the thorough \
             test, or leaving it running for longer, is more certain."
        );
    }

    #[test]
    fn pass_advice_claims_more_only_after_more_passes() {
        let slots = vec![slot("A1", Some(stick(8192, Some("S"))))];
        let advice = |passes: u32, end: RunEnd| {
            judge(&slots, &after(passes, summary(end, 0, 8 * GIB))).advice
        };
        // One pass: obvious faults only, and how to be more certain.
        let one = advice(1, RunEnd::Completed);
        assert!(one.contains("The test ran 1 full pass."), "{one}");
        assert!(one.contains("quick test"), "{one}");
        assert!(one.contains("thorough test"), "{one}");
        assert!(one.contains("running for longer"), "{one}");
        assert!(!one.contains("unlikely"), "{one}");
        // Two or three passes: still no claim that a fault is unlikely.
        let three = advice(3, RunEnd::Completed);
        assert!(three.contains("The test ran 3 full passes."), "{three}");
        assert!(three.contains("short test"), "{three}");
        assert!(!three.contains("unlikely"), "{three}");
        // Four or more: the stronger wording.
        for passes in [4, 60] {
            let many = advice(passes, RunEnd::Completed);
            assert!(
                many.contains(&format!("The test ran {passes} full passes.")),
                "{many}"
            );
            assert!(many.contains("unlikely"), "{many}");
            assert!(!many.contains("quick test"), "{many}");
        }
    }

    #[test]
    fn stopped_after_full_passes_is_a_pass_that_says_how_many() {
        let slots = two_of_four();
        let v = judge(&slots, &after(3, summary(RunEnd::Stopped, 0, 28 * GIB)));
        assert_eq!(v.overall, Overall::Pass);
        assert_eq!(v.per_slot, vec![Untested, Passed, Untested, Passed]);
        assert_eq!(v.headline, "No errors found");
        assert!(
            v.advice
                .contains("The test ran 3 full passes before it was stopped."),
            "{}",
            v.advice
        );

        let v = judge(&slots, &after(1, summary(RunEnd::Stopped, 0, 28 * GIB)));
        assert_eq!(v.overall, Overall::Pass);
        assert!(
            v.advice
                .contains("The test ran 1 full pass before it was stopped."),
            "{}",
            v.advice
        );

        // An hour of an endless run: many passes, the stronger wording.
        let v = judge(&slots, &after(80, summary(RunEnd::Stopped, 0, 28 * GIB)));
        assert_eq!(v.overall, Overall::Pass);
        assert!(v.advice.contains("80 full passes before"), "{}", v.advice);
        assert!(v.advice.contains("unlikely"), "{}", v.advice);
    }

    #[test]
    fn a_key_seen_shared_before_is_not_promised_to_be_remembered() {
        let twin = || stick(8192, Some("SAME"));
        let mut history = History::in_memory();
        // Both twins were installed together once.
        let together = vec![slot("A1", Some(twin())), slot("A2", Some(twin()))];
        let seen = super::judge(
            &together,
            &summary(RunEnd::Completed, 1, 12 * GIB),
            &history,
        );
        history.apply(&together, &seen, 1);
        // Now one twin is tested with a different stick: its serial is unique
        // here, but this app will not store its result.
        let now = vec![
            slot("A1", Some(twin())),
            slot("A2", Some(stick(8192, Some("OTHER")))),
        ];
        let v = super::judge(&now, &summary(RunEnd::Completed, 1, 12 * GIB), &history);
        assert_eq!(v.overall, Overall::FailMultiple);
        assert!(!v.advice.contains("remembers"), "{}", v.advice);
        assert!(v.advice.contains("write each result down"), "{}", v.advice);
        // A history that has not seen them together still promises.
        let fresh = judge(&now, &summary(RunEnd::Completed, 1, 12 * GIB));
        assert!(fresh.advice.contains("remembers"), "{}", fresh.advice);
    }

    #[test]
    fn pass_advice_with_everything_tested_does_not_mention_the_rest() {
        let slots = vec![slot("A1", Some(stick(8192, Some("S"))))];
        let v = judge(&slots, &summary(RunEnd::Completed, 0, 8 * GIB));
        assert!(v.advice.starts_with("Tested 8 GB of 8 GB."), "{}", v.advice);
        assert!(!v.advice.contains("the rest"), "{}", v.advice);
    }

    #[test]
    fn pass_advice_without_known_sticks_omits_installed_amount() {
        let v = judge(&[], &summary(RunEnd::Completed, 0, GIB * 3 / 2));
        assert_eq!(v.overall, Overall::Pass);
        assert!(v.per_slot.is_empty());
        assert!(
            v.advice.starts_with("Tested 1.5 GB of memory."),
            "{}",
            v.advice
        );
        assert!(!v.advice.contains(" of 0 GB"), "{}", v.advice);
    }

    #[test]
    fn errors_with_one_stick_among_empty_slots_fail_that_stick() {
        let slots = vec![
            slot("A1", None),
            slot("A2", Some(stick(16384, Some("AAA")))),
            slot("B1", None),
        ];
        let v = judge(&slots, &summary(RunEnd::Completed, 3, 14 * GIB));
        assert_eq!(v.overall, Overall::FailSingle);
        assert_eq!(v.per_slot, vec![Untested, Failed, Untested]);
        assert_eq!(v.headline, "This memory stick failed the test");
        assert!(v.advice.contains("same slot"), "{}", v.advice);
        assert!(v.advice.contains("replaced"), "{}", v.advice);
        assert!(v.advice.contains("warranty"), "{}", v.advice);
        assert!(v.advice.contains("memory speed setting"), "{}", v.advice);
        // A pass in another slot would not clear the stored failure, so the
        // advice must not suggest one.
        assert!(!v.advice.contains("different slot"), "{}", v.advice);
    }

    #[test]
    fn errors_with_several_sticks_make_all_installed_suspect() {
        let slots = two_of_four();
        let v = judge(&slots, &summary(RunEnd::Completed, 1, 28 * GIB));
        assert_eq!(v.overall, Overall::FailMultiple);
        assert_eq!(v.per_slot, vec![Untested, Suspect, Untested, Suspect]);
        assert!(v.advice.contains("cannot tell which"), "{}", v.advice);
        assert!(v.advice.contains("leave only one stick in"), "{}", v.advice);
        assert!(
            v.advice.contains("repeating for each stick"),
            "{}",
            v.advice
        );
        assert!(v.advice.contains("remembers each"), "{}", v.advice);
    }

    #[test]
    fn several_sticks_without_serials_are_not_promised_to_be_remembered() {
        let slots = vec![
            slot("A1", Some(stick(8192, Some("AAA")))),
            slot("A2", Some(stick(8192, None))),
        ];
        let v = judge(&slots, &summary(RunEnd::Completed, 1, 12 * GIB));
        assert_eq!(v.overall, Overall::FailMultiple);
        assert!(v.advice.contains("leave only one stick in"), "{}", v.advice);
        assert!(!v.advice.contains("remembers"), "{}", v.advice);
        assert!(v.advice.contains("write each result down"), "{}", v.advice);
        // Only one of the two lacks a serial, so do not say "these sticks".
        assert!(v.advice.contains("some of these sticks"), "{}", v.advice);
    }

    #[test]
    fn errors_count_even_when_stopped_early() {
        let one = vec![slot("A1", Some(stick(8192, Some("S"))))];
        let v = judge(&one, &summary(RunEnd::Stopped, 1, 6 * GIB));
        assert_eq!(v.overall, Overall::FailSingle);
        assert_eq!(v.per_slot, vec![Failed]);

        let v = judge(&two_of_four(), &summary(RunEnd::Stopped, 5, 6 * GIB));
        assert_eq!(v.overall, Overall::FailMultiple);
        assert_eq!(v.per_slot, vec![Untested, Suspect, Untested, Suspect]);
    }

    #[test]
    fn errors_with_no_known_sticks() {
        let v = judge(&[], &summary(RunEnd::Completed, 2, 6 * GIB));
        assert_eq!(v.overall, Overall::FailUnknownSticks);
        assert!(v.per_slot.is_empty());

        let empty_slots = vec![slot("A1", None), slot("A2", None)];
        let v = judge(&empty_slots, &summary(RunEnd::Stopped, 2, 6 * GIB));
        assert_eq!(v.overall, Overall::FailUnknownSticks);
        assert_eq!(v.per_slot, vec![Untested, Untested]);
        // Covers both a one-stick and a several-stick computer, with the how-to.
        assert!(v.advice.contains("only one stick"), "{}", v.advice);
        assert!(v.advice.contains("leave only one stick in"), "{}", v.advice);
        assert!(
            v.advice
                .ends_with("repeating for each stick and writing each result down."),
            "{}",
            v.advice
        );
        assert!(!v.advice.contains("remembers"), "{}", v.advice);
    }

    #[test]
    fn allocation_failure_tests_nothing() {
        let v = judge(&two_of_four(), &summary(RunEnd::AllocationFailed, 0, 0));
        assert_eq!(v.overall, Overall::CouldNotTest);
        assert_eq!(v.per_slot, vec![Untested; 4]);
        assert!(v.advice.contains("nothing was tested"), "{}", v.advice);
    }

    #[test]
    fn stopped_before_the_first_full_pass_is_inconclusive() {
        let v = judge(
            &two_of_four(),
            &after(0, summary(RunEnd::Stopped, 0, 28 * GIB)),
        );
        assert_eq!(v.overall, Overall::Inconclusive);
        assert_eq!(v.per_slot, vec![Untested; 4]);
        assert_eq!(v.headline, "Test stopped early");
        assert!(
            v.advice.contains("before it finished one full pass"),
            "{}",
            v.advice
        );
        // It asks for something possible even for a run that never ends.
        assert!(!v.advice.contains("to the end"), "{}", v.advice);
        assert!(v.advice.contains("at least one pass"), "{}", v.advice);
    }

    #[test]
    fn completed_without_a_single_pass_is_not_a_pass() {
        let v = judge(
            &two_of_four(),
            &after(0, summary(RunEnd::Completed, 0, 28 * GIB)),
        );
        assert_eq!(v.overall, Overall::CouldNotTest);
        assert_eq!(v.per_slot, vec![Untested; 4]);
    }

    #[test]
    fn wording_is_calm_and_short() {
        let cases = [
            judge(&two_of_four(), &summary(RunEnd::Completed, 0, 28 * GIB)),
            judge(&two_of_four(), &summary(RunEnd::Completed, 1, 28 * GIB)),
            judge(
                &two_of_four()[1..2],
                &summary(RunEnd::Completed, 1, 14 * GIB),
            ),
            judge(&[], &summary(RunEnd::Completed, 1, GIB)),
            judge(&[], &summary(RunEnd::Completed, 0, GIB)),
            judge(&two_of_four(), &after(0, summary(RunEnd::Stopped, 0, GIB))),
            judge(&two_of_four(), &summary(RunEnd::AllocationFailed, 0, 0)),
            // Pass after several passes, completed or stopped.
            judge(
                &two_of_four(),
                &after(2, summary(RunEnd::Completed, 0, 28 * GIB)),
            ),
            judge(
                &two_of_four(),
                &after(4, summary(RunEnd::Completed, 0, 28 * GIB)),
            ),
            judge(
                &two_of_four(),
                &after(1, summary(RunEnd::Stopped, 0, 28 * GIB)),
            ),
            judge(
                &two_of_four(),
                &after(90, summary(RunEnd::Stopped, 0, 28 * GIB)),
            ),
            // FailMultiple without the remember promise.
            judge(
                &[
                    slot("A1", Some(stick(8192, Some("S")))),
                    slot("A2", Some(stick(8192, None))),
                ],
                &summary(RunEnd::Completed, 1, 12 * GIB),
            ),
            // FailUnknownSticks with empty slots, and with one undercounted stick.
            judge(
                &[slot("A1", None), slot("A2", None)],
                &summary(RunEnd::Completed, 1, GIB),
            ),
            judge(
                &two_of_four()[1..2],
                &summary(RunEnd::Completed, 1, 24 * GIB),
            ),
            // Pass with less than 0.1 GB tested.
            judge(&two_of_four(), &summary(RunEnd::Completed, 0, MIB)),
        ];
        for v in cases {
            assert!(!v.headline.is_empty());
            // The advice is shown right under the headline, so it must not
            // repeat it.
            let headline = v.headline.to_lowercase();
            assert!(
                !v.advice.to_lowercase().contains(&headline),
                "advice repeats the headline: {} / {}",
                v.headline,
                v.advice
            );
            assert!(!v.advice.contains('!'), "{}", v.advice);
            assert!(!v.headline.contains('!'), "{}", v.headline);
            assert!(!v.advice.contains("  "), "double space: {}", v.advice);
            let sentences = v.advice.matches(". ").count() + 1;
            assert!((1..=3).contains(&sentences), "{}", v.advice);
            assert!(v.advice.ends_with('.'), "{}", v.advice);
        }
    }
}
