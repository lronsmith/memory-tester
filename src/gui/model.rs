//! What the window shows and how it changes, kept apart from the drawing so
//! it can be tested without a window.

use std::sync::{Arc, Mutex, Weak};
use std::time::{SystemTime, UNIX_EPOCH};

use memory_tester::engine::{self, Progress, TestConfig, TestHandle};
use memory_tester::history::{History, Record, StickKey, rememberable_keys};
use memory_tester::types::{Slot, StickStatus, TestSummary};
use memory_tester::verdict::{self, Overall, Verdict};

use super::cli::Demo;
use super::demo::DEMO_MAX_BYTES;
use super::text::{self, Shown};

/// How long a run goes on for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Length {
    /// One pass.
    Quick,
    /// Four passes.
    Thorough,
    /// Passes until the person presses Stop.
    UntilStopped,
}

impl Length {
    /// Every choice, in the order offered; the first is the default.
    pub const ALL: [Length; 3] = [Length::Quick, Length::Thorough, Length::UntilStopped];

    /// The words shown for this choice.
    pub fn label(self) -> &'static str {
        match self {
            Length::Quick => "Quick (1 pass)",
            Length::Thorough => "Thorough (4 passes)",
            Length::UntilStopped => "Until I stop it",
        }
    }

    /// Passes to ask the engine for; `None` means until stopped.
    pub fn passes(self) -> Option<u32> {
        match self {
            Length::Quick => Some(1),
            Length::Thorough => Some(4),
            Length::UntilStopped => None,
        }
    }
}

/// The run in progress, reachable from the panic hook (see
/// [`stop_active_run`]). Only a weak reference: the model owns the run.
static ACTIVE_RUN: Mutex<Weak<TestHandle>> = Mutex::new(Weak::new());

/// Asks the run in progress, if there is one, to stop. Meant for the panic
/// hook: it never waits for a lock and never panics.
pub fn stop_active_run() {
    if let Ok(active) = ACTIVE_RUN.try_lock()
        && let Some(handle) = active.upgrade()
    {
        handle.stop();
    }
}

/// What to do when the person closes the window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseAction {
    /// Nothing is running: let the window close.
    Close,
    /// A run is going: keep the window open while the run stops and gives
    /// its memory back, then close (see [`Model::ready_to_close`]).
    WaitForStop,
}

/// A run in progress.
#[derive(Debug)]
pub struct Running {
    /// Shared only with [`ACTIVE_RUN`], which holds a weak reference.
    handle: Arc<TestHandle>,
    /// The latest snapshot, refreshed by [`Model::poll`].
    pub progress: Progress,
    /// Stop has been asked for.
    pub stopping: bool,
}

/// The result of the last run.
#[derive(Debug, Clone)]
pub struct Finished {
    /// What the verdict module made of it.
    pub verdict: Verdict,
    /// The engine's raw result.
    pub summary: TestSummary,
    /// Set when the result could not be saved to the history file.
    pub save_error: Option<String>,
}

/// Where the window is in its life.
#[derive(Debug)]
pub enum RunState {
    /// No run yet.
    Idle,
    /// A run is going.
    Running(Box<Running>),
    /// A run has ended; another can be started.
    Finished(Box<Finished>),
}

/// What one slot (or the stand-in block for undetected memory) shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlotShown {
    /// Decides the colour and status word.
    pub shown: Shown,
    /// A small caption about an earlier result, e.g. "Remembered from 2026-11-23".
    pub note: Option<String>,
}

/// One remembered stick that is not installed now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PastResult {
    /// e.g. "Corsair CMK32GX4M2E3200C16, 16 GB".
    pub label: String,
    /// `Passed` or `Failed`.
    pub status: StickStatus,
    /// e.g. "2026-11-23".
    pub date: String,
}

/// Everything behind the window.
#[derive(Debug)]
pub struct Model {
    /// Slots as detected (or made up in demo mode); empty if detection failed.
    pub slots: Vec<Slot>,
    /// Remembered results.
    pub history: History,
    /// False in demo mode: history then lives in memory only.
    persist: bool,
    demo: Option<Demo>,
    /// The run length currently chosen.
    pub length: Length,
    /// Idle, running or finished.
    pub run: RunState,
    /// The "Forget saved results" control is waiting for its confirming click.
    pub confirm_forget: bool,
    /// Set when forgetting could not be saved.
    pub forget_error: Option<String>,
    /// The window was closed during a run: close it once the run has ended.
    close_when_stopped: bool,
}

