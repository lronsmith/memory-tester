//! Remembers the last definitive result for each memory stick between runs.
//!
//! A stick is recognised by its serial number (plus part number and size), so
//! a person can test sticks one at a time, put them all back, and still see
//! which were good and which was bad.
//!
//! The results live in a small UTF-8 text file. The first line is
//! [`HEADER`]; every other line is one record with six tab-separated fields:
//!
//! ```text
//! serial  part_number  size_mb  status  when_unix  label
//! ```
//!
//! `status` is `passed` or `failed`, `size_mb` and `when_unix` are decimal
//! integers. In the text fields a backslash, tab, newline or carriage return
//! is written as `\\`, `\t`, `\n` or `\r`.

use std::ffi::OsString;
use std::fs::File;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::types::{Slot, Stick, StickStatus};
use crate::verdict::{Verdict, format_gb};

/// First line of the history file; names the format and its version.
const HEADER: &str = "memory-tester history v1";

/// File name used inside whichever folder holds the history.
const FILE_NAME: &str = "history.tsv";

/// Files larger than this are not history files written by this program, so
/// they are ignored rather than read into memory.
const MAX_FILE_BYTES: u64 = 4 * 1024 * 1024;

/// How many times `save` tries the final rename. On Windows, antivirus or the
/// search indexer can hold a freshly written file open for a moment, which
/// makes the rename fail; a few short retries ride that out.
const RENAME_ATTEMPTS: u32 = if cfg!(windows) { 5 } else { 1 };

/// Pause between rename attempts (4 pauses of 50 ms at most).
const RENAME_PAUSE: Duration = Duration::from_millis(50);

/// Identifies one physical stick across runs.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct StickKey {
    serial: String,
    part_number: String,
    size_mb: u64,
}

impl StickKey {
    /// `None` when the stick has no usable serial number, because then it
    /// cannot be told apart from its twins.
    pub fn from_stick(stick: &Stick) -> Option<StickKey> {
        let serial = stick.serial.as_deref()?.trim();
        if serial.is_empty() {
            return None;
        }
        Some(StickKey {
            serial: serial.to_string(),
            part_number: stick.part_number.trim().to_string(),
            size_mb: stick.size_mb,
        })
    }

    /// The stick's serial number, never empty.
    pub fn serial(&self) -> &str {
        &self.serial
    }
}

/// One remembered result.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Record {
    /// Which stick this is about.
    pub key: StickKey,
    /// Only `Passed` or `Failed` are ever stored.
    pub status: StickStatus,
    /// When it was recorded, seconds since the Unix epoch.
    pub when_unix: u64,
    /// Description for display when the stick is not installed right now,
    /// e.g. "Corsair CMK32GX4M2E3200C16, 16 GB".
    pub label: String,
}

/// The remembered results, together with the file they are saved to.
#[derive(Clone, Debug)]
pub struct History {
    /// At most one record per key, oldest first.
    records: Vec<Record>,
    path: PathBuf,
}

impl History {
    /// Loads from `path`. A missing or unreadable file gives an empty history;
    /// unparseable lines are skipped. Never fails.
    pub fn load(path: &Path) -> History {
        let mut history = History {
            records: Vec::new(),
            path: path.to_path_buf(),
        };
        if let Some(bytes) = read_small_file(path) {
            for record in parse(&bytes) {
                history.merge(record);
            }
            history.sort();
        }
        history
    }

    /// Loads from the default location (see [`default_path`]).
    pub fn load_default() -> History {
        History::load(&default_path())
    }

    /// The remembered result for `key`, if any.
    pub fn get(&self, key: &StickKey) -> Option<&Record> {
        self.records.iter().find(|r| &r.key == key)
    }

    /// All remembered results, oldest first.
    pub fn records(&self) -> &[Record] {
        &self.records
    }

    /// Records the outcome of a run for every installed stick that has a key
    /// (see [`rememberable_keys`]). Returns true if anything changed.
    ///
    /// Only `Passed` and `Failed` are remembered. A stored `Failed` is never
    /// replaced by a later `Passed`, because a faulty stick can pass a short
    /// test; only [`History::clear`] removes it. A result of the same status
    /// never moves the stored time backwards.
    pub fn apply(&mut self, slots: &[Slot], verdict: &Verdict, now_unix: u64) -> bool {
        let keys = rememberable_keys(slots);
        let mut changed = false;
        for ((slot, key), &status) in slots.iter().zip(keys).zip(&verdict.per_slot) {
            if !matches!(status, StickStatus::Passed | StickStatus::Failed) {
                continue;
            }
            let (Some(stick), Some(key)) = (&slot.stick, key) else {
                continue;
            };
            changed |= self.merge(Record {
                key,
                status,
                when_unix: now_unix,
                label: label_for(stick),
            });
        }
        if changed {
            self.sort();
        }
        changed
    }

