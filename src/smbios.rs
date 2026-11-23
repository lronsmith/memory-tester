//! Finds out which memory slots the motherboard has and which stick is in
//! each, by reading the firmware's SMBIOS table (no administrator rights
//! needed on Windows).

use crate::types::{Slot, Stick};
use std::fmt;

/// Why stick detection failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DetectError {
    /// Stick detection is only implemented on Windows.
    Unsupported,
    /// Windows refused to hand over the SMBIOS table. Holds the Windows error
    /// code from `GetLastError` (0 when Windows gave none).
    TableUnavailable(u32),
    /// The table Windows returned was too short or its header was
    /// inconsistent with its actual size.
    Malformed,
}

impl fmt::Display for DetectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DetectError::Unsupported => {
                write!(f, "memory stick detection is only supported on Windows")
            }
            DetectError::TableUnavailable(code) => write!(
                f,
                "the firmware memory table could not be read (Windows error {code})"
            ),
            DetectError::Malformed => write!(f, "the firmware memory table is malformed"),
        }
    }
}

impl std::error::Error for DetectError {}

/// Reads the firmware memory table and lists every memory slot, in the order
/// the firmware reports them.
///
/// On Windows this asks for the raw SMBIOS table with
/// `GetSystemFirmwareTable`, which works without administrator rights. On
/// other platforms it returns [`DetectError::Unsupported`].
pub fn detect_slots() -> Result<Vec<Slot>, DetectError> {
    #[cfg(windows)]
    {
        let raw = windows::read_raw_smbios()?;
        Ok(parse_slots(table_from_raw(&raw)?))
    }
    #[cfg(not(windows))]
    {
        Err(DetectError::Unsupported)
    }
}

/// Size of the Windows `RawSMBIOSData` header that precedes the table:
/// four one-byte version fields, then the table length as a little-endian u32.
#[cfg(any(windows, test))]
const RAW_HEADER_LEN: usize = 8;

/// Strips the Windows `RawSMBIOSData` header from `raw` and returns the
/// structure table it describes. Fails if `raw` is shorter than the header or
/// the header claims more table bytes than `raw` holds.
#[cfg(any(windows, test))]
fn table_from_raw(raw: &[u8]) -> Result<&[u8], DetectError> {
    let len_bytes: [u8; 4] = raw
        .get(4..RAW_HEADER_LEN)
        .and_then(|b| b.try_into().ok())
        .ok_or(DetectError::Malformed)?;
    let len = usize::try_from(u32::from_le_bytes(len_bytes)).map_err(|_| DetectError::Malformed)?;
    let end = RAW_HEADER_LEN
        .checked_add(len)
        .ok_or(DetectError::Malformed)?;
    raw.get(RAW_HEADER_LEN..end).ok_or(DetectError::Malformed)
}

#[cfg(windows)]
mod windows {
    use super::DetectError;
    use windows_sys::Win32::Foundation::GetLastError;
    use windows_sys::Win32::System::SystemInformation::{GetSystemFirmwareTable, RSMB};

    /// How many times to retry if the table grows between asking for its size
    /// and fetching it.
    const ATTEMPTS: usize = 3;

    /// Fetches the raw SMBIOS table, `RawSMBIOSData` header included.
    pub(super) fn read_raw_smbios() -> Result<Vec<u8>, DetectError> {
        for _ in 0..ATTEMPTS {
            // SAFETY: a null buffer with size 0 is the documented way to ask
            // for the required size; nothing is written.
            let needed = unsafe { GetSystemFirmwareTable(RSMB, 0, std::ptr::null_mut(), 0) };
            if needed == 0 {
                return Err(last_error());
            }
            let mut buf = vec![0u8; needed as usize];
            // SAFETY: `buf` is a live, writable allocation of exactly
            // `needed` bytes, and that is the size passed, so Windows writes
            // at most `needed` bytes into it.
            let written = unsafe { GetSystemFirmwareTable(RSMB, 0, buf.as_mut_ptr(), needed) };
            if written == 0 {
                return Err(last_error());
            }
            if written <= needed {
                buf.truncate(written as usize);
                return Ok(buf);
            }
            // The return value is the size now required and the buffer was
            // not filled: the table grew. Ask again.
        }
        Err(DetectError::TableUnavailable(0))
    }

    fn last_error() -> DetectError {
        // SAFETY: GetLastError has no preconditions; it reads thread-local state.
        DetectError::TableUnavailable(unsafe { GetLastError() })
    }
}

/// Structure type of a Memory Device (one slot, populated or not).
const TYPE_MEMORY_DEVICE: u8 = 17;
/// Structure type that marks the end of the table.
const TYPE_END_OF_TABLE: u8 = 127;

// Offsets of the Memory Device fields, from the start of the structure.
const OFF_SIZE: usize = 0x0C;
const OFF_DEVICE_LOCATOR: usize = 0x10;
const OFF_BANK_LOCATOR: usize = 0x11;
const OFF_MEMORY_TYPE: usize = 0x12;
const OFF_SPEED: usize = 0x15;
const OFF_MANUFACTURER: usize = 0x17;
const OFF_SERIAL: usize = 0x18;
const OFF_PART_NUMBER: usize = 0x1A;
const OFF_EXTENDED_SIZE: usize = 0x1C;
const OFF_CONFIGURED_SPEED: usize = 0x20;

/// Parses a raw SMBIOS structure table (the bytes after the 8-byte Windows
/// `RawSMBIOSData` header) into slots. Pure, never panics.
///
/// Every Memory Device (type 17) structure that describes RAM becomes one
/// [`Slot`], in table order; non-volatile devices such as the BIOS flash chip
/// are left out. Parsing stops at the end-of-table structure (type 127), at
/// the end of the input, or at the first malformed structure, returning the
/// slots found before it. A structure whose string set is cut off by the end
/// of the input is dropped rather than read with missing strings.
pub fn parse_slots(table: &[u8]) -> Vec<Slot> {
    let mut slots = Vec::new();
    let mut pos = 0;
    while let Some((structure, next)) = next_structure(table, pos) {
        match structure.byte(0) {
            Some(TYPE_END_OF_TABLE) => break,
            Some(TYPE_MEMORY_DEVICE) => slots.extend(parse_memory_device(&structure)),
            _ => {}
        }
        pos = next;
    }
    slots
}

/// One SMBIOS structure split into its two parts.
struct Structure<'a> {
    /// The formatted area, header included (at least 4 bytes).
    formatted: &'a [u8],
    /// The string set without its final terminating NUL: strings separated
    /// by single NULs. Empty when the structure has no strings.
    strings: &'a [u8],
}

