//! The pretend computers behind `--demo=<name>`. Their sticks are made up,
//! but the memory test itself is real (capped at [`DEMO_MAX_BYTES`]).

use memory_tester::history::History;
use memory_tester::types::{Slot, Stick, StickStatus};
use memory_tester::verdict::{Overall, Verdict};

use super::cli::Demo;

/// How much memory a demo run tests, so it finishes in seconds.
pub const DEMO_MAX_BYTES: u64 = 256 * 1024 * 1024;

/// A date in the past for the results a demo pretends to remember
/// (2026-11-02, 00:00 UTC).
const REMEMBERED_AT: u64 = 1_793_577_600;

impl Demo {
    /// The slots this pretend computer has. Empty for [`Demo::NoSticks`],
    /// which stands for detection having failed.
    pub fn slots(self) -> Vec<Slot> {
        let corsair = |serial: &str| {
            stick(
                16384,
                "Corsair",
                "CMK32GX4M2E3200C16",
                Some(serial),
                Some(3200),
                "DDR4",
            )
        };
        match self {
            Demo::Pass | Demo::Fail => vec![
                slot("DIMM_A1", None),
                slot("DIMM_A2", Some(corsair("DEMO-0001"))),
                slot("DIMM_B1", None),
                slot("DIMM_B2", Some(corsair("DEMO-0002"))),
            ],
            Demo::SingleFail => vec![
                slot("DIMM_A1", None),
                slot("DIMM_A2", Some(corsair("DEMO-0001"))),
                slot("DIMM_B1", None),
                slot("DIMM_B2", None),
            ],
            Demo::Eight => eight_slots(),
            Demo::NoSticks => Vec::new(),
        }
    }

    /// Whether this demo's run corrupts a word on purpose.
    pub fn simulates_fault(self) -> bool {
        matches!(self, Demo::Fail | Demo::SingleFail)
    }

    /// The remembered results this demo starts with, held in memory only.
    ///
    /// [`Demo::Eight`] remembers one of its installed sticks as failed and two
    /// sticks that are not installed, so every part of the window that shows
    /// remembered results has something in it. The others start empty.
    pub fn history(self) -> History {
        // No file behind it: even an accidental save writes nothing.
        let mut history = History::in_memory();
        if self == Demo::Eight {
            let installed = &eight_slots()[1];
            let gone = [
                slot(
                    "",
                    Some(stick(
                        8192,
                        "Crucial",
                        "CT8G4DFRA32A",
                        Some("DEMO-OLD-1"),
                        Some(3200),
                        "DDR4",
                    )),
                ),
                slot(
                    "",
                    Some(stick(
                        8192,
                        "Crucial",
                        "CT8G4DFRA32A",
                        Some("DEMO-OLD-2"),
                        Some(3200),
                        "DDR4",
                    )),
                ),
            ];
            remember(&mut history, installed, StickStatus::Failed, REMEMBERED_AT);
            remember(&mut history, &gone[0], StickStatus::Passed, REMEMBERED_AT);
            remember(
                &mut history,
                &gone[1],
                StickStatus::Failed,
                REMEMBERED_AT + 86_400,
            );
        }
        history
    }
}

/// Eight slots, six filled: long part numbers, odd sizes, and one stick that
/// reports only a size and a manufacturer, like some real firmware does.
fn eight_slots() -> Vec<Slot> {
    let gskill = |serial: &str| {
        stick(
            32768,
            "G.Skill",
            "F4-3600C16Q2-256GTZN-RGB-EXTENDED",
            Some(serial),
            Some(3600),
            "DDR4",
        )
    };
    vec![
        slot("DIMM_A1", Some(gskill("DEMO-8A1"))),
        slot("DIMM_A2", Some(gskill("DEMO-8A2"))),
        slot("DIMM_B1", Some(gskill("DEMO-8B1"))),
        slot("DIMM_B2", None),
        slot(
            "DIMM_C1",
            Some(stick(
                15360,
                "Kingston",
                "KF432C16BB1K2/32-FURY-BEAST-BLACK",
                Some("DEMO-8C1"),
                None,
                "",
            )),
        ),
        slot("DIMM_C2", None),
        slot(
            "DIMM_D1",
            Some(stick(1024, "Micron Technology", "", None, None, "")),
        ),
        slot(
            "ChannelD-DIMM2-WithAVeryLongLocatorName",
            Some(stick(16384, "", "", Some("DEMO-8D2"), Some(2666), "DDR4")),
        ),
    ]
}