    /// Writes the file. Creates the folder if needed.
    ///
    /// The data goes to a temporary file in the same folder first, which is
    /// then renamed over the real file, so a crash mid-write cannot leave a
    /// half-written history behind.
    pub fn save(&self) -> io::Result<()> {
        let file_name = self.path.file_name().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "history path has no file name")
        })?;
        let folder = self.path.parent().unwrap_or(Path::new(""));
        if !folder.as_os_str().is_empty() {
            std::fs::create_dir_all(folder)?;
        }
        // The process id keeps two copies of the program from sharing a temp file.
        let mut temp_name = OsString::from(".");
        temp_name.push(file_name);
        temp_name.push(format!(".{}.tmp", std::process::id()));
        let temp = folder.join(temp_name);

        let result = write_and_sync(&temp, self.to_text().as_bytes()).and_then(|()| {
            with_retries(RENAME_ATTEMPTS, RENAME_PAUSE, || {
                std::fs::rename(&temp, &self.path)
            })
        });
        if result.is_err() {
            let _ = std::fs::remove_file(&temp);
        }
        result
    }

    /// Forgets everything (in memory; call `save` to persist).
    pub fn clear(&mut self) {
        self.records.clear();
    }

    /// Folds one result into the records following the sticky-failed rule.
    /// Returns true if anything changed.
    fn merge(&mut self, mut new: Record) -> bool {
        let Some(old) = self.records.iter_mut().find(|r| r.key == new.key) else {
            self.records.push(new);
            return true;
        };
        if old.status == StickStatus::Failed && new.status == StickStatus::Passed {
            return false;
        }
        if old.status == new.status {
            new.when_unix = new.when_unix.max(old.when_unix);
        }
        if *old == new {
            return false;
        }
        *old = new;
        true
    }

    /// Restores oldest-first order; ties keep their existing order.
    fn sort(&mut self) {
        self.records.sort_by_key(|r| r.when_unix);
    }

    fn to_text(&self) -> String {
        let mut text = String::from(HEADER);
        text.push('\n');
        for r in &self.records {
            let status = match r.status {
                StickStatus::Failed => "failed",
                _ => "passed",
            };
            text.push_str(&format!(
                "{}\t{}\t{}\t{}\t{}\t{}\n",
                escape(&r.key.serial),
                escape(&r.key.part_number),
                r.key.size_mb,
                status,
                r.when_unix,
                escape(&r.label),
            ));
        }
        text
    }
}

/// Builds the display label, e.g. "Corsair CMK32GX4M2E3200C16, 16 GB".
fn label_for(stick: &Stick) -> String {
    let name: Vec<&str> = [stick.manufacturer.trim(), stick.part_number.trim()]
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect();
    let name = if name.is_empty() {
        "Memory stick".to_string()
    } else {
        name.join(" ")
    };
    let size = format_gb(stick.size_mb.saturating_mul(1024 * 1024));
    format!("{name}, {size}")
}

/// Reads the bytes of `path`, or `None` if it is missing, unreadable or too
/// large.
fn read_small_file(path: &Path) -> Option<Vec<u8>> {
    let file = File::open(path).ok()?;
    let mut bytes = Vec::new();
    file.take(MAX_FILE_BYTES + 1).read_to_end(&mut bytes).ok()?;
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return None;
    }
    Some(bytes)
}

/// Parses the file contents. An unknown header gives nothing; bad lines,
/// including lines that are not valid UTF-8, are skipped one by one.
fn parse(bytes: &[u8]) -> Vec<Record> {
    let bytes = bytes.strip_prefix("\u{FEFF}".as_bytes()).unwrap_or(bytes);
    let mut lines = bytes
        .split(|&b| b == b'\n')
        .map(|l| l.strip_suffix(b"\r").unwrap_or(l));
    if lines.next() != Some(HEADER.as_bytes()) {
        return Vec::new();
    }
    lines
        .filter_map(|line| std::str::from_utf8(line).ok())
        .filter_map(parse_line)
        .collect()
}