impl Model {
    /// A model for the real computer: history is saved to `history`'s file.
    pub fn new(slots: Vec<Slot>, history: History) -> Model {
        Model::build(slots, history, true, None)
    }

    /// A model for a pretend computer: its history is never saved.
    pub fn demo(demo: Demo) -> Model {
        Model::build(demo.slots(), demo.history(), false, Some(demo))
    }

    fn build(slots: Vec<Slot>, history: History, persist: bool, demo: Option<Demo>) -> Model {
        Model {
            slots,
            history,
            persist,
            demo,
            length: Length::ALL[0],
            run: RunState::Idle,
            confirm_forget: false,
            forget_error: None,
            close_when_stopped: false,
        }
    }

    /// The demo being shown, if any.
    pub fn demo_kind(&self) -> Option<Demo> {
        self.demo
    }

    /// True when results are saved to the history file (false in demo mode).
    pub fn persists(&self) -> bool {
        self.persist
    }

    /// True while a run is going.
    pub fn is_running(&self) -> bool {
        matches!(self.run, RunState::Running(_))
    }

    /// The engine settings for a run with the chosen length.
    pub fn test_config(&self) -> TestConfig {
        let mut config = TestConfig {
            passes: self.length.passes(),
            ..TestConfig::default()
        };
        if let Some(demo) = self.demo {
            config.max_bytes = Some(DEMO_MAX_BYTES);
            config.simulate_fault = demo.simulates_fault();
        }
        config
    }

    /// Starts a run, unless one is already going.
    pub fn start(&mut self) {
        self.start_with(self.test_config());
    }

    fn start_with(&mut self, config: TestConfig) {
        if self.is_running() {
            return;
        }
        let handle = Arc::new(engine::start(config));
        *ACTIVE_RUN.lock().unwrap_or_else(|e| e.into_inner()) = Arc::downgrade(&handle);
        let progress = handle.progress();
        self.confirm_forget = false;
        self.run = RunState::Running(Box::new(Running {
            handle,
            progress,
            stopping: false,
        }));
    }

    /// Asks the current run to stop; it ends at a later [`Model::poll`].
    pub fn stop(&mut self) {
        if let RunState::Running(run) = &mut self.run {
            run.handle.stop();
            run.stopping = true;
        }
    }

    /// Refreshes the progress of a run, and once the run is over judges it,
    /// records it in the history and saves that. Never blocks for long:
    /// it waits for the engine only after the engine reports it has finished.
    pub fn poll(&mut self) {
        let RunState::Running(run) = &mut self.run else {
            return;
        };
        run.progress = run.handle.progress();
        if !run.handle.is_finished() {
            return;
        }
        if let Some(handle) = self.take_run() {
            let summary = handle.join();
            self.finish(summary, unix_now());
        }
    }

    /// Stops any run and waits for it to give its memory back. For shutdown.
    pub fn stop_and_wait(&mut self) {
        if let Some(handle) = self.take_run() {
            handle.stop();
            let summary = handle.join();
            // The result is still worth keeping (e.g. errors found before
            // the window was closed).
            self.finish(summary, unix_now());
        }
    }

    /// The person closed the window. If a run is going it is told to stop,
    /// and the window should stay open until [`Model::ready_to_close`].
    pub fn request_close(&mut self) -> CloseAction {
        if self.is_running() {
            self.stop();
            self.close_when_stopped = true;
            CloseAction::WaitForStop
        } else {
            CloseAction::Close
        }
    }

    /// True once a close that had to wait for a run can go ahead.
    pub fn ready_to_close(&self) -> bool {
        self.close_when_stopped && !self.is_running()
    }