fn slot(locator: &str, stick: Option<Stick>) -> Slot {
    Slot {
        locator: locator.to_string(),
        bank: String::new(),
        stick,
    }
}

fn stick(
    size_mb: u64,
    manufacturer: &str,
    part_number: &str,
    serial: Option<&str>,
    speed_mts: Option<u32>,
    memory_type: &str,
) -> Stick {
    Stick {
        size_mb,
        manufacturer: manufacturer.to_string(),
        part_number: part_number.to_string(),
        serial: serial.map(str::to_string),
        speed_mts,
        memory_type: memory_type.to_string(),
    }
}

/// Records one result for one stick, as a finished run would.
fn remember(history: &mut History, slot: &Slot, status: StickStatus, when_unix: u64) {
    let verdict = Verdict {
        overall: Overall::Pass,
        per_slot: vec![status],
        headline: String::new(),
        advice: String::new(),
    };
    history.apply(std::slice::from_ref(slot), &verdict, when_unix);
}

#[cfg(test)]
mod tests {
    use super::*;
    use memory_tester::history::StickKey;

    fn installed(slots: &[Slot]) -> Vec<&Stick> {
        slots.iter().filter_map(|s| s.stick.as_ref()).collect()
    }

    #[test]
    fn pass_and_fail_have_two_16_gb_ddr4_sticks_with_serials_in_four_slots() {
        for demo in [Demo::Pass, Demo::Fail] {
            let slots = demo.slots();
            assert_eq!(slots.len(), 4, "{demo:?}");
            let sticks = installed(&slots);
            assert_eq!(sticks.len(), 2, "{demo:?}");
            for s in sticks {
                assert_eq!(s.size_mb, 16 * 1024);
                assert_eq!(s.memory_type, "DDR4");
                assert!(StickKey::from_stick(s).is_some(), "needs a serial");
            }
        }
        assert!(!Demo::Pass.simulates_fault());
        assert!(Demo::Fail.simulates_fault());
    }

    #[test]
    fn single_fail_has_one_stick_and_simulates_a_fault() {
        let slots = Demo::SingleFail.slots();
        assert_eq!(slots.len(), 4);
        assert_eq!(installed(&slots).len(), 1);
        assert!(Demo::SingleFail.simulates_fault());
    }

    #[test]
    fn eight_has_six_of_eight_filled_with_long_part_numbers() {
        let slots = Demo::Eight.slots();
        assert_eq!(slots.len(), 8);
        let sticks = installed(&slots);
        assert_eq!(sticks.len(), 6);
        assert!(sticks.iter().any(|s| s.part_number.len() > 30));
        // One stick reports nothing but a size and a manufacturer.
        assert!(sticks.iter().any(|s| s.part_number.is_empty()
            && s.serial.is_none()
            && s.speed_mts.is_none()
            && s.memory_type.is_empty()));
        assert!(!Demo::Eight.simulates_fault());
    }

    #[test]
    fn nosticks_detects_nothing() {
        assert!(Demo::NoSticks.slots().is_empty());
        assert!(!Demo::NoSticks.simulates_fault());
    }

    #[test]
    fn only_eight_starts_with_remembered_results() {
        for demo in [Demo::Pass, Demo::Fail, Demo::SingleFail, Demo::NoSticks] {
            assert!(demo.history().records().is_empty(), "{demo:?}");
        }
        let history = Demo::Eight.history();
        assert_eq!(history.records().len(), 3);
        // One of them belongs to a stick that is installed in the demo.
        let slots = Demo::Eight.slots();
        let key = StickKey::from_stick(slots[1].stick.as_ref().unwrap()).unwrap();
        assert_eq!(history.get(&key).unwrap().status, StickStatus::Failed);
    }

    #[test]
    fn demo_history_has_no_file_behind_it() {
        // A history loaded from a file would fail here or write the file;
        // an in-memory one does nothing and keeps its records.
        let history = Demo::Eight.history();
        history.save().unwrap();
        assert_eq!(history.records().len(), 3);
    }
}