fn parse_line(line: &str) -> Option<Record> {
    let fields: Vec<&str> = line.split('\t').collect();
    let [serial, part_number, size_mb, status, when_unix, label] = fields[..] else {
        return None;
    };
    let serial = unescape(serial)?;
    if serial.trim().is_empty() {
        return None;
    }
    let status = match status {
        "passed" => StickStatus::Passed,
        "failed" => StickStatus::Failed,
        _ => return None,
    };
    Some(Record {
        key: StickKey {
            serial,
            part_number: unescape(part_number)?,
            size_mb: size_mb.parse().ok()?,
        },
        status,
        when_unix: when_unix.parse().ok()?,
        label: unescape(label)?,
    })
}

fn escape(field: &str) -> String {
    let mut out = String::with_capacity(field.len());
    for c in field.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            _ => out.push(c),
        }
    }
    out
}

/// Reverses [`escape`]; `None` for an unknown or unfinished escape.
fn unescape(field: &str) -> Option<String> {
    let mut out = String::with_capacity(field.len());
    let mut chars = field.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        out.push(match chars.next()? {
            '\\' => '\\',
            't' => '\t',
            'n' => '\n',
            'r' => '\r',
            _ => return None,
        });
    }
    Some(out)
}

/// The key under which each slot's result can be remembered, one entry per
/// slot in the same order.
///
/// An entry is `None` for an empty slot, for a stick without a usable serial,
/// and for every stick whose key is shared with another installed stick: a
/// result stored under a shared key would be shown for both sticks.
pub fn rememberable_keys(slots: &[Slot]) -> Vec<Option<StickKey>> {
    let keys: Vec<Option<StickKey>> = slots
        .iter()
        .map(|s| s.stick.as_ref().and_then(StickKey::from_stick))
        .collect();
    keys.iter()
        .map(|key| {
            let key = key.as_ref()?;
            let uses = keys.iter().filter(|k| k.as_ref() == Some(key)).count();
            (uses == 1).then(|| key.clone())
        })
        .collect()
}

/// Runs `op` up to `attempts` times (at least once), pausing between tries,
/// and returns the first success or the last error.
fn with_retries<T>(
    attempts: u32,
    pause: Duration,
    mut op: impl FnMut() -> io::Result<T>,
) -> io::Result<T> {
    let mut tries_left = attempts.max(1);
    loop {
        tries_left -= 1;
        match op() {
            Ok(value) => return Ok(value),
            Err(err) if tries_left == 0 => return Err(err),
            Err(_) => std::thread::sleep(pause),
        }
    }
}

fn write_and_sync(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut file = File::create(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

/// Where the history file lives by default.
///
/// On Windows this is `%LOCALAPPDATA%\memory-tester\history.tsv`. Elsewhere it
/// is `$XDG_DATA_HOME/memory-tester/history.tsv`, else
/// `$HOME/.local/share/memory-tester/history.tsv`. When none of those
/// variables is set (an empty value counts as unset), it is `history.tsv` next
/// to the executable, or in the current directory if that is unknown.
pub fn default_path() -> PathBuf {
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf));
    path_from(cfg!(windows), |name| std::env::var_os(name), exe_dir)
}

/// The logic of [`default_path`], with the platform, environment lookup and
/// executable folder passed in.
fn path_from(
    windows: bool,
    var: impl Fn(&str) -> Option<OsString>,
    exe_dir: Option<PathBuf>,
) -> PathBuf {
    let var = |name: &str| var(name).filter(|v| !v.is_empty()).map(PathBuf::from);
    let data_dir = if windows {
        var("LOCALAPPDATA")
    } else {
        var("XDG_DATA_HOME").or_else(|| var("HOME").map(|home| home.join(".local").join("share")))
    };
    match data_dir {
        Some(dir) => dir.join("memory-tester").join(FILE_NAME),
        None => exe_dir.unwrap_or_default().join(FILE_NAME),
    }
}