    /// Takes the run's handle out of the state, leaving any other state as
    /// it is, and unregisters it from the panic hook.
    fn take_run(&mut self) -> Option<TestHandle> {
        let run = match std::mem::replace(&mut self.run, RunState::Idle) {
            RunState::Running(run) => run,
            other => {
                self.run = other;
                return None;
            }
        };
        *ACTIVE_RUN.lock().unwrap_or_else(|e| e.into_inner()) = Weak::new();
        // The model holds the only strong reference; the panic hook only
        // upgrades its weak one for an instant while the process is failing.
        Arc::into_inner(run.handle)
    }

    /// Turns a finished run into a [`Finished`] state, updating history.
    fn finish(&mut self, summary: TestSummary, now_unix: u64) {
        let verdict = verdict::judge(&self.slots, &summary);
        let changed = self.history.apply(&self.slots, &verdict, now_unix);
        let save_error = if changed && self.persist {
            self.history
                .save()
                .err()
                .map(|e| format!("This result could not be saved for next time ({e})."))
        } else {
            None
        };
        self.run = RunState::Finished(Box::new(Finished {
            verdict,
            summary,
            save_error,
        }));
    }

    /// First click on "Forget saved results": ask for confirmation.
    pub fn ask_forget(&mut self) {
        self.confirm_forget = true;
        self.forget_error = None;
    }

    /// Second click: clear the remembered results and save that.
    pub fn forget(&mut self) {
        self.confirm_forget = false;
        self.history.clear();
        self.forget_error = if self.persist {
            self.history
                .save()
                .err()
                .map(|e| format!("Could not save that change ({e})."))
        } else {
            None
        };
    }

    /// The verdict of the last run, if a run has finished.
    pub fn finished(&self) -> Option<&Finished> {
        match &self.run {
            RunState::Finished(f) => Some(f),
            _ => None,
        }
    }

    /// True when no installed stick was detected, so the window shows one
    /// "Installed memory" block instead of sticks.
    pub fn sticks_unknown(&self) -> bool {
        self.slots.iter().all(|s| s.stick.is_none())
    }

    /// What each slot shows, one entry per slot.
    pub fn slot_views(&self) -> Vec<SlotShown> {
        resolve_slots(
            &self.slots,
            self.finished().map(|f| f.verdict.per_slot.as_slice()),
            &self.history,
            self.is_running(),
        )
    }

    /// What the "Installed memory" block shows when no sticks were detected.
    pub fn unknown_sticks_view(&self) -> Shown {
        if self.is_running() {
            Shown::Testing
        } else {
            Shown::Status(overall_status(self.finished().map(|f| f.verdict.overall)))
        }
    }

    /// Remembered sticks that are not installed now, newest first.
    pub fn past_results(&self) -> Vec<PastResult> {
        past_results(&self.slots, self.history.records())
    }
}

/// Seconds since the Unix epoch, 0 if the clock is before it.
fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Decides what each slot shows.
///
/// - An empty slot shows `Empty`.
/// - While a run is going, every installed stick shows `Testing`, keeping its
///   "Remembered from" note so the layout does not jump.
/// - A remembered `Failed` beats a `Passed` from this session: the stick stays
///   `Failed`, with a note that it passed this time. The history keeps a
///   failure until the person clears it, so this is also what the window
///   shows after a restart.
/// - Otherwise a conclusive result from this session (`session`, one per
///   slot) beats a remembered one. A session `Suspect` for a stick remembered
///   as failed notes that earlier failure.
/// - Otherwise the remembered result is shown with its date, if the stick
///   can be recognised (see [`rememberable_keys`]).
/// - Otherwise the stick is `Untested`.
pub fn resolve_slots(
    slots: &[Slot],
    session: Option<&[StickStatus]>,
    history: &History,
    running: bool,
) -> Vec<SlotShown> {
    let keys = rememberable_keys(slots);
    slots
        .iter()
        .zip(keys)
        .enumerate()
        .map(|(i, (slot, key))| {
            if slot.stick.is_none() {
                return SlotShown {
                    shown: Shown::Empty,
                    note: None,
                };
            }
            let remembered = key.as_ref().and_then(|k| history.get(k));
            if running {
                return SlotShown {
                    shown: Shown::Testing,
                    note: remembered.map(|r| text::remembered_note(r.when_unix)),
                };
            }
            let failed_before = remembered
                .filter(|r| r.status == StickStatus::Failed)
                .map(|r| r.when_unix);
            let now = session
                .and_then(|s| s.get(i).copied())
                .filter(|&s| s != StickStatus::Untested);
            match (now, remembered) {
                (Some(StickStatus::Passed), _) if failed_before.is_some() => SlotShown {
                    shown: Shown::Status(StickStatus::Failed),
                    note: failed_before.map(text::passed_but_failed_note),
                },
                (Some(StickStatus::Suspect), _) if failed_before.is_some() => SlotShown {
                    shown: Shown::Status(StickStatus::Suspect),
                    note: failed_before.map(text::failed_alone_note),
                },
                (Some(status), _) => SlotShown {
                    shown: Shown::Status(status),
                    note: None,
                },
                (None, Some(r)) => SlotShown {
                    shown: Shown::Status(r.status),
                    note: Some(text::remembered_note(r.when_unix)),
                },
                (None, None) => SlotShown {
                    shown: Shown::Status(StickStatus::Untested),
                    note: None,
                },
            }
        })
        .collect()
}