impl Structure<'_> {
    fn byte(&self, offset: usize) -> Option<u8> {
        self.formatted.get(offset).copied()
    }

    fn word(&self, offset: usize) -> Option<u16> {
        let b = self.formatted.get(offset..offset.checked_add(2)?)?;
        Some(u16::from_le_bytes([b[0], b[1]]))
    }

    fn dword(&self, offset: usize) -> Option<u32> {
        let b = self.formatted.get(offset..offset.checked_add(4)?)?;
        Some(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    /// The cleaned-up string referenced by the string-index byte at
    /// `offset`, or an empty string if the field, the index or the string
    /// does not exist.
    fn string(&self, offset: usize) -> String {
        match self.byte(offset) {
            Some(index @ 1..) if !self.strings.is_empty() => self
                .strings
                .split(|&b| b == 0)
                .nth(usize::from(index) - 1)
                .map(clean)
                .unwrap_or_default(),
            _ => String::new(),
        }
    }
}

/// Splits off the structure starting at `pos`. Returns it and the position
/// of the following structure, or `None` if no complete structure starts
/// there.
fn next_structure(table: &[u8], pos: usize) -> Option<(Structure<'_>, usize)> {
    let len = usize::from(*table.get(pos.checked_add(1)?)?);
    if len < 4 {
        return None;
    }
    let strings_start = pos.checked_add(len)?;
    let formatted = table.get(pos..strings_start)?;
    // The string set ends at the first double NUL. With no strings it is
    // just that double NUL.
    let rest = table.get(strings_start..)?;
    let terminator = rest.windows(2).position(|w| w == [0, 0])?;
    let strings = &rest[..terminator];
    Some((
        Structure { formatted, strings },
        strings_start + terminator + 2,
    ))
}

/// Builds a slot from a Memory Device structure, or `None` if the device is
/// not RAM.
fn parse_memory_device(s: &Structure<'_>) -> Option<Slot> {
    let memory_type = s.byte(OFF_MEMORY_TYPE);
    if memory_type.is_some_and(is_non_volatile) {
        return None;
    }
    let stick = size_mb(s).map(|size_mb| Stick {
        size_mb,
        manufacturer: usable(s.string(OFF_MANUFACTURER), is_placeholder),
        part_number: usable(s.string(OFF_PART_NUMBER), is_placeholder),
        serial: Some(usable(s.string(OFF_SERIAL), is_placeholder_serial))
            .filter(|serial| !serial.is_empty()),
        speed_mts: known_speed(s.word(OFF_CONFIGURED_SPEED))
            .or_else(|| known_speed(s.word(OFF_SPEED))),
        memory_type: memory_type.map(memory_type_name).unwrap_or("").to_string(),
    });
    Some(Slot {
        locator: usable(s.string(OFF_DEVICE_LOCATOR), is_placeholder),
        bank: usable(s.string(OFF_BANK_LOCATOR), is_placeholder),
        stick,
    })
}

/// The installed size in MiB, or `None` when the slot is empty or the size is
/// unknown (both are shown as an empty slot).
fn size_mb(s: &Structure<'_>) -> Option<u64> {
    let mib = match s.word(OFF_SIZE)? {
        0 | 0xFFFF => return None,
        // 32 GiB or more: the real size is in Extended Size, bits 30..0, MiB.
        0x7FFF => u64::from(s.dword(OFF_EXTENDED_SIZE)? & 0x7FFF_FFFF),
        // Bit 15 set: the low 15 bits are KiB. Round up so a tiny device is
        // not reported as 0 MiB.
        kib if kib & 0x8000 != 0 => u64::from(kib & 0x7FFF).div_ceil(1024),
        mib => u64::from(mib),
    };
    (mib != 0).then_some(mib)
}

/// A speed field's value, or `None` for 0 (unknown) and 0xFFFF (the value is
/// in an extended field this parser does not read).
fn known_speed(field: Option<u16>) -> Option<u32> {
    match field? {
        0 | 0xFFFF => None,
        mts => Some(u32::from(mts)),
    }
}

/// Memory Type codes for devices that are not RAM: ROM, FLASH, EEPROM,
/// FEPROM, EPROM, and logical non-volatile device.
fn is_non_volatile(memory_type: u8) -> bool {
    matches!(memory_type, 0x08..=0x0C | 0x1F)
}

/// Short display name for a Memory Type code, or `""` for uncommon or unknown
/// kinds.
fn memory_type_name(memory_type: u8) -> &'static str {
    match memory_type {
        0x0F => "SDRAM",
        0x12 => "DDR",
        0x13 => "DDR2",
        0x14 => "DDR2 FB-DIMM",
        0x18 => "DDR3",
        0x1A => "DDR4",
        0x1B => "LPDDR",
        0x1C => "LPDDR2",
        0x1D => "LPDDR3",
        0x1E => "LPDDR4",
        0x22 => "DDR5",
        0x23 => "LPDDR5",
        _ => "",
    }
}

/// Converts firmware string bytes (encoding unknown) to text: lossy UTF-8,
/// control characters removed, surrounding whitespace trimmed.
fn clean(bytes: &[u8]) -> String {
    let text: String = String::from_utf8_lossy(bytes)
        .chars()
        .filter(|c| !c.is_control())
        .collect();
    text.trim().to_string()
}

/// Placeholder text firmware uses when it has no real value. Compared
/// case-insensitively against the whole cleaned string.
const PLACEHOLDERS: &[&str] = &[
    "",
    "unknown",
    "not specified",
    "none",
    "n/a",
    "undefined",
    "to be filled by o.e.m.",
    "default string",
    "no dimm",
];

/// Field names that AMI firmware templates fill in, followed by digits, when
/// the board vendor did not supply a value (e.g. "Manufacturer00",
/// "ModulePartNumber00", "SerNum01"). Compared case-insensitively.
const TEMPLATE_PREFIXES: &[&str] = &[
    "Manufacturer",
    "ModulePartNumber",
    "PartNum",
    "SerNum",
    "AssetTagNum",
];

/// Whether a cleaned string (manufacturer, part number, serial, or slot or
/// bank label) carries no real information: one of [`PLACEHOLDERS`], or a
/// [`TEMPLATE_PREFIXES`] entry followed only by digits.
fn is_placeholder(text: &str) -> bool {
    let template = TEMPLATE_PREFIXES.iter().any(|prefix| {
        text.get(..prefix.len())
            .zip(text.get(prefix.len()..))
            .is_some_and(|(head, tail)| {
                head.eq_ignore_ascii_case(prefix) && tail.chars().all(|c| c.is_ascii_digit())
            })
    });
    template || PLACEHOLDERS.iter().any(|p| text.eq_ignore_ascii_case(p))
}