/// Formats seconds since the Unix epoch as a UTC calendar date "YYYY-MM-DD".
pub fn format_date(unix_secs: u64) -> String {
    // Howard Hinnant's civil-from-days algorithm, on days since 1970-01-01.
    // Shifting to 0000-03-01 puts the leap day at the end of each year, and
    // 400-year eras repeat the Gregorian calendar exactly.
    let days = (unix_secs / 86_400) as i64 + 719_468;
    let era = days.div_euclid(146_097);
    let day_of_era = days.rem_euclid(146_097); // 0..=146096
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365; // 0..=399
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100); // 0..=365
    let month_index = (5 * day_of_year + 2) / 153; // 0 = March .. 11 = February
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::verdict::Overall;
    use std::collections::HashMap;
    use std::ffi::OsString;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::Duration;

    use StickStatus::{Failed, Passed, Suspect, Untested};

    /// A fresh, empty directory that is deleted when dropped.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> TempDir {
            static NEXT: AtomicU32 = AtomicU32::new(0);
            let n = NEXT.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!(
                "memory-tester-history-test-{}-{n}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            TempDir(dir)
        }

        fn file(&self) -> PathBuf {
            self.0.join("history.tsv")
        }

        fn entries(&self) -> Vec<String> {
            let mut names: Vec<String> = std::fs::read_dir(&self.0)
                .unwrap()
                .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .collect();
            names.sort();
            names
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn stick(serial: Option<&str>) -> Stick {
        Stick {
            size_mb: 16384,
            manufacturer: "Corsair".to_string(),
            part_number: "CMK32GX4M2E3200C16".to_string(),
            serial: serial.map(str::to_string),
            speed_mts: Some(3200),
            memory_type: "DDR4".to_string(),
        }
    }

    fn slot(stick: Option<Stick>) -> Slot {
        Slot {
            locator: "DIMM_A1".to_string(),
            bank: String::new(),
            stick,
        }
    }

    fn verdict(per_slot: Vec<StickStatus>) -> Verdict {
        Verdict {
            overall: Overall::Pass,
            per_slot,
            headline: String::new(),
            advice: String::new(),
        }
    }

    fn key(serial: &str) -> StickKey {
        StickKey::from_stick(&stick(Some(serial))).unwrap()
    }

    fn empty_at(path: &Path) -> History {
        History::load(path)
    }

    #[test]
    fn key_needs_a_serial() {
        assert_eq!(StickKey::from_stick(&stick(None)), None);
        assert_eq!(StickKey::from_stick(&stick(Some(""))), None);
        assert_eq!(StickKey::from_stick(&stick(Some("   "))), None);
    }

    #[test]
    fn keys_differ_by_serial_part_and_size() {
        let a = key("1234ABCD");
        let b = key("1234ABCE");
        assert_ne!(a, b);
        // Surrounding whitespace is not part of the serial.
        assert_eq!(a, StickKey::from_stick(&stick(Some(" 1234ABCD "))).unwrap());

        let mut other_part = stick(Some("1234ABCD"));
        other_part.part_number = "F4-3200C16-16GVK".to_string();
        assert_ne!(a, StickKey::from_stick(&other_part).unwrap());

        let mut other_size = stick(Some("1234ABCD"));
        other_size.size_mb = 8192;
        assert_ne!(a, StickKey::from_stick(&other_size).unwrap());
    }

    #[test]
    fn apply_records_label_and_time() {
        let dir = TempDir::new();
        let mut h = empty_at(&dir.file());
        let slots = vec![slot(Some(stick(Some("S1"))))];
        assert!(h.apply(&slots, &verdict(vec![Passed]), 1000));
        let r = h.get(&key("S1")).unwrap();
        assert_eq!(r.status, Passed);
        assert_eq!(r.when_unix, 1000);
        assert_eq!(r.label, "Corsair CMK32GX4M2E3200C16, 16 GB");
    }

    #[test]
    fn label_without_maker_or_part() {
        let dir = TempDir::new();
        let mut h = empty_at(&dir.file());
        let mut s = stick(Some("S1"));
        s.manufacturer.clear();
        s.part_number.clear();
        s.size_mb = 8192;
        let k = StickKey::from_stick(&s).unwrap();
        h.apply(&[slot(Some(s))], &verdict(vec![Failed]), 5);
        assert_eq!(h.get(&k).unwrap().label, "Memory stick, 8 GB");
    }

    /// Applies `first` at t=100 then `second` at t=200 to one stick and
    /// returns (changed by second, stored status, stored time).
    fn two_runs(first: StickStatus, second: StickStatus) -> (bool, StickStatus, u64) {
        let dir = TempDir::new();
        let mut h = empty_at(&dir.file());
        let slots = vec![slot(Some(stick(Some("S1"))))];
        assert!(h.apply(&slots, &verdict(vec![first]), 100));
        let changed = h.apply(&slots, &verdict(vec![second]), 200);
        assert_eq!(h.records().len(), 1);
        let r = h.get(&key("S1")).unwrap();
        (changed, r.status, r.when_unix)
    }

    #[test]
    fn pass_then_fail_becomes_failed() {
        assert_eq!(two_runs(Passed, Failed), (true, Failed, 200));
    }

    #[test]
    fn fail_then_pass_stays_failed() {
        assert_eq!(two_runs(Failed, Passed), (false, Failed, 100));
    }

    #[test]
    fn pass_then_pass_updates_time() {
        assert_eq!(two_runs(Passed, Passed), (true, Passed, 200));
    }

    #[test]
    fn fail_then_fail_updates_time() {
        assert_eq!(two_runs(Failed, Failed), (true, Failed, 200));
    }

    #[test]
    fn same_result_at_same_time_is_no_change() {
        let dir = TempDir::new();
        let mut h = empty_at(&dir.file());
        let slots = vec![slot(Some(stick(Some("S1"))))];
        assert!(h.apply(&slots, &verdict(vec![Passed]), 100));
        assert!(!h.apply(&slots, &verdict(vec![Passed]), 100));
    }

    #[test]
    fn apply_ignores_inconclusive_statuses_missing_serials_and_empty_slots() {
        let dir = TempDir::new();
        let mut h = empty_at(&dir.file());
        let slots = vec![
            slot(Some(stick(Some("SUSPECT")))),
            slot(Some(stick(Some("UNTESTED")))),
            slot(Some(stick(None))),
            slot(None),
        ];
        let v = verdict(vec![Suspect, Untested, Failed, Failed]);
        assert!(!h.apply(&slots, &v, 100));
        assert!(h.records().is_empty());

        // A Suspect result does not disturb an earlier definitive one.
        let one = vec![slot(Some(stick(Some("S1"))))];
        assert!(h.apply(&one, &verdict(vec![Passed]), 100));
        assert!(!h.apply(&one, &verdict(vec![Suspect]), 200));
        assert_eq!(h.get(&key("S1")).unwrap().when_unix, 100);
    }

    #[test]
    fn apply_tolerates_mismatched_lengths() {
        let dir = TempDir::new();
        let mut h = empty_at(&dir.file());
        let slots = vec![slot(Some(stick(Some("A")))), slot(Some(stick(Some("B"))))];
        assert!(h.apply(&slots, &verdict(vec![Passed]), 1));
        assert!(h.get(&key("A")).is_some());
        assert!(h.get(&key("B")).is_none());
    }

    #[test]
    fn records_are_oldest_first() {
        let dir = TempDir::new();
        let mut h = empty_at(&dir.file());
        let a = vec![slot(Some(stick(Some("A"))))];
        let b = vec![slot(Some(stick(Some("B"))))];
        h.apply(&a, &verdict(vec![Passed]), 100);
        h.apply(&b, &verdict(vec![Passed]), 200);
        h.apply(&a, &verdict(vec![Passed]), 300);
        let order: Vec<u64> = h.records().iter().map(|r| r.when_unix).collect();
        assert_eq!(order, vec![200, 300]);
        assert_eq!(h.records()[0].key, key("B"));
    }

    #[test]
    fn clear_forgets_everything() {
        let dir = TempDir::new();
        let mut h = empty_at(&dir.file());
        h.apply(&[slot(Some(stick(Some("A"))))], &verdict(vec![Failed]), 1);
        h.clear();
        assert!(h.records().is_empty());
        h.save().unwrap();
        assert!(History::load(&dir.file()).records().is_empty());
    }

    #[test]
    fn save_then_load_round_trips_awkward_text() {
        let dir = TempDir::new();
        let mut h = empty_at(&dir.file());
        let mut odd = stick(Some("tab\there\\back\\slash"));
        odd.part_number = "line\nbreak\r\nand \\t not a tab".to_string();
        odd.manufacturer = "Müller 内存 \u{1F600}".to_string();
        let plain = stick(Some("PLAIN"));
        let slots = vec![slot(Some(odd)), slot(Some(plain))];
        h.apply(&slots, &verdict(vec![Failed, Passed]), 1_795_392_000);
        h.save().unwrap();

        let loaded = History::load(&dir.file());
        assert_eq!(loaded.records(), h.records());
        assert_eq!(loaded.records().len(), 2);
        assert!(
            loaded.records()[0]
                .label
                .starts_with("Müller 内存 \u{1F600} line\nbreak")
        );
    }

    #[test]
    fn save_creates_folders_and_leaves_no_temp_file() {
        let dir = TempDir::new();
        let path = dir.0.join("a").join("b").join("history.tsv");
        let mut h = empty_at(&path);
        h.apply(&[slot(Some(stick(Some("A"))))], &verdict(vec![Passed]), 7);
        h.save().unwrap();
        // Saving again replaces the existing file.
        h.apply(&[slot(Some(stick(Some("A"))))], &verdict(vec![Passed]), 8);
        h.save().unwrap();
        let names: Vec<String> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["history.tsv".to_string()]);
        assert_eq!(History::load(&path).records()[0].when_unix, 8);
    }

    #[test]
    fn failed_save_leaves_no_temp_file() {
        let dir = TempDir::new();
        // The target is a folder, so the final rename cannot replace it.
        std::fs::create_dir(dir.file()).unwrap();
        let mut h = empty_at(&dir.file());
        h.apply(&[slot(Some(stick(Some("A"))))], &verdict(vec![Passed]), 7);
        assert!(h.save().is_err());
        assert_eq!(dir.entries(), vec!["history.tsv".to_string()]);
    }

    #[test]
    fn file_format_is_as_documented() {
        let dir = TempDir::new();
        let mut h = empty_at(&dir.file());
        h.apply(
            &[slot(Some(stick(Some("A\tB"))))],
            &verdict(vec![Failed]),
            42,
        );
        h.save().unwrap();
        let text = std::fs::read_to_string(dir.file()).unwrap();
        assert_eq!(
            text,
            "memory-tester history v1\n\
             A\\tB\tCMK32GX4M2E3200C16\t16384\tfailed\t42\tCorsair CMK32GX4M2E3200C16, 16 GB\n"
        );
    }

    #[test]
    fn load_missing_or_empty_file_is_empty() {
        let dir = TempDir::new();
        let h = History::load(&dir.file());
        assert!(h.records().is_empty());

        std::fs::write(dir.file(), "").unwrap();
        assert!(History::load(&dir.file()).records().is_empty());

        // A folder where the file should be is unreadable, not a crash.
        let folder = dir.0.join("folder");
        std::fs::create_dir(&folder).unwrap();
        assert!(History::load(&folder).records().is_empty());
    }

    const GOOD: &str = "S1\tP1\t8192\tpassed\t100\tLabel one";

    #[test]
    fn load_rejects_wrong_or_missing_header() {
        let dir = TempDir::new();
        for text in [
            format!("{GOOD}\n"),
            format!("memory-tester history v2\n{GOOD}\n"),
            format!("something else\n{GOOD}\n"),
            format!("\u{FEFF}memory-tester history v2\n{GOOD}\n"),
        ] {
            std::fs::write(dir.file(), text).unwrap();
            assert!(History::load(&dir.file()).records().is_empty());
        }
        // A byte-order mark added by a text editor is tolerated.
        std::fs::write(dir.file(), format!("\u{FEFF}{HEADER}\n{GOOD}\n")).unwrap();
        assert_eq!(History::load(&dir.file()).records().len(), 1);
    }

    #[test]
    fn load_skips_garbage_lines_and_keeps_good_ones() {
        let dir = TempDir::new();
        let text = format!(
            "memory-tester history v1\r\n\
             {GOOD}\r\n\
             \r\n\
             too\tfew\tfields\n\
             S2\tP\t8192\tpassed\t100\tLabel\textra\n\
             S3\tP\tbig\tpassed\t100\tLabel\n\
             S4\tP\t8192\tpassed\t-5\tLabel\n\
             S5\tP\t8192\tsuspect\t100\tLabel\n\
             S6\tP\t8192\tPASSED\t100\tLabel\n\
             S7\\x\tP\t8192\tpassed\t100\tbad escape\n\
             S8\tP\t8192\tpassed\t100\ttrailing backslash\\\n\
             \tP\t8192\tpassed\t100\tno serial\n\
             S9\tP\t99999999999999999999999\tpassed\t100\tLabel\n\
             S10\tP2\t4096\tfailed\t200\tLabel two\n"
        );
        std::fs::write(dir.file(), text).unwrap();
        let h = History::load(&dir.file());
        let serials: Vec<&str> = h.records().iter().map(|r| r.key.serial.as_str()).collect();
        assert_eq!(serials, vec!["S1", "S10"]);
        assert_eq!(h.records()[1].status, Failed);
        assert_eq!(h.records()[1].key.size_mb, 4096);
    }

    #[test]
    fn load_skips_only_the_line_with_bad_bytes() {
        let dir = TempDir::new();
        let mut bytes = b"memory-tester history v1\n".to_vec();
        bytes.extend_from_slice(b"S1\tP\t8192\tfailed\t1\tgood\n");
        bytes.extend_from_slice(b"S2\tP\t8192\tpassed\t2\tcaf\xe9\n");
        bytes.extend_from_slice("S3\tP\t8192\tpassed\t3\tMüller\n".as_bytes());
        std::fs::write(dir.file(), bytes).unwrap();
        let h = History::load(&dir.file());
        let serials: Vec<&str> = h.records().iter().map(|r| r.key.serial()).collect();
        assert_eq!(serials, vec!["S1", "S3"]);
        assert_eq!(h.records()[1].label, "Müller");

        // A damaged header still means the file is not ours.
        std::fs::write(
            dir.file(),
            b"memory-tester history v1\xff\nS1\tP\t1\tpassed\t1\tL\n",
        )
        .unwrap();
        assert!(History::load(&dir.file()).records().is_empty());
    }

    #[test]
    fn same_status_never_moves_time_backwards() {
        let dir = TempDir::new();
        let mut h = empty_at(&dir.file());
        let slots = vec![slot(Some(stick(Some("S1"))))];
        assert!(h.apply(&slots, &verdict(vec![Passed]), 2000));
        assert!(!h.apply(&slots, &verdict(vec![Passed]), 1000));
        assert_eq!(h.get(&key("S1")).unwrap().when_unix, 2000);

        // A failure always replaces a pass, whatever the clock says.
        assert!(h.apply(&slots, &verdict(vec![Failed]), 1500));
        let r = h.get(&key("S1")).unwrap();
        assert_eq!((r.status, r.when_unix), (Failed, 1500));
        assert!(!h.apply(&slots, &verdict(vec![Failed]), 1200));
        assert_eq!(h.get(&key("S1")).unwrap().when_unix, 1500);
    }

    #[test]
    fn apply_skips_sticks_that_share_a_key() {
        let dir = TempDir::new();
        let mut h = empty_at(&dir.file());
        let slots = vec![
            slot(Some(stick(Some("12345678")))),
            slot(Some(stick(Some("12345678")))),
            slot(Some(stick(Some("UNIQUE")))),
        ];
        assert!(h.apply(&slots, &verdict(vec![Failed, Failed, Failed]), 10));
        assert!(h.get(&key("12345678")).is_none());
        assert!(h.get(&key("UNIQUE")).is_some());

        let keys = rememberable_keys(&slots);
        assert_eq!(keys, vec![None, None, Some(key("UNIQUE"))]);
        // An empty slot never has a key, and a lone stick keeps its key.
        assert_eq!(
            rememberable_keys(&[slot(None), slot(Some(stick(Some("A"))))]),
            vec![None, Some(key("A"))]
        );
    }

    #[test]
    fn save_never_writes_the_target_directly() {
        let dir = TempDir::new();
        let mut h = empty_at(&dir.file());
        h.apply(&[slot(Some(stick(Some("A"))))], &verdict(vec![Passed]), 7);
        h.save().unwrap();
        let before = std::fs::read(dir.file()).unwrap();

        // Block the temporary file; a save that wrote straight to the target
        // would succeed and change it.
        let temp = dir
            .0
            .join(format!(".history.tsv.{}.tmp", std::process::id()));
        std::fs::create_dir(&temp).unwrap();
        h.apply(&[slot(Some(stick(Some("B"))))], &verdict(vec![Failed]), 8);
        assert!(h.save().is_err());
        assert_eq!(std::fs::read(dir.file()).unwrap(), before);
    }

    #[test]
    fn retries_stop_at_first_success_or_after_the_last_attempt() {
        let mut calls = 0;
        let result = with_retries(5, Duration::ZERO, || {
            calls += 1;
            if calls < 3 {
                Err(io::Error::other("locked"))
            } else {
                Ok(calls)
            }
        });
        assert_eq!(result.unwrap(), 3);

        let mut calls = 0;
        let result: io::Result<()> = with_retries(4, Duration::ZERO, || {
            calls += 1;
            Err(io::Error::other("locked"))
        });
        assert!(result.is_err());
        assert_eq!(calls, 4);
    }

    #[test]
    fn load_applies_sticky_failed_to_duplicate_lines() {
        let dir = TempDir::new();
        let text = "memory-tester history v1\n\
                    S1\tP\t8192\tfailed\t100\tL\n\
                    S1\tP\t8192\tpassed\t200\tL\n";
        std::fs::write(dir.file(), text).unwrap();
        let h = History::load(&dir.file());
        assert_eq!(h.records().len(), 1);
        assert_eq!(h.records()[0].status, Failed);
        assert_eq!(h.records()[0].when_unix, 100);
    }

    #[test]
    fn format_date_known_days() {
        assert_eq!(format_date(0), "1970-01-01");
        assert_eq!(format_date(86_399), "1970-01-01");
        assert_eq!(format_date(86_400), "1970-01-02");
        assert_eq!(format_date(951_782_400), "2000-02-29");
        assert_eq!(format_date(951_868_800), "2000-03-01");
        assert_eq!(format_date(1_709_164_800), "2024-02-29");
        assert_eq!(format_date(4_107_542_400), "2100-03-01");
        assert_eq!(format_date(4_107_456_000), "2100-02-28");
        assert_eq!(format_date(1_795_392_000), "2026-11-23");
        assert_eq!(format_date(1_798_761_599), "2026-12-31");
        // Far future must not panic.
        let _ = format_date(u64::MAX);
    }

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<OsString> {
        let map: HashMap<String, OsString> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), OsString::from(v)))
            .collect();
        move |name| map.get(name).cloned()
    }

    #[test]
    fn default_path_follows_environment() {
        let exe = Some(PathBuf::from("/opt/app"));
        let tail = |base: &str| {
            PathBuf::from(base)
                .join("memory-tester")
                .join("history.tsv")
        };

        // Windows: LOCALAPPDATA, ignoring the Unix variables.
        let all = env(&[
            ("LOCALAPPDATA", r"C:\Users\Shea\AppData\Local"),
            ("XDG_DATA_HOME", "/xdg"),
            ("HOME", "/home/shea"),
        ]);
        assert_eq!(
            path_from(true, &all, exe.clone()),
            tail(r"C:\Users\Shea\AppData\Local")
        );
        assert_eq!(
            path_from(true, env(&[("HOME", "/home/shea")]), exe.clone()),
            PathBuf::from("/opt/app").join("history.tsv")
        );

        // Elsewhere: XDG_DATA_HOME, then HOME, ignoring LOCALAPPDATA.
        assert_eq!(path_from(false, &all, exe.clone()), tail("/xdg"));
        assert_eq!(
            path_from(false, env(&[("HOME", "/home/shea")]), exe.clone()),
            PathBuf::from("/home/shea/.local/share/memory-tester/history.tsv")
        );
        // An empty variable counts as unset.
        assert_eq!(
            path_from(
                false,
                env(&[("XDG_DATA_HOME", ""), ("HOME", "/home/shea")]),
                exe.clone()
            ),
            PathBuf::from("/home/shea/.local/share/memory-tester/history.tsv")
        );

        // Nothing set: next to the executable, else the current directory.
        let none = env(&[("LOCALAPPDATA", r"C:\x")]);
        assert_eq!(
            path_from(false, &none, exe.clone()),
            PathBuf::from("/opt/app/history.tsv")
        );
        assert_eq!(path_from(false, &none, None), PathBuf::from("history.tsv"));
        assert_eq!(
            path_from(true, env(&[]), None),
            PathBuf::from("history.tsv")
        );
    }

    #[test]
    fn default_path_ends_in_history_file() {
        assert_eq!(default_path().file_name().unwrap(), "history.tsv");
    }
}