/// The colour-giving status for a whole run's outcome: used for the result
/// banner, and for the single "Installed memory" block shown when no sticks
/// were detected. Errors that cannot be pinned on one stick give `Suspect`,
/// as the verdict does for sticks it cannot single out.
pub fn overall_status(overall: Option<Overall>) -> StickStatus {
    match overall {
        Some(Overall::Pass) => StickStatus::Passed,
        Some(Overall::FailSingle) => StickStatus::Failed,
        Some(Overall::FailMultiple | Overall::FailUnknownSticks) => StickStatus::Suspect,
        Some(Overall::Inconclusive | Overall::CouldNotTest) | None => StickStatus::Untested,
    }
}

/// Remembered results for sticks not installed now, newest first. A stick
/// counts as installed when any installed stick has the same key, so a
/// result is never listed as "not installed" for a stick that is in the
/// computer.
pub fn past_results(slots: &[Slot], records: &[Record]) -> Vec<PastResult> {
    let installed: Vec<StickKey> = slots
        .iter()
        .filter_map(|s| s.stick.as_ref().and_then(StickKey::from_stick))
        .collect();
    records
        .iter()
        .rev()
        .filter(|r| !installed.contains(&r.key))
        .map(|r| PastResult {
            label: r.label.clone(),
            status: r.status,
            date: memory_tester::history::format_date(r.when_unix),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use StickStatus::{Failed, Passed, Suspect, Untested};
    use memory_tester::types::{RunEnd, Stick};
    use std::time::{Duration, Instant};

    /// Tests that start runs go one at a time: they share the process-wide
    /// registration used by [`stop_active_run`].
    static ENGINE: Mutex<()> = Mutex::new(());

    const DAY1: u64 = 1_795_392_000; // 2026-11-23
    const DAY0: u64 = DAY1 - 86_400; // 2026-11-22

    fn stick(serial: Option<&str>) -> Stick {
        Stick {
            size_mb: 16384,
            manufacturer: "Corsair".to_string(),
            part_number: "CMK".to_string(),
            serial: serial.map(str::to_string),
            speed_mts: Some(3200),
            memory_type: "DDR4".to_string(),
        }
    }

    fn slot(stick: Option<Stick>) -> Slot {
        Slot {
            locator: "A".to_string(),
            bank: String::new(),
            stick,
        }
    }

    fn empty_history() -> History {
        History::in_memory()
    }

    /// A history remembering `status` for each stick in `slots`.
    fn remembering(slots: &[Slot], status: StickStatus, when: u64) -> History {
        let mut h = empty_history();
        let v = Verdict {
            overall: Overall::Pass,
            per_slot: vec![status; slots.len()],
            headline: String::new(),
            advice: String::new(),
        };
        h.apply(slots, &v, when);
        h
    }

    fn shown(s: StickStatus) -> Shown {
        Shown::Status(s)
    }

    #[test]
    fn length_choices_in_order_with_quick_first() {
        let labels: Vec<&str> = Length::ALL.iter().map(|l| l.label()).collect();
        assert_eq!(
            labels,
            ["Quick (1 pass)", "Thorough (4 passes)", "Until I stop it"]
        );
        assert_eq!(Length::Quick.passes(), Some(1));
        assert_eq!(Length::Thorough.passes(), Some(4));
        assert_eq!(Length::UntilStopped.passes(), None);
        let model = Model::new(Vec::new(), empty_history());
        assert_eq!(model.length, Length::Quick);
    }

    #[test]
    fn real_runs_test_as_much_as_is_safe_and_demo_runs_are_capped() {
        let mut real = Model::new(Demo::Fail.slots(), empty_history());
        real.length = Length::Thorough;
        assert_eq!(
            real.test_config(),
            TestConfig {
                passes: Some(4),
                max_bytes: None,
                threads: None,
                simulate_fault: false
            }
        );
        let mut demo = Model::demo(Demo::Fail);
        demo.length = Length::UntilStopped;
        let config = demo.test_config();
        assert_eq!(config.passes, None);
        assert_eq!(config.max_bytes, Some(DEMO_MAX_BYTES));
        assert!(config.simulate_fault);
        assert!(!Model::demo(Demo::Pass).test_config().simulate_fault);
        // Only the real computer's results are written to the history file.
        assert!(real.persists());
        assert!(!demo.persists());
    }

    #[test]
    fn empty_slot_shows_empty_even_while_running() {
        let slots = [slot(None)];
        for running in [false, true] {
            let views = resolve_slots(&slots, Some(&[Untested]), &empty_history(), running);
            assert_eq!(views[0].shown, Shown::Empty);
        }
    }

    #[test]
    fn untested_without_session_or_memory() {
        let views = resolve_slots(
            &[slot(Some(stick(Some("S"))))],
            None,
            &empty_history(),
            false,
        );
        assert_eq!(
            views,
            vec![SlotShown {
                shown: shown(Untested),
                note: None
            }]
        );
    }

    #[test]
    fn remembered_result_shown_when_there_is_no_session_result() {
        let slots = [slot(Some(stick(Some("S"))))];
        let history = remembering(&slots, Passed, DAY1);
        for session in [None, Some(&[Untested][..])] {
            let views = resolve_slots(&slots, session, &history, false);
            assert_eq!(views[0].shown, shown(Passed));
            assert_eq!(views[0].note.as_deref(), Some("Remembered from 2026-11-23"));
        }
    }

    #[test]
    fn session_result_beats_remembered_result() {
        let slots = [slot(Some(stick(Some("S"))))];
        let history = remembering(&slots, Passed, DAY0);
        let views = resolve_slots(&slots, Some(&[Suspect]), &history, false);
        assert_eq!(
            views[0],
            SlotShown {
                shown: shown(Suspect),
                note: None
            }
        );
    }

    #[test]
    fn remembered_failure_beats_a_later_pass() {
        let slots = [slot(Some(stick(Some("S"))))];
        let history = remembering(&slots, Failed, DAY0);
        let views = resolve_slots(&slots, Some(&[Passed]), &history, false);
        assert_eq!(
            views[0],
            SlotShown {
                shown: shown(Failed),
                note: Some(
                    "Passed this time, but failed on 2026-11-22. \
                     A faulty stick can pass a short test."
                        .to_string()
                ),
            }
        );
        // Same colour and word after a restart (no session result).
        let after_restart = resolve_slots(&slots, None, &history, false);
        assert_eq!(after_restart[0].shown, views[0].shown);
    }

    #[test]
    fn session_failure_or_suspect_beats_anything_remembered() {
        let slots = [slot(Some(stick(Some("S"))))];
        let passed = remembering(&slots, Passed, DAY0);
        for session in [Failed, Suspect] {
            let views = resolve_slots(&slots, Some(&[session]), &passed, false);
            assert_eq!(
                views[0],
                SlotShown {
                    shown: shown(session),
                    note: None
                }
            );
        }
        let failed = remembering(&slots, Failed, DAY0);
        let views = resolve_slots(&slots, Some(&[Failed]), &failed, false);
        assert_eq!(
            views[0],
            SlotShown {
                shown: shown(Failed),
                note: None
            }
        );
        // Suspect stays amber, with the earlier failure noted.
        let views = resolve_slots(&slots, Some(&[Suspect]), &failed, false);
        assert_eq!(views[0].shown, shown(Suspect));
        assert_eq!(
            views[0].note.as_deref(),
            Some("Failed when tested alone on 2026-11-22.")
        );
    }

    #[test]
    fn session_pass_beats_a_remembered_pass() {
        let slots = [slot(Some(stick(Some("S"))))];
        let history = remembering(&slots, Passed, DAY0);
        let views = resolve_slots(&slots, Some(&[Passed]), &history, false);
        assert_eq!(
            views[0],
            SlotShown {
                shown: shown(Passed),
                note: None
            }
        );
    }

    #[test]
    fn running_shows_testing_and_keeps_the_remembered_note() {
        let slots = [
            slot(Some(stick(Some("S")))),
            slot(None),
            slot(Some(stick(None))),
        ];
        let history = remembering(&slots, Failed, DAY0);
        let views = resolve_slots(&slots, Some(&[Passed, Untested, Passed]), &history, true);
        assert_eq!(views[0].shown, Shown::Testing);
        assert_eq!(views[0].note.as_deref(), Some("Remembered from 2026-11-22"));
        assert_eq!(views[1].shown, Shown::Empty);
        assert_eq!(
            views[2],
            SlotShown {
                shown: Shown::Testing,
                note: None
            }
        );
    }

    #[test]
    fn stick_without_serial_shows_session_result_but_nothing_remembered() {
        let no_serial = [slot(Some(stick(None)))];
        // Its twin with a serial is remembered; that must not leak across.
        let history = remembering(&[slot(Some(stick(Some("S"))))], Failed, DAY0);
        let views = resolve_slots(&no_serial, None, &history, false);
        assert_eq!(views[0].shown, shown(Untested));
        let views = resolve_slots(&no_serial, Some(&[Passed]), &history, false);
        assert_eq!(views[0].shown, shown(Passed));
        assert_eq!(views[0].note, None);
    }

    #[test]
    fn sticks_sharing_a_key_do_not_show_a_remembered_result() {
        let one = [slot(Some(stick(Some("SAME"))))];
        let history = remembering(&one, Failed, DAY0);
        let twins = [
            slot(Some(stick(Some("SAME")))),
            slot(Some(stick(Some("SAME")))),
        ];
        let views = resolve_slots(&twins, None, &history, false);
        assert!(views.iter().all(|v| v.shown == shown(Untested)));
        // ...and that record is not listed as "not installed" either.
        assert!(past_results(&twins, history.records()).is_empty());
    }

    #[test]
    fn whole_memory_block_follows_the_outcome() {
        assert_eq!(overall_status(None), Untested);
        assert_eq!(overall_status(Some(Overall::Pass)), Passed);
        assert_eq!(overall_status(Some(Overall::FailSingle)), Failed);
        assert_eq!(overall_status(Some(Overall::FailMultiple)), Suspect);
        assert_eq!(overall_status(Some(Overall::FailUnknownSticks)), Suspect);
        assert_eq!(overall_status(Some(Overall::Inconclusive)), Untested);
        assert_eq!(overall_status(Some(Overall::CouldNotTest)), Untested);
    }

    #[test]
    fn past_results_lists_only_sticks_not_installed_newest_first() {
        let a = slot(Some(stick(Some("A"))));
        let b = slot(Some(stick(Some("B"))));
        let c = slot(Some(stick(Some("C"))));
        let mut h = remembering(std::slice::from_ref(&a), Passed, DAY0);
        let v = |s| Verdict {
            overall: Overall::Pass,
            per_slot: vec![s],
            headline: String::new(),
            advice: String::new(),
        };
        h.apply(std::slice::from_ref(&b), &v(Failed), DAY1);
        h.apply(std::slice::from_ref(&c), &v(Passed), DAY1 + 10);

        let rows = past_results(&[b.clone(), slot(None)], h.records());
        assert_eq!(
            rows,
            vec![
                PastResult {
                    label: "Corsair CMK, 16 GB".to_string(),
                    status: Passed,
                    date: "2026-11-23".to_string()
                },
                PastResult {
                    label: "Corsair CMK, 16 GB".to_string(),
                    status: Passed,
                    date: "2026-11-22".to_string()
                },
            ]
        );
        assert!(past_results(&[a, b, c], h.records()).is_empty());
    }

    fn summary(error_count: u64) -> TestSummary {
        TestSummary {
            end: RunEnd::Completed,
            tested_bytes: 1024 * 1024 * 1024,
            locked_bytes: 0,
            passes_completed: 1,
            error_count,
            errors: Vec::new(),
            elapsed: Duration::from_secs(5),
        }
    }

    #[test]
    fn finishing_judges_remembers_and_colours_the_sticks() {
        let mut model = Model::demo(Demo::SingleFail);
        model.finish(summary(1), DAY1);
        let finished = model.finished().unwrap();
        assert_eq!(finished.verdict.overall, Overall::FailSingle);
        assert_eq!(finished.save_error, None);
        let views = model.slot_views();
        assert_eq!(views[1].shown, shown(Failed));
        assert_eq!(views[0].shown, Shown::Empty);
        assert_eq!(model.history.records().len(), 1);
        assert_eq!(model.history.records()[0].status, Failed);
    }

    #[test]
    fn a_save_failure_becomes_a_note() {
        // A real model whose history file cannot be written: its "file" is a
        // folder.
        let dir = std::env::temp_dir().join(format!("memory-tester-gui-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut model = Model::new(Demo::Pass.slots(), History::load(&dir));
        model.finish(summary(0), DAY1);
        let note = model.finished().unwrap().save_error.clone();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(
            note.as_deref()
                .is_some_and(|n| n.starts_with("This result could not be saved")),
            "{note:?}"
        );
        // The result itself is still shown.
        assert_eq!(model.slot_views()[1].shown, shown(Passed));
    }

    #[test]
    fn forgetting_needs_a_confirming_click_and_clears_history() {
        let mut model = Model::demo(Demo::Eight);
        assert!(!model.history.records().is_empty());
        model.ask_forget();
        assert!(model.confirm_forget);
        assert!(!model.history.records().is_empty(), "first click only asks");
        model.forget();
        assert!(!model.confirm_forget);
        assert!(model.history.records().is_empty());
        assert_eq!(model.forget_error, None, "demo history is never saved");
        assert!(model.past_results().is_empty());
        assert_eq!(model.slot_views()[1].shown, shown(Untested));
    }

    #[test]
    fn unknown_sticks_block_while_idle_running_and_finished() {
        let mut model = Model::demo(Demo::NoSticks);
        assert!(model.sticks_unknown());
        assert_eq!(model.unknown_sticks_view(), shown(Untested));
        model.finish(summary(0), DAY1);
        assert_eq!(model.unknown_sticks_view(), shown(Passed));
        model.finish(summary(2), DAY1);
        assert_eq!(model.unknown_sticks_view(), shown(Suspect));
        assert!(!Model::demo(Demo::Pass).sticks_unknown());
    }

    /// The model's own run settings, with less memory so the test is quick.
    fn small(model: &Model) -> TestConfig {
        TestConfig {
            max_bytes: Some(4 * 1024 * 1024),
            ..model.test_config()
        }
    }

    /// Polls until the run is over, failing after a generous limit.
    fn run_to_end(model: &mut Model) {
        let deadline = Instant::now() + Duration::from_secs(60);
        while model.is_running() {
            assert!(Instant::now() < deadline, "run never ended");
            model.poll();
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn a_real_demo_run_goes_from_running_to_finished() {
        let _one_at_a_time = ENGINE.lock().unwrap_or_else(|e| e.into_inner());
        let mut model = Model::demo(Demo::Fail);
        model.ask_forget();
        model.start_with(small(&model));
        assert!(model.is_running());
        assert!(!model.confirm_forget, "starting cancels a pending forget");
        assert!(
            model
                .slot_views()
                .iter()
                .all(|v| v.shown != shown(Untested)),
            "installed sticks show Testing"
        );
        run_to_end(&mut model);
        let finished = model.finished().unwrap();
        assert_eq!(finished.summary.end, RunEnd::Completed);
        assert_eq!(finished.summary.error_count, 1);
        assert_eq!(finished.verdict.overall, Overall::FailMultiple);
        let statuses: Vec<Shown> = model.slot_views().into_iter().map(|v| v.shown).collect();
        assert_eq!(
            statuses,
            vec![Shown::Empty, shown(Suspect), Shown::Empty, shown(Suspect)]
        );
    }

    #[test]
    fn stopping_ends_the_run_inconclusively() {
        let _one_at_a_time = ENGINE.lock().unwrap_or_else(|e| e.into_inner());
        let mut model = Model::demo(Demo::Pass);
        model.length = Length::UntilStopped;
        model.start_with(small(&model));
        model.start_with(small(&model)); // a second start while running is ignored
        model.stop();
        let RunState::Running(run) = &model.run else {
            panic!("still running until polled")
        };
        assert!(run.stopping);
        run_to_end(&mut model);
        let finished = model.finished().unwrap();
        assert_eq!(finished.summary.end, RunEnd::Stopped);
        assert_eq!(finished.verdict.overall, Overall::Inconclusive);
        assert!(model.history.records().is_empty());
    }

    #[test]
    fn closing_while_idle_closes_at_once() {
        let mut model = Model::demo(Demo::Pass);
        assert_eq!(model.request_close(), CloseAction::Close);
        assert!(!model.ready_to_close(), "nothing was waited for");
    }

    #[test]
    fn closing_during_a_run_stops_it_and_waits() {
        let _one_at_a_time = ENGINE.lock().unwrap_or_else(|e| e.into_inner());
        let mut model = Model::demo(Demo::Pass);
        model.length = Length::UntilStopped;
        model.start_with(small(&model));
        assert_eq!(model.request_close(), CloseAction::WaitForStop);
        let RunState::Running(run) = &model.run else {
            panic!("still running until polled")
        };
        assert!(run.stopping, "the run was told to stop");
        assert!(!model.ready_to_close(), "must wait for the run");
        run_to_end(&mut model);
        assert!(model.ready_to_close());
    }

    #[test]
    fn a_second_run_replaces_the_first_result() {
        let _one_at_a_time = ENGINE.lock().unwrap_or_else(|e| e.into_inner());
        let mut model = Model::demo(Demo::SingleFail);
        model.start_with(small(&model));
        run_to_end(&mut model);
        assert_eq!(
            model.finished().unwrap().verdict.overall,
            Overall::FailSingle
        );
        model.ask_forget();
        model.forget_error = Some("old".to_string());
        // The second run has no simulated fault.
        model.start_with(TestConfig {
            simulate_fault: false,
            ..small(&model)
        });
        assert!(!model.confirm_forget);
        run_to_end(&mut model);
        let finished = model.finished().unwrap();
        assert_eq!(finished.verdict.overall, Overall::Pass);
        assert_eq!(finished.summary.error_count, 0);
        // The stick passed this time but stays failed, as the history does.
        assert_eq!(model.slot_views()[1].shown, shown(Failed));
        assert_eq!(model.history.records()[0].status, Failed);
    }

    #[test]
    fn the_panic_hook_can_stop_the_active_run() {
        let _one_at_a_time = ENGINE.lock().unwrap_or_else(|e| e.into_inner());
        let mut model = Model::demo(Demo::Pass);
        model.length = Length::UntilStopped;
        model.start_with(small(&model));
        stop_active_run();
        run_to_end(&mut model);
        assert_eq!(model.finished().unwrap().summary.end, RunEnd::Stopped);
        // With no run registered it does nothing.
        stop_active_run();
    }

    #[test]
    fn stop_and_wait_releases_a_run_and_keeps_its_result() {
        let _one_at_a_time = ENGINE.lock().unwrap_or_else(|e| e.into_inner());
        let mut model = Model::demo(Demo::Pass);
        model.length = Length::UntilStopped;
        model.start_with(small(&model));
        model.stop_and_wait();
        assert!(!model.is_running());
        assert!(model.finished().is_some());
        // Nothing to do when no run is going.
        model.stop_and_wait();
        assert!(model.finished().is_some());
    }
}