/// Whether a cleaned serial number carries no real information: a general
/// placeholder, no ASCII letter or digit at all, all `0`s or all `F`s
/// (optionally after a `0x` prefix), or `12345678`.
fn is_placeholder_serial(text: &str) -> bool {
    let digits = text
        .get(..2)
        .filter(|prefix| prefix.eq_ignore_ascii_case("0x"))
        .and_then(|_| text.get(2..))
        .unwrap_or(text);
    let all = |c: char| digits.chars().all(|d| d.eq_ignore_ascii_case(&c));
    is_placeholder(text)
        || !text.chars().any(|c| c.is_ascii_alphanumeric())
        || all('0')
        || all('F')
        || text == "12345678"
}

/// Returns `text`, or an empty string if `placeholder` says it is one.
fn usable(text: String, placeholder: fn(&str) -> bool) -> String {
    if placeholder(&text) {
        String::new()
    } else {
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds one SMBIOS structure: 4-byte header, `body` (the rest of the
    /// formatted area), then the string set.
    fn structure(ty: u8, body: &[u8], strings: &[&[u8]]) -> Vec<u8> {
        let mut out = vec![ty, (4 + body.len()) as u8, 0x34, 0x12];
        out.extend_from_slice(body);
        if strings.is_empty() {
            out.extend_from_slice(&[0, 0]);
        } else {
            for s in strings {
                out.extend_from_slice(s);
                out.push(0);
            }
            out.push(0);
        }
        out
    }

    /// The type-127 end-of-table structure.
    fn end() -> Vec<u8> {
        structure(127, &[], &[])
    }

    /// An unrelated structure of the given type with a short body and strings.
    fn other(ty: u8) -> Vec<u8> {
        structure(ty, &[1, 2, 3, 4, 5, 6], &[b"Vendor", b"Version"])
    }

    /// An unrelated structure with no strings at all, as types 16, 19 and 20
    /// always are: the formatted area is followed by exactly two NULs.
    /// `len` is the formatted-area length, header included.
    fn bare(ty: u8, len: u8) -> Vec<u8> {
        let body: Vec<u8> = (4..len).map(|i| i.wrapping_mul(7) | 1).collect();
        let s = structure(ty, &body, &[]);
        assert_eq!(s.len(), len as usize + 2);
        assert_eq!(&s[s.len() - 2..], [0, 0]);
        s
    }

    /// A type-17 Memory Device. Empty string fields are written as string
    /// index 0 ("no string"). `len` is the formatted-area length, at most
    /// 0x5C (SMBIOS 3.3).
    #[derive(Clone)]
    struct Dev {
        len: u8,
        size: u16,
        ext_size: u32,
        locator: &'static [u8],
        bank: &'static [u8],
        mem_type: u8,
        speed: u16,
        manufacturer: &'static [u8],
        serial: &'static [u8],
        asset: &'static [u8],
        part: &'static [u8],
        configured: u16,
    }

    fn dev() -> Dev {
        Dev {
            len: 0x28,
            size: 8192,
            ext_size: 0,
            locator: b"DIMM_A1",
            bank: b"BANK 0",
            mem_type: 0x1A,
            speed: 3200,
            manufacturer: b"Kingston",
            serial: b"1A2B3C4D",
            asset: b"AssetTag",
            part: b"KF432C16BB/8",
            configured: 3000,
        }
    }

    /// A type-17 device whose every string index is 0, so it has no strings.
    fn dev_without_strings() -> Dev {
        Dev {
            locator: b"",
            bank: b"",
            manufacturer: b"",
            serial: b"",
            asset: b"",
            part: b"",
            ..dev()
        }
    }

    impl Dev {
        fn bytes(&self) -> Vec<u8> {
            let mut strings: Vec<&[u8]> = Vec::new();
            let mut idx = |s: &'static [u8]| -> u8 {
                if s.is_empty() {
                    0
                } else {
                    strings.push(s);
                    strings.len() as u8
                }
            };
            let locator = idx(self.locator);
            let bank = idx(self.bank);
            let manufacturer = idx(self.manufacturer);
            let serial = idx(self.serial);
            let asset = idx(self.asset);
            let part = idx(self.part);

            let mut f = vec![0u8; 0x5C];
            f[0x04..0x06].copy_from_slice(&0x1000u16.to_le_bytes());
            f[0x06..0x08].copy_from_slice(&0xFFFEu16.to_le_bytes());
            f[0x08..0x0A].copy_from_slice(&64u16.to_le_bytes());
            f[0x0A..0x0C].copy_from_slice(&64u16.to_le_bytes());
            f[0x0C..0x0E].copy_from_slice(&self.size.to_le_bytes());
            f[0x0E] = 0x09;
            f[0x10] = locator;
            f[0x11] = bank;
            f[0x12] = self.mem_type;
            f[0x13..0x15].copy_from_slice(&0x0080u16.to_le_bytes());
            f[0x15..0x17].copy_from_slice(&self.speed.to_le_bytes());
            f[0x17] = manufacturer;
            f[0x18] = serial;
            f[0x19] = asset;
            f[0x1A] = part;
            f[0x1B] = 1;
            f[0x1C..0x20].copy_from_slice(&self.ext_size.to_le_bytes());
            f[0x20..0x22].copy_from_slice(&self.configured.to_le_bytes());
            f.truncate(self.len as usize);
            structure(17, &f[4..], &strings)
        }
    }

    fn table(parts: &[Vec<u8>]) -> Vec<u8> {
        parts.concat()
    }

    /// Parses a table holding just this one device and returns its slot.
    fn one(d: &Dev) -> Slot {
        let slots = parse_slots(&table(&[d.bytes(), end()]));
        assert_eq!(slots.len(), 1, "expected exactly one slot");
        slots.into_iter().next().unwrap()
    }

    fn one_stick(d: &Dev) -> Stick {
        one(d).stick.expect("expected a stick")
    }

    #[test]
    fn two_sticks_two_empty_slots_among_other_structures() {
        let a1 = Dev {
            locator: b"DIMM_A1",
            bank: b"P0 CHANNEL A",
            size: 0,
            manufacturer: b"Unknown",
            serial: b"",
            part: b"",
            ..dev()
        };
        let a2 = Dev {
            locator: b"DIMM_A2",
            bank: b"P0 CHANNEL A",
            size: 16384,
            manufacturer: b"G.Skill",
            serial: b"0F3A21B9",
            part: b"F4-3600C16-16GVKC",
            speed: 3600,
            configured: 3200,
            ..dev()
        };
        let b1 = Dev {
            locator: b"DIMM_B1",
            bank: b"P0 CHANNEL B",
            size: 0,
            manufacturer: b"",
            serial: b"",
            part: b"",
            ..dev()
        };
        let b2 = Dev {
            locator: b"DIMM_B2",
            bank: b"P0 CHANNEL B",
            size: 8192,
            manufacturer: b"Kingston",
            serial: b"A1B2C3D4",
            part: b"KF432C16BB/8",
            speed: 3200,
            configured: 2666,
            ..dev()
        };
        // Types 16, 19 and 20 have no strings, as on real boards.
        let t = table(&[
            other(0),
            other(1),
            bare(16, 0x17),
            a1.bytes(),
            a2.bytes(),
            bare(19, 0x1F),
            b1.bytes(),
            bare(20, 0x23),
            b2.bytes(),
            bare(19, 0x1F),
            end(),
        ]);
        let slots = parse_slots(&t);
        assert_eq!(
            slots,
            vec![
                Slot {
                    locator: "DIMM_A1".into(),
                    bank: "P0 CHANNEL A".into(),
                    stick: None,
                },
                Slot {
                    locator: "DIMM_A2".into(),
                    bank: "P0 CHANNEL A".into(),
                    stick: Some(Stick {
                        size_mb: 16384,
                        manufacturer: "G.Skill".into(),
                        part_number: "F4-3600C16-16GVKC".into(),
                        serial: Some("0F3A21B9".into()),
                        speed_mts: Some(3200),
                        memory_type: "DDR4".into(),
                    }),
                },
                Slot {
                    locator: "DIMM_B1".into(),
                    bank: "P0 CHANNEL B".into(),
                    stick: None,
                },
                Slot {
                    locator: "DIMM_B2".into(),
                    bank: "P0 CHANNEL B".into(),
                    stick: Some(Stick {
                        size_mb: 8192,
                        manufacturer: "Kingston".into(),
                        part_number: "KF432C16BB/8".into(),
                        serial: Some("A1B2C3D4".into()),
                        speed_mts: Some(2666),
                        memory_type: "DDR4".into(),
                    }),
                },
            ]
        );
    }

    #[test]
    fn size_in_mib() {
        assert_eq!(
            one_stick(&Dev {
                size: 4096,
                ..dev()
            })
            .size_mb,
            4096
        );
        assert_eq!(
            one_stick(&Dev {
                size: 0x7FFE,
                ..dev()
            })
            .size_mb,
            0x7FFE
        );
    }

    #[test]
    fn size_in_kib_when_bit_15_set() {
        // 0x4000 KiB = 16 MiB.
        let s = one_stick(&Dev {
            size: 0x8000 | 0x4000,
            ..dev()
        });
        assert_eq!(s.size_mb, 16);
        // Under 1 MiB rounds up so a present device never shows as 0 MiB.
        let s = one_stick(&Dev {
            size: 0x8000 | 512,
            ..dev()
        });
        assert_eq!(s.size_mb, 1);
    }

    #[test]
    fn extended_size_for_large_sticks() {
        let s = one_stick(&Dev {
            size: 0x7FFF,
            ext_size: 32 * 1024,
            ..dev()
        });
        assert_eq!(s.size_mb, 32 * 1024);
        let s = one_stick(&Dev {
            size: 0x7FFF,
            ext_size: 64 * 1024,
            ..dev()
        });
        assert_eq!(s.size_mb, 64 * 1024);
        // Bit 31 is reserved and must be ignored.
        let s = one_stick(&Dev {
            size: 0x7FFF,
            ext_size: 0x8000_0000 | (64 * 1024),
            ..dev()
        });
        assert_eq!(s.size_mb, 64 * 1024);
    }

    #[test]
    fn extended_size_missing_or_zero_means_unknown() {
        let s = one(&Dev {
            size: 0x7FFF,
            ext_size: 0,
            ..dev()
        });
        assert_eq!(s.stick, None);
        // Structure too short to contain Extended Size.
        let s = one(&Dev {
            size: 0x7FFF,
            ext_size: 65536,
            len: 0x1C,
            ..dev()
        });
        assert_eq!(s.stick, None);
        assert_eq!(s.locator, "DIMM_A1");
    }

    #[test]
    fn size_zero_is_empty_slot_with_locator() {
        let s = one(&Dev { size: 0, ..dev() });
        assert_eq!(s.stick, None);
        assert_eq!(s.locator, "DIMM_A1");
        assert_eq!(s.bank, "BANK 0");
    }

    #[test]
    fn size_unknown_is_empty_slot() {
        let s = one(&Dev {
            size: 0xFFFF,
            ..dev()
        });
        assert_eq!(s.stick, None);
        assert_eq!(s.locator, "DIMM_A1");
    }

    #[test]
    fn configured_speed_preferred_over_speed() {
        let s = one_stick(&Dev {
            speed: 3600,
            configured: 3200,
            ..dev()
        });
        assert_eq!(s.speed_mts, Some(3200));
    }

    #[test]
    fn speed_used_when_configured_speed_unknown() {
        let s = one_stick(&Dev {
            speed: 3600,
            configured: 0,
            ..dev()
        });
        assert_eq!(s.speed_mts, Some(3600));
        let s = one_stick(&Dev {
            speed: 3600,
            configured: 0xFFFF,
            ..dev()
        });
        assert_eq!(s.speed_mts, Some(3600));
    }

    #[test]
    fn speed_none_when_both_unknown() {
        let s = one_stick(&Dev {
            speed: 0,
            configured: 0,
            ..dev()
        });
        assert_eq!(s.speed_mts, None);
        let s = one_stick(&Dev {
            speed: 0xFFFF,
            configured: 0xFFFF,
            ..dev()
        });
        assert_eq!(s.speed_mts, None);
        let s = one_stick(&Dev {
            speed: 0xFFFF,
            configured: 0,
            ..dev()
        });
        assert_eq!(s.speed_mts, None);
    }

    #[test]
    fn smbios_2_1_structure_has_only_early_fields() {
        // Length 0x15: ends after Type Detail. No speed, no manufacturer,
        // serial or part number (their string indexes are not present).
        let s = one(&Dev { len: 0x15, ..dev() });
        assert_eq!(s.locator, "DIMM_A1");
        assert_eq!(s.bank, "BANK 0");
        assert_eq!(
            s.stick,
            Some(Stick {
                size_mb: 8192,
                manufacturer: String::new(),
                part_number: String::new(),
                serial: None,
                speed_mts: None,
                memory_type: "DDR4".into(),
            })
        );
    }

    #[test]
    fn smbios_2_3_structure_has_speed_and_strings_but_no_configured_speed() {
        let s = one_stick(&Dev {
            len: 0x1B,
            speed: 2400,
            configured: 2133,
            ..dev()
        });
        assert_eq!(s.speed_mts, Some(2400));
        assert_eq!(s.manufacturer, "Kingston");
        assert_eq!(s.serial.as_deref(), Some("1A2B3C4D"));
        assert_eq!(s.part_number, "KF432C16BB/8");
        assert_eq!(s.size_mb, 8192);
    }

    #[test]
    fn structure_ending_mid_field_ignores_that_field() {
        // Length 0x16 cuts the two-byte Speed field in half.
        let s = one_stick(&Dev {
            len: 0x16,
            speed: 2400,
            ..dev()
        });
        assert_eq!(s.speed_mts, None);
        // Length 0x21 cuts Configured Memory Speed in half.
        let s = one_stick(&Dev {
            len: 0x21,
            speed: 2400,
            configured: 2133,
            ..dev()
        });
        assert_eq!(s.speed_mts, Some(2400));
    }

    #[test]
    fn very_short_structure_still_gives_a_slot() {
        // Too short even for Size: an empty, unlabelled slot.
        let s = one(&Dev { len: 0x0C, ..dev() });
        assert_eq!(
            s,
            Slot {
                locator: String::new(),
                bank: String::new(),
                stick: None
            }
        );
    }

    #[test]
    fn structure_too_short_for_memory_type_keeps_locators_and_is_not_skipped() {
        // The Memory Type byte (0x12) would say FLASH, but it lies outside
        // the structure, so the device must not be skipped as non-RAM.
        let flash_if_read = Dev {
            mem_type: 0x09,
            ..dev()
        };
        let s = one(&Dev {
            len: 0x11,
            ..flash_if_read.clone()
        });
        assert_eq!(s.locator, "DIMM_A1");
        assert_eq!(s.bank, "", "bank index at 0x11 is outside length 0x11");
        assert_eq!(s.stick.as_ref().map(|k| k.size_mb), Some(8192));
        assert_eq!(s.stick.unwrap().memory_type, "");

        let s = one(&Dev {
            len: 0x12,
            ..flash_if_read
        });
        assert_eq!(s.locator, "DIMM_A1");
        assert_eq!(s.bank, "BANK 0");
        assert_eq!(s.stick.as_ref().map(|k| k.size_mb), Some(8192));
        assert_eq!(s.stick.unwrap().memory_type, "");
    }

    #[test]
    fn size_0x8000_is_zero_kib_and_an_empty_slot() {
        let s = one(&Dev {
            size: 0x8000,
            ..dev()
        });
        assert_eq!(s.stick, None);
        assert_eq!(s.locator, "DIMM_A1");
    }

    #[test]
    fn no_string_structures_before_between_and_after_devices() {
        let a = Dev {
            locator: b"A",
            ..dev()
        };
        let b = Dev {
            locator: b"B",
            serial: b"B-SERIAL",
            ..dev()
        };
        let t = table(&[
            bare(16, 0x17),
            a.bytes(),
            bare(19, 0x1F),
            bare(20, 0x23),
            b.bytes(),
            bare(20, 0x23),
            bare(19, 0x1F),
            end(),
        ]);
        let slots = parse_slots(&t);
        let locators: Vec<_> = slots.iter().map(|s| s.locator.as_str()).collect();
        assert_eq!(locators, ["A", "B"]);
        let serials: Vec<_> = slots
            .iter()
            .map(|s| s.stick.as_ref().and_then(|k| k.serial.as_deref()))
            .collect();
        assert_eq!(serials, [Some("1A2B3C4D"), Some("B-SERIAL")]);
    }

    #[test]
    fn no_string_structure_directly_before_a_device() {
        // A single no-string structure, then the only device: if the
        // terminator were mis-measured the device would be lost.
        let t = table(&[bare(32, 0x0B), dev().bytes(), end()]);
        let slots = parse_slots(&t);
        assert_eq!(slots.len(), 1);
        assert_eq!(slots[0].locator, "DIMM_A1");
    }

    #[test]
    fn device_with_no_strings_followed_by_another_device() {
        let t = table(&[
            dev_without_strings().bytes(),
            Dev {
                locator: b"B",
                ..dev()
            }
            .bytes(),
            end(),
        ]);
        let slots = parse_slots(&t);
        assert_eq!(
            slots,
            vec![
                Slot {
                    locator: String::new(),
                    bank: String::new(),
                    stick: Some(Stick {
                        size_mb: 8192,
                        manufacturer: String::new(),
                        part_number: String::new(),
                        serial: None,
                        speed_mts: Some(3000),
                        memory_type: "DDR4".into(),
                    }),
                },
                Slot {
                    locator: "B".into(),
                    bank: "BANK 0".into(),
                    stick: Some(Stick {
                        size_mb: 8192,
                        manufacturer: "Kingston".into(),
                        part_number: "KF432C16BB/8".into(),
                        serial: Some("1A2B3C4D".into()),
                        speed_mts: Some(3000),
                        memory_type: "DDR4".into(),
                    }),
                },
            ]
        );
    }

    /// A structure of type `ty` and formatted length `len` whose bytes are
    /// zero apart from string indexes: `refs` pairs an offset with a 1-based
    /// string index.
    fn with_strings(ty: u8, len: u8, refs: &[(usize, u8)], strings: &[&[u8]]) -> Vec<u8> {
        let mut f = vec![0u8; len as usize];
        for &(offset, index) in refs {
            f[offset] = index;
        }
        structure(ty, &f[4..], strings)
    }

    /// A hand-built table shaped like a typical two-of-four-populated AMI
    /// desktop board with SMBIOS 3.3. It is not a dump from a real machine
    /// (none was available); the layout, structure lengths and strings
    /// follow what such firmware emits: BIOS (0), System (1) and Baseboard
    /// (2) with strings; Physical Memory Array (16), Memory Array Mapped
    /// Address (19) and Memory Device Mapped Address (20) with no strings;
    /// four 0x5C-byte Memory Devices (17) whose empty slots carry "Unknown"
    /// / "Not Specified" strings; then End of Table (127).
    #[test]
    fn hand_built_realistic_desktop_layout() {
        let bios = with_strings(
            0,
            0x1A,
            &[(0x04, 1), (0x05, 2), (0x08, 3)],
            &[
                b"American Megatrends International, LLC.",
                b"1.A0",
                b"05/12/2023",
            ],
        );
        let system = with_strings(
            1,
            0x1B,
            &[
                (0x04, 1),
                (0x05, 2),
                (0x06, 3),
                (0x07, 4),
                (0x19, 5),
                (0x1A, 6),
            ],
            &[
                b"Example Board Co.",
                b"Example Desktop",
                b"1.0",
                b"To be filled by O.E.M.",
                b"To be filled by O.E.M.",
                b"To be filled by O.E.M.",
            ],
        );
        let baseboard = with_strings(
            2,
            0x0F,
            &[
                (0x04, 1),
                (0x05, 2),
                (0x06, 3),
                (0x07, 4),
                (0x08, 5),
                (0x0A, 6),
            ],
            &[
                b"Example Board Co.",
                b"B550 EXAMPLE",
                b"1.0",
                b"BSS-0123456789",
                b"To be filled by O.E.M.",
                b"To be filled by O.E.M.",
            ],
        );
        let empty = |locator, bank| Dev {
            len: 0x5C,
            size: 0,
            locator,
            bank,
            mem_type: 0x02,
            speed: 0,
            configured: 0,
            manufacturer: b"Unknown",
            serial: b"Unknown",
            asset: b"Not Specified",
            part: b"Unknown",
            ..dev()
        };
        let populated = |locator, bank, serial| Dev {
            len: 0x5C,
            size: 16384,
            locator,
            bank,
            mem_type: 0x1A,
            speed: 3600,
            configured: 3600,
            manufacturer: b"G Skill Intl",
            serial,
            asset: b"Not Specified",
            part: b"F4-3600C18-16GVK    ",
            ..dev()
        };
        let t = table(&[
            bios,
            system,
            baseboard,
            bare(16, 0x17),
            empty(b"DIMM 0", b"P0 CHANNEL A").bytes(),
            populated(b"DIMM 1", b"P0 CHANNEL A", b"0A1B2C3D").bytes(),
            empty(b"DIMM 0", b"P0 CHANNEL B").bytes(),
            populated(b"DIMM 1", b"P0 CHANNEL B", b"00000000").bytes(),
            bare(19, 0x1F),
            bare(20, 0x23),
            bare(20, 0x23),
            end(),
        ]);
        let stick = |serial: Option<&str>| Stick {
            size_mb: 16384,
            manufacturer: "G Skill Intl".into(),
            part_number: "F4-3600C18-16GVK".into(),
            serial: serial.map(String::from),
            speed_mts: Some(3600),
            memory_type: "DDR4".into(),
        };
        let slot = |locator: &str, bank: &str, stick| Slot {
            locator: locator.into(),
            bank: bank.into(),
            stick,
        };
        assert_eq!(
            parse_slots(&t),
            vec![
                slot("DIMM 0", "P0 CHANNEL A", None),
                slot("DIMM 1", "P0 CHANNEL A", Some(stick(Some("0A1B2C3D")))),
                slot("DIMM 0", "P0 CHANNEL B", None),
                slot("DIMM 1", "P0 CHANNEL B", Some(stick(None))),
            ]
        );
    }

    const COMMON_PLACEHOLDERS: &[&[u8]] = &[
        b"",
        b"   ",
        b"Unknown",
        b"UNKNOWN",
        b"Not Specified",
        b"not specified",
        b"None",
        b"N/A",
        b"Undefined",
        b"To Be Filled By O.E.M.",
        b"to be filled by o.e.m.",
        b"Default string",
        b"NO DIMM",
        b"  Not Specified  ",
        // AMI firmware template defaults: a field name followed by digits.
        b"Manufacturer00",
        b"Manufacturer0",
        b"MANUFACTURER1",
        b"ModulePartNumber00",
        b"PartNum0",
        b"PartNum",
        b"SerNum00",
        b"AssetTagNum0",
    ];

    #[test]
    fn placeholder_manufacturer_becomes_empty() {
        for p in COMMON_PLACEHOLDERS {
            let s = one_stick(&Dev {
                manufacturer: p,
                ..dev()
            });
            assert_eq!(
                s.manufacturer,
                "",
                "manufacturer {:?}",
                String::from_utf8_lossy(p)
            );
        }
    }

    #[test]
    fn placeholder_part_number_becomes_empty() {
        for p in COMMON_PLACEHOLDERS {
            let s = one_stick(&Dev { part: p, ..dev() });
            assert_eq!(s.part_number, "", "part {:?}", String::from_utf8_lossy(p));
        }
    }

    #[test]
    fn placeholder_serial_becomes_none() {
        let serial_only: &[&[u8]] = &[
            b"0",
            b"00000000",
            b"0000000000000000",
            b"FFFFFFFF",
            b"ffffffff",
            b"12345678",
            b"SerNum0",
            b"SerNum01",
            b"SerNum3",
            b"SERNUM12",
            b" 00000000 ",
            // No ASCII letter or digit at all.
            b"\xFF\xFF\xFF\xFF",
            b"----",
            b"#. .#",
            // Hex-prefixed all-zero or all-F.
            b"0x00000000",
            b"0X0000",
            b"0xFFFFFFFF",
            b"0xffff",
            b"0x",
        ];
        for p in COMMON_PLACEHOLDERS.iter().chain(serial_only) {
            let s = one_stick(&Dev { serial: p, ..dev() });
            assert_eq!(s.serial, None, "serial {:?}", String::from_utf8_lossy(p));
        }
    }

    #[test]
    fn real_serials_with_hex_prefix_or_punctuation_are_kept() {
        for real in [
            &b"0x1234ABCD"[..],
            b"0xF0",
            b"0x00000001",
            b"-A-",
            b"\xFF7\xFF",
        ] {
            let s = one_stick(&Dev {
                serial: real,
                ..dev()
            });
            let expected = clean(real);
            assert_eq!(s.serial.as_deref(), Some(expected.as_str()));
        }
    }

    #[test]
    fn template_prefix_needs_only_digits_after_it() {
        let s = one_stick(&Dev {
            manufacturer: b"Manufacturer Inc",
            part: b"PartNum0A",
            serial: b"SerNum 1",
            ..dev()
        });
        assert_eq!(s.manufacturer, "Manufacturer Inc");
        assert_eq!(s.part_number, "PartNum0A");
        assert_eq!(s.serial.as_deref(), Some("SerNum 1"));
    }

    #[test]
    fn placeholder_locators_become_empty() {
        for p in COMMON_PLACEHOLDERS {
            let s = one(&Dev {
                locator: p,
                bank: p,
                ..dev()
            });
            let shown = String::from_utf8_lossy(p);
            assert_eq!(s.locator, "", "locator {shown:?}");
            assert_eq!(s.bank, "", "bank {shown:?}");
            assert!(s.stick.is_some());
        }
    }

    #[test]
    fn real_values_that_resemble_placeholders_are_kept() {
        let s = one_stick(&Dev {
            serial: b"00000001",
            manufacturer: b"Unknown Corp",
            part: b"NONE-1234",
            ..dev()
        });
        assert_eq!(s.serial.as_deref(), Some("00000001"));
        assert_eq!(s.manufacturer, "Unknown Corp");
        assert_eq!(s.part_number, "NONE-1234");
        let s = one_stick(&Dev {
            serial: b"F0F0F0F0",
            ..dev()
        });
        assert_eq!(s.serial.as_deref(), Some("F0F0F0F0"));
        let s = one_stick(&Dev {
            serial: b"SerNumX",
            ..dev()
        });
        assert_eq!(s.serial.as_deref(), Some("SerNumX"));
    }

    #[test]
    fn serial_only_placeholders_are_valid_elsewhere() {
        // The all-zero, all-F, hex-prefix and no-alphanumeric rules apply to
        // serials only.
        let s = one_stick(&Dev {
            part: b"00000000",
            manufacturer: b"FFFF",
            ..dev()
        });
        assert_eq!(s.part_number, "00000000");
        assert_eq!(s.manufacturer, "FFFF");
        let s = one(&Dev {
            part: b"0x0000",
            manufacturer: b"----",
            locator: b"0",
            ..dev()
        });
        assert_eq!(s.locator, "0");
        let stick = s.stick.unwrap();
        assert_eq!(stick.part_number, "0x0000");
        assert_eq!(stick.manufacturer, "----");
    }

    #[test]
    fn locators_are_trimmed() {
        let s = one(&Dev {
            locator: b"  ChannelA-DIMM0 ",
            bank: b" BANK 0\t",
            ..dev()
        });
        assert_eq!(s.locator, "ChannelA-DIMM0");
        assert_eq!(s.bank, "BANK 0");
    }

    #[test]
    fn strings_are_trimmed_and_control_characters_stripped() {
        let s = one_stick(&Dev {
            manufacturer: b"  Micron\tTech\x07  ",
            part: b"\x01MTA16ATF2G64AZ\x7f",
            serial: b" 1A2B\r\n3C4D ",
            ..dev()
        });
        assert_eq!(s.manufacturer, "MicronTech");
        assert_eq!(s.part_number, "MTA16ATF2G64AZ");
        assert_eq!(s.serial.as_deref(), Some("1A2B3C4D"));
    }

    #[test]
    fn invalid_utf8_is_converted_lossily() {
        let s = one_stick(&Dev {
            manufacturer: b"Sams\xFFung",
            ..dev()
        });
        assert_eq!(s.manufacturer, "Sams\u{FFFD}ung");
    }

    #[test]
    fn memory_type_names() {
        let cases: &[(u8, &str)] = &[
            (0x0F, "SDRAM"),
            (0x12, "DDR"),
            (0x13, "DDR2"),
            (0x14, "DDR2 FB-DIMM"),
            (0x18, "DDR3"),
            (0x1A, "DDR4"),
            (0x1B, "LPDDR"),
            (0x1C, "LPDDR2"),
            (0x1D, "LPDDR3"),
            (0x1E, "LPDDR4"),
            (0x22, "DDR5"),
            (0x23, "LPDDR5"),
            (0x01, ""),
            (0x02, ""),
            (0x03, ""),
            (0x19, ""),
            (0x99, ""),
        ];
        for &(code, name) in cases {
            let s = one_stick(&Dev {
                mem_type: code,
                ..dev()
            });
            assert_eq!(s.memory_type, name, "memory type {code:#04x}");
        }
    }

    #[test]
    fn non_ram_memory_devices_are_skipped() {
        for code in [0x08u8, 0x09, 0x0A, 0x0B, 0x0C, 0x1F] {
            let flash = Dev {
                mem_type: code,
                locator: b"BIOS",
                size: 16,
                ..dev()
            };
            let ram = Dev {
                locator: b"DIMM_A1",
                ..dev()
            };
            let slots = parse_slots(&table(&[flash.bytes(), ram.bytes(), end()]));
            assert_eq!(slots.len(), 1, "type {code:#04x} should be skipped");
            assert_eq!(slots[0].locator, "DIMM_A1");
        }
    }

    #[test]
    fn stops_at_end_of_table_structure() {
        let a = Dev {
            locator: b"A",
            ..dev()
        };
        let b = Dev {
            locator: b"B",
            ..dev()
        };
        let slots = parse_slots(&table(&[a.bytes(), end(), b.bytes()]));
        assert_eq!(slots.len(), 1);
        assert_eq!(slots[0].locator, "A");
    }

    #[test]
    fn missing_end_of_table_still_parses_everything() {
        let a = Dev {
            locator: b"A",
            ..dev()
        };
        let b = Dev {
            locator: b"B",
            ..dev()
        };
        let slots = parse_slots(&table(&[other(0), a.bytes(), b.bytes()]));
        let locators: Vec<_> = slots.iter().map(|s| s.locator.as_str()).collect();
        assert_eq!(locators, ["A", "B"]);
    }

    #[test]
    fn empty_input_gives_no_slots() {
        assert_eq!(parse_slots(&[]), vec![]);
    }

    #[test]
    fn truncated_mid_header_keeps_earlier_slots() {
        let a = Dev {
            locator: b"A",
            ..dev()
        }
        .bytes();
        let b = Dev {
            locator: b"B",
            ..dev()
        }
        .bytes();
        for cut in 1..4 {
            let mut t = a.clone();
            t.extend_from_slice(&b[..cut]);
            let slots = parse_slots(&t);
            assert_eq!(slots.len(), 1);
            assert_eq!(slots[0].locator, "A");
        }
    }

    #[test]
    fn truncated_mid_formatted_area_keeps_earlier_slots() {
        let a = Dev {
            locator: b"A",
            ..dev()
        }
        .bytes();
        let b = Dev {
            locator: b"B",
            ..dev()
        }
        .bytes();
        let mut t = a.clone();
        t.extend_from_slice(&b[..0x20]);
        let slots = parse_slots(&t);
        assert_eq!(slots.len(), 1);
        assert_eq!(slots[0].locator, "A");
    }

    #[test]
    fn truncated_mid_strings_drops_the_incomplete_structure() {
        let a = Dev {
            locator: b"A",
            ..dev()
        }
        .bytes();
        let b = Dev {
            locator: b"B",
            ..dev()
        }
        .bytes();
        // Cut inside the string set, and just before the final NUL.
        for cut in [0x28 + 1, 0x28 + 5, b.len() - 1] {
            let mut t = a.clone();
            t.extend_from_slice(&b[..cut]);
            let slots = parse_slots(&t);
            assert_eq!(slots.len(), 1, "cut at {cut}");
            assert_eq!(slots[0].locator, "A");
        }
    }

    #[test]
    fn length_below_header_size_stops_parsing() {
        let a = Dev {
            locator: b"A",
            ..dev()
        }
        .bytes();
        for bad_len in 0..4u8 {
            let mut t = a.clone();
            t.extend_from_slice(&[17, bad_len, 0, 0, 0, 0]);
            t.extend_from_slice(
                &Dev {
                    locator: b"B",
                    ..dev()
                }
                .bytes(),
            );
            let slots = parse_slots(&t);
            assert_eq!(slots.len(), 1, "length {bad_len}");
            assert_eq!(slots[0].locator, "A");
        }
    }

    #[test]
    fn length_past_end_stops_parsing() {
        let a = Dev {
            locator: b"A",
            ..dev()
        }
        .bytes();
        let mut b = Dev {
            locator: b"B",
            ..dev()
        }
        .bytes();
        b[1] = 0xFF;
        let mut t = a.clone();
        t.extend_from_slice(&b);
        let slots = parse_slots(&t);
        assert_eq!(slots.len(), 1);
        assert_eq!(slots[0].locator, "A");
    }

    #[test]
    fn string_index_out_of_range_reads_as_no_string() {
        let mut t = Dev {
            serial: b"",
            ..dev()
        }
        .bytes();
        // Device Locator says string 9 and Serial says string 200; only
        // five strings exist.
        t[0x10] = 9;
        t[0x18] = 200;
        t.extend_from_slice(&end());
        let slots = parse_slots(&t);
        assert_eq!(slots.len(), 1);
        assert_eq!(slots[0].locator, "");
        let stick = slots[0].stick.as_ref().unwrap();
        assert_eq!(stick.serial, None);
        assert_eq!(stick.manufacturer, "Kingston");
    }

    /// Small deterministic xorshift generator so the fuzz test is repeatable.
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn below(&mut self, n: usize) -> usize {
            (self.next() % n as u64) as usize
        }
    }

    #[test]
    fn fuzzed_tables_never_panic() {
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        let base = table(&[
            other(0),
            bare(16, 0x17),
            Dev { len: 0x5C, ..dev() }.bytes(),
            dev_without_strings().bytes(),
            bare(20, 0x23),
            Dev {
                locator: b"A",
                ..dev()
            }
            .bytes(),
            Dev { size: 0, ..dev() }.bytes(),
            other(19),
            Dev {
                size: 0x7FFF,
                ext_size: 65536,
                ..dev()
            }
            .bytes(),
            Dev { len: 0x15, ..dev() }.bytes(),
            end(),
        ]);
        for round in 0..5000 {
            let t: Vec<u8> = match round % 3 {
                // Mutate a few bytes of a valid table, then maybe truncate.
                0 | 1 => {
                    let mut t = base.clone();
                    for _ in 0..1 + rng.below(8) {
                        let i = rng.below(t.len());
                        t[i] = rng.next() as u8;
                    }
                    if rng.below(2) == 0 {
                        t.truncate(rng.below(t.len() + 1));
                    }
                    t
                }
                // Pure garbage, starting with type 17 so the device parser runs.
                _ => {
                    let n = rng.below(300);
                    let mut t: Vec<u8> = (0..n).map(|_| rng.next() as u8).collect();
                    if n > 0 {
                        t[0] = 17;
                    }
                    t
                }
            };
            let slots = parse_slots(&t);
            // Every slot comes from a complete structure of at least 6 bytes.
            assert!(slots.len() <= t.len() / 6, "round {round}");
        }
    }

    /// Builds a Windows `RawSMBIOSData` blob: 8-byte header then `table`.
    fn raw(length: u32, table: &[u8]) -> Vec<u8> {
        let mut r = vec![0, 3, 4, 0];
        r.extend_from_slice(&length.to_le_bytes());
        r.extend_from_slice(table);
        r
    }

    #[test]
    fn raw_header_gives_table_of_stated_length() {
        let t = table(&[dev().bytes(), end()]);
        let r = raw(t.len() as u32, &t);
        assert_eq!(table_from_raw(&r), Ok(&t[..]));
    }

    #[test]
    fn raw_header_ignores_trailing_padding() {
        let t = table(&[dev().bytes(), end()]);
        let mut r = raw(t.len() as u32, &t);
        r.extend_from_slice(&[0xAA; 16]);
        assert_eq!(table_from_raw(&r), Ok(&t[..]));
    }

    #[test]
    fn raw_header_length_past_returned_data_is_malformed() {
        let t = table(&[dev().bytes(), end()]);
        let r = raw(t.len() as u32 + 1, &t);
        assert_eq!(table_from_raw(&r), Err(DetectError::Malformed));
        let r = raw(u32::MAX, &t);
        assert_eq!(table_from_raw(&r), Err(DetectError::Malformed));
    }

    #[test]
    fn raw_data_shorter_than_header_is_malformed() {
        for n in 0..8 {
            assert_eq!(table_from_raw(&vec![0; n]), Err(DetectError::Malformed));
        }
        // Exactly a header with zero length is a valid, empty table.
        assert_eq!(table_from_raw(&raw(0, &[])), Ok(&[][..]));
    }

    #[test]
    fn errors_have_readable_messages() {
        assert!(DetectError::Unsupported.to_string().contains("Windows"));
        assert!(DetectError::TableUnavailable(5).to_string().contains("5"));
        assert!(DetectError::Malformed.to_string().contains("malformed"));
    }

    #[cfg(not(windows))]
    #[test]
    fn detect_is_unsupported_off_windows() {
        assert_eq!(detect_slots(), Err(DetectError::Unsupported));
    }

    #[cfg(windows)]
    #[test]
    fn detect_reads_the_real_table() {
        let slots = detect_slots().expect("SMBIOS table should be readable");
        println!("detected slots: {slots:#?}");
    }
}
