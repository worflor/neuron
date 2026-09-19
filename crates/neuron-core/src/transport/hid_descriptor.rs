// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! A pure HID report-descriptor parser (USB HID 1.11 §6.2.2 item format).
//!
//! Windows hands back per-collection report lengths ready-made (`HidP_GetCaps`); Linux only
//! exposes the raw descriptor bytes (`/sys/class/hidraw/*/device/report_descriptor`), so the
//! Linux backend has to compute the same numbers itself. Compiled on every platform (not just
//! `cfg(target_os = "linux")`) so this parser's tests run in Windows CI too — the byte-level logic
//! has no OS dependency and deserves the wider net.
//!
//! Only top-level Application collections produce output entries; a device with several top-level
//! collections behind one Linux `/dev/hidrawN` interface (which Windows would enumerate as several
//! separate paths) is exactly why the Linux backend needs this parser at all — see
//! `transport/hidraw.rs`.

use std::collections::HashMap;

/// One top-level Application collection's usage and per-kind report byte lengths, in
/// `HIDP_CAPS` semantics: the maximum report size of that kind within the collection, including
/// the one report-ID byte (present even when the collection doesn't use report IDs), or 0 if the
/// collection has no report of that kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CollectionCaps {
    pub usage_page: u16,
    pub usage: u16,
    pub feature_len: u16,
    pub input_len: u16,
    pub output_len: u16,
}

// Main item tags (bType = 0b00).
const TAG_INPUT: u8 = 0x8;
const TAG_OUTPUT: u8 = 0x9;
const TAG_COLLECTION: u8 = 0xA;
const TAG_FEATURE: u8 = 0xB;
const TAG_END_COLLECTION: u8 = 0xC;

// Global item tags (bType = 0b01). Only the ones this parser needs to track are named; the rest
// (Logical/Physical Minimum/Maximum, Unit, Unit Exponent, ...) fall through the `_` arm — their
// bytes are still consumed (so the cursor advances correctly), just not interpreted.
const TAG_USAGE_PAGE: u8 = 0x0;
const TAG_REPORT_SIZE: u8 = 0x7;
const TAG_REPORT_ID: u8 = 0x8;
const TAG_REPORT_COUNT: u8 = 0x9;
const TAG_PUSH: u8 = 0xA;
const TAG_POP: u8 = 0xB;

// Local item tags (bType = 0b10). Only Usage matters here.
const TAG_LOCAL_USAGE: u8 = 0x0;

const APPLICATION_COLLECTION: u8 = 0x01;

/// The Global items this parser tracks, saved/restored whole by Push/Pop (HID 1.11 §6.2.2.9-10).
/// Global items this parser doesn't need (Logical/Physical Minimum/Maximum, Unit, Unit Exponent)
/// are consumed for cursor advancement but never stored, so they aren't part of this snapshot —
/// a real device's Push/Pop pair still round-trips correctly for the fields we DO track.
#[derive(Clone, Copy, Default)]
struct Globals {
    usage_page: u16,
    report_id: u8,
    report_size: u32,
    report_count: u32,
}

/// Report kind index into the per-collection bit-accumulator array.
const KIND_INPUT: usize = 0;
const KIND_OUTPUT: usize = 1;
const KIND_FEATURE: usize = 2;

/// Parse a HID report descriptor into one [`CollectionCaps`] per top-level Application collection,
/// in descriptor order. Never panics: a truncated or malformed descriptor yields whatever prefix
/// of it could be understood (a truncated item stops the scan rather than erroring, since a
/// descriptor that names ITS OWN top-level collections correctly before the truncation point still
/// deserves an honest answer for those).
pub fn parse(desc: &[u8]) -> Vec<CollectionCaps> {
    let mut entries: Vec<CollectionCaps> = Vec::new();
    // Per top-level entry, per kind, bit total per report ID — report length is the MAX over
    // report IDs (HIDP_CAPS semantics: the buffer must fit the largest report of that kind).
    let mut bits: Vec<[HashMap<u8, u64>; 3]> = Vec::new();

    let mut globals = Globals::default();
    let mut global_stack: Vec<Globals> = Vec::new();
    // The most recent Local Usage item since the last Main item — HID 1.11 §6.2.2.8: local state
    // clears after every Main item, so this is `None` again once a Collection/Input/Output/Feature
    // item has consumed it.
    let mut local_usage: Option<(u16, u16)> = None;
    // Per open Collection, the `current_top` value to restore on its matching End Collection.
    let mut collection_stack: Vec<Option<usize>> = Vec::new();
    let mut current_top: Option<usize> = None;

    let mut i = 0usize;
    while i < desc.len() {
        let prefix = desc[i];

        // Long item (HID 1.11 §6.2.3): 0xFE, then a 1-byte size, a 1-byte tag, then `size` bytes
        // of data. Long items carry no Main/Global/Local semantics this parser tracks, so skip.
        if prefix == 0xFE {
            if desc.len() < i + 3 {
                break; // truncated header — nothing more can be read
            }
            let size = desc[i + 1] as usize;
            let end = i + 3 + size;
            if end > desc.len() {
                break; // truncated payload
            }
            i = end;
            continue;
        }

        // Short item prefix: bTag (bits 7-4), bType (bits 3-2), bSize (bits 1-0). bSize 3 means
        // 4 bytes of data, not 3 (HID 1.11 §6.2.2.2).
        let b_size_code = prefix & 0x03;
        let data_len = match b_size_code {
            0 => 0,
            1 => 1,
            2 => 2,
            _ => 4,
        };
        let b_type = (prefix >> 2) & 0x03;
        let b_tag = (prefix >> 4) & 0x0F;

        let data_start = i + 1;
        let data_end = data_start + data_len;
        if data_end > desc.len() {
            break; // truncated item
        }
        let data = &desc[data_start..data_end];
        let value: u32 = match data_len {
            0 => 0,
            1 => u32::from(data[0]),
            2 => u32::from(u16::from_le_bytes([data[0], data[1]])),
            _ => u32::from_le_bytes([data[0], data[1], data[2], data[3]]),
        };

        match b_type {
            // Global (0b01)
            1 => match b_tag {
                TAG_USAGE_PAGE => globals.usage_page = value as u16,
                TAG_REPORT_SIZE => globals.report_size = value,
                TAG_REPORT_ID => globals.report_id = value as u8,
                TAG_REPORT_COUNT => globals.report_count = value,
                TAG_PUSH => global_stack.push(globals),
                TAG_POP => {
                    if let Some(g) = global_stack.pop() {
                        globals = g;
                    }
                }
                _ => {}
            },
            // Local (0b10)
            2 => {
                // HID 1.11 §6.2.2.8: when several Usage items stack up before the Main item that
                // consumes them, the FIRST one is what a Collection binds to (Windows' `HidP_*`
                // agrees) — so only the first Usage since the last Main item is kept; later ones
                // are ignored for OUR purposes (this parser only needs the top-level collection's
                // own usage, not a full usage list).
                if b_tag == TAG_LOCAL_USAGE && local_usage.is_none() {
                    // A 4-byte Usage is an EXTENDED usage (HID 1.11 §6.2.2.7): high 16 bits are
                    // the usage page, low 16 bits the usage ID, overriding the global Usage Page
                    // for this item only (the global state itself is untouched).
                    local_usage = Some(if data_len == 4 {
                        ((value >> 16) as u16, (value & 0xFFFF) as u16)
                    } else {
                        (globals.usage_page, value as u16)
                    });
                }
            }
            // Main (0b00)
            0 => {
                match b_tag {
                    TAG_COLLECTION => {
                        let coll_type = data.first().copied().unwrap_or(0);
                        let prev = current_top;
                        let new_top = if coll_type == APPLICATION_COLLECTION && prev.is_none() {
                            let (usage_page, usage) = local_usage.unwrap_or((globals.usage_page, 0));
                            entries.push(CollectionCaps {
                                usage_page,
                                usage,
                                ..Default::default()
                            });
                            bits.push([HashMap::new(), HashMap::new(), HashMap::new()]);
                            Some(entries.len() - 1)
                        } else {
                            // Nested collection (Physical/Logical/Report/... or an Application
                            // collection nested inside another one): reports inside it still
                            // count toward the enclosing top-level entry, so `current_top` is
                            // simply carried through unchanged.
                            prev
                        };
                        collection_stack.push(prev);
                        current_top = new_top;
                    }
                    TAG_END_COLLECTION => {
                        // An extra End Collection with no matching Collection is malformed input;
                        // degrade to "not inside any collection" rather than panic on the pop.
                        current_top = collection_stack.pop().unwrap_or(None);
                    }
                    TAG_INPUT | TAG_OUTPUT | TAG_FEATURE => {
                        if let Some(idx) = current_top {
                            let kind = match b_tag {
                                TAG_INPUT => KIND_INPUT,
                                TAG_OUTPUT => KIND_OUTPUT,
                                _ => KIND_FEATURE,
                            };
                            // Saturating, both times. Report Size and Report Count are whatever
                            // 32-bit values the descriptor declares, and a descriptor is untrusted
                            // input from whatever got plugged in: their product reaches u64::MAX,
                            // and a second such item overflows the running total. Debug builds
                            // panic there; release builds wrap, which is worse — a wrapped total
                            // becomes a plausible-looking report length that is simply wrong.
                            // Saturating leaves an absurd length that the caller treats as absurd.
                            let add = u64::from(globals.report_size).saturating_mul(u64::from(globals.report_count));
                            let slot = bits[idx][kind].entry(globals.report_id).or_insert(0);
                            *slot = slot.saturating_add(add);
                        }
                    }
                    _ => {}
                }
                local_usage = None; // HID 1.11 §6.2.2.8: local state clears after every Main item.
            }
            _ => {}
        }

        i = data_end;
    }

    for (idx, kinds) in bits.into_iter().enumerate() {
        for (kind, per_report_id) in kinds.into_iter().enumerate() {
            let len = if per_report_id.is_empty() {
                0
            } else {
                let max_bits = per_report_id.values().copied().max().unwrap_or(0);
                let bytes = max_bits.div_ceil(8);
                // +1 for the report-ID byte, always present in HIDP_CAPS semantics — see the
                // module doc and `CollectionCaps`.
                (bytes + 1).min(u64::from(u16::MAX)) as u16
            };
            match kind {
                KIND_INPUT => entries[idx].input_len = len,
                KIND_OUTPUT => entries[idx].output_len = len,
                _ => entries[idx].feature_len = len,
            }
        }
    }

    entries
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Global item (1-byte data): bTag<<4 | bType<<2 | bSize.
    fn global1(tag: u8, val: u8) -> Vec<u8> {
        vec![(tag << 4) | (0b01 << 2) | 0b01, val]
    }
    /// Local item (1-byte data).
    fn local1(tag: u8, val: u8) -> Vec<u8> {
        vec![(tag << 4) | (0b10 << 2) | 0b01, val]
    }
    /// Main item (1-byte data).
    fn main1(tag: u8, val: u8) -> Vec<u8> {
        vec![(tag << 4) | 0b01, val]
    }
    /// Main item with no data (End Collection).
    fn main0(tag: u8) -> Vec<u8> {
        vec![(tag << 4)]
    }

    fn concat(parts: Vec<Vec<u8>>) -> Vec<u8> {
        parts.into_iter().flatten().collect()
    }

    #[test]
    fn unnumbered_90_byte_feature_report() {
        let desc = concat(vec![
            global1(TAG_USAGE_PAGE, 0x01),
            local1(TAG_LOCAL_USAGE, 0x01),
            main1(TAG_COLLECTION, 0x01), // Application
            global1(TAG_REPORT_SIZE, 8),
            global1(TAG_REPORT_COUNT, 90),
            main1(TAG_FEATURE, 0x02),
            main0(TAG_END_COLLECTION),
        ]);
        let caps = parse(&desc);
        assert_eq!(caps.len(), 1);
        assert_eq!(caps[0].feature_len, 91, "90 bytes of feature data + 1 report-ID byte");
        assert_eq!(caps[0].input_len, 0, "no Input item at all");
        assert_eq!(caps[0].output_len, 0, "no Output item at all");
    }

    /// The standard boot-keyboard descriptor, HID 1.11 Appendix B.1 / E.6: modifier byte (8 Input
    /// bits) + reserved byte (8 Input bits) + 5-bit LED report + 3-bit LED padding (both Output)
    /// + 6-byte keycode array (48 Input bits). Input total 64 bits = 8 bytes -> `input_len` 9.
    /// Output total 8 bits = 1 byte -> `output_len` 2.
    #[test]
    fn standard_boot_keyboard_descriptor() {
        let desc = concat(vec![
            global1(TAG_USAGE_PAGE, 0x01), // Generic Desktop
            local1(TAG_LOCAL_USAGE, 0x06), // Keyboard
            main1(TAG_COLLECTION, 0x01),   // Application
            global1(TAG_USAGE_PAGE, 0x07), // Key Codes
            vec![0x19, 0xE0],              // Usage Minimum (224) — 2-byte-ish data ignored fields
            vec![0x29, 0xE7],              // Usage Maximum (231)
            vec![0x15, 0x00],              // Logical Minimum (0)
            vec![0x25, 0x01],              // Logical Maximum (1)
            global1(TAG_REPORT_SIZE, 1),
            global1(TAG_REPORT_COUNT, 8),
            main1(TAG_INPUT, 0x02), // modifier byte
            global1(TAG_REPORT_COUNT, 1),
            global1(TAG_REPORT_SIZE, 8),
            main1(TAG_INPUT, 0x01), // reserved byte
            global1(TAG_REPORT_COUNT, 5),
            global1(TAG_REPORT_SIZE, 1),
            global1(TAG_USAGE_PAGE, 0x08), // LEDs
            vec![0x19, 0x01],               // Usage Minimum (1)
            vec![0x29, 0x05],               // Usage Maximum (5)
            main1(TAG_OUTPUT, 0x02), // LED report
            global1(TAG_REPORT_COUNT, 1),
            global1(TAG_REPORT_SIZE, 3),
            main1(TAG_OUTPUT, 0x01), // LED padding
            global1(TAG_REPORT_COUNT, 6),
            global1(TAG_REPORT_SIZE, 8),
            vec![0x15, 0x00], // Logical Minimum (0)
            vec![0x25, 0x65], // Logical Maximum (101)
            global1(TAG_USAGE_PAGE, 0x07), // Key Codes
            vec![0x19, 0x00],
            vec![0x29, 0x65],
            main1(TAG_INPUT, 0x00), // keycode array
            main0(TAG_END_COLLECTION),
        ]);
        let caps = parse(&desc);
        assert_eq!(caps.len(), 1);
        assert_eq!(caps[0].usage_page, 0x01);
        assert_eq!(caps[0].usage, 0x06);
        assert_eq!(caps[0].input_len, 9);
        assert_eq!(caps[0].output_len, 2);
        assert_eq!(caps[0].feature_len, 0);
    }

    /// Several top-level collections, each with its OWN Report ID, prove per-collection maxima
    /// don't bleed into each other.
    #[test]
    fn several_top_level_collections_with_report_ids() {
        let desc = concat(vec![
            // Collection 1: usage page 0xFF00 usage 0x01, report id 1, 8-byte feature report.
            global1(TAG_USAGE_PAGE, 0x01),
            local1(TAG_LOCAL_USAGE, 0x01),
            main1(TAG_COLLECTION, 0x01),
            global1(TAG_REPORT_ID, 1),
            global1(TAG_REPORT_SIZE, 8),
            global1(TAG_REPORT_COUNT, 8),
            main1(TAG_FEATURE, 0x02),
            main0(TAG_END_COLLECTION),
            // Collection 2: usage page 0x0C usage 0x01, report id 2, a smaller 2-byte input.
            global1(TAG_USAGE_PAGE, 0x0C),
            local1(TAG_LOCAL_USAGE, 0x01),
            main1(TAG_COLLECTION, 0x01),
            global1(TAG_REPORT_ID, 2),
            global1(TAG_REPORT_SIZE, 8),
            global1(TAG_REPORT_COUNT, 2),
            main1(TAG_INPUT, 0x02),
            main0(TAG_END_COLLECTION),
        ]);
        let caps = parse(&desc);
        assert_eq!(caps.len(), 2);
        assert_eq!(caps[0].usage_page, 0x01);
        assert_eq!(caps[0].feature_len, 9, "8 bytes + report-ID byte");
        assert_eq!(caps[0].input_len, 0);
        assert_eq!(caps[1].usage_page, 0x0C);
        assert_eq!(caps[1].input_len, 3, "2 bytes + report-ID byte");
        assert_eq!(caps[1].feature_len, 0);
    }

    /// Push/Pop: Report Size is overridden inside a nested scope, then restored by Pop so a LATER
    /// item in the outer scope sees the ORIGINAL size, not the overridden one.
    #[test]
    fn push_pop_restores_global_state() {
        let desc = concat(vec![
            global1(TAG_USAGE_PAGE, 0x01),
            local1(TAG_LOCAL_USAGE, 0x01),
            main1(TAG_COLLECTION, 0x01),
            global1(TAG_REPORT_SIZE, 8),
            global1(TAG_REPORT_COUNT, 1),
            vec![(TAG_PUSH << 4) | (0b01 << 2)], // Push, 0-byte data
            global1(TAG_REPORT_SIZE, 32),        // overridden inside the pushed scope
            vec![(TAG_POP << 4) | (0b01 << 2)],  // Pop — restores report_size = 8
            main1(TAG_FEATURE, 0x02), // must see the RESTORED size (8), not 32
            main0(TAG_END_COLLECTION),
        ]);
        let caps = parse(&desc);
        assert_eq!(caps.len(), 1);
        assert_eq!(caps[0].feature_len, 2, "1 byte (restored size 8, count 1) + report-ID byte");
    }

    /// A long item (0xFE) in the middle of the descriptor must be skipped without disturbing the
    /// surrounding short-item parse.
    #[test]
    fn long_item_is_skipped() {
        let mut desc = concat(vec![
            global1(TAG_USAGE_PAGE, 0x01),
            local1(TAG_LOCAL_USAGE, 0x01),
            main1(TAG_COLLECTION, 0x01),
        ]);
        desc.extend_from_slice(&[0xFE, 0x03, 0x99, 0xAA, 0xBB, 0xCC]); // long item, 3 bytes of junk
        desc.extend(concat(vec![
            global1(TAG_REPORT_SIZE, 8),
            global1(TAG_REPORT_COUNT, 4),
            main1(TAG_INPUT, 0x02),
            main0(TAG_END_COLLECTION),
        ]));
        let caps = parse(&desc);
        assert_eq!(caps.len(), 1);
        assert_eq!(caps[0].input_len, 5, "the long item must not have eaten any short items");
    }

    /// A Physical collection nested inside the Application collection: its Input item still
    /// counts toward the ENCLOSING top-level entry, and nesting doesn't create a second entry.
    #[test]
    fn nested_collection_reports_count_toward_the_top_level() {
        let desc = concat(vec![
            global1(TAG_USAGE_PAGE, 0x01),
            local1(TAG_LOCAL_USAGE, 0x02), // Mouse
            main1(TAG_COLLECTION, 0x01),   // Application
            local1(TAG_LOCAL_USAGE, 0x01),
            main1(TAG_COLLECTION, 0x00), // Physical (nested)
            global1(TAG_REPORT_SIZE, 8),
            global1(TAG_REPORT_COUNT, 3),
            main1(TAG_INPUT, 0x02),
            main0(TAG_END_COLLECTION), // end Physical
            main0(TAG_END_COLLECTION), // end Application
        ]);
        let caps = parse(&desc);
        assert_eq!(caps.len(), 1, "the nested Physical collection must not become its own entry");
        assert_eq!(caps[0].input_len, 4);
    }

    /// A 4-byte extended Usage local item (HID 1.11 §6.2.2.7) carries its own usage page in the
    /// high 16 bits, overriding the current global Usage Page for the collection it opens.
    #[test]
    fn extended_usage_carries_its_own_usage_page() {
        let mut desc = vec![(0b01 << 2) | 0b01, 0x01]; // Usage Page = 0x01 (global)
        desc.push((TAG_LOCAL_USAGE << 4) | (0b10 << 2) | 0b11); // Usage, 4-byte data
        desc.extend_from_slice(&0x000C_0001u32.to_le_bytes()); // page 0x000C usage 0x0001
        desc.extend(main1(TAG_COLLECTION, 0x01));
        desc.extend(main0(TAG_END_COLLECTION));
        let caps = parse(&desc);
        assert_eq!(caps.len(), 1);
        assert_eq!(caps[0].usage_page, 0x000C, "the extended usage's page, not the global 0x01");
        assert_eq!(caps[0].usage, 0x0001);
    }

    /// When several Usage items stack up before the Collection item that consumes them, the
    /// FIRST one is what the collection binds to (HID 1.11 §6.2.2.8), not the last.
    #[test]
    fn collection_binds_to_the_first_stacked_usage_not_the_last() {
        let desc = concat(vec![
            global1(TAG_USAGE_PAGE, 0x01),
            local1(TAG_LOCAL_USAGE, 0x02), // Mouse — first, must win
            local1(TAG_LOCAL_USAGE, 0x06), // Keyboard — second, must be ignored
            main1(TAG_COLLECTION, 0x01),
            main0(TAG_END_COLLECTION),
        ]);
        let caps = parse(&desc);
        assert_eq!(caps.len(), 1);
        assert_eq!(caps[0].usage, 0x02, "the FIRST stacked usage, not the last");
    }

    #[test]
    fn truncated_descriptor_does_not_panic() {
        let caps = parse(&[0x05]); // Usage Page item claiming 1 byte of data that isn't there
        assert!(caps.is_empty());
        let caps = parse(&[0xA1]); // Collection item, same story
        assert!(caps.is_empty());
        let caps = parse(&[]);
        assert!(caps.is_empty());
    }

    /// A descriptor declaring the largest Report Size and Report Count the item encoding allows,
    /// twice, overflows a u64 bit total. Found by `parser_never_panics`; kept as a named case
    /// because the consequence in a release build is not a panic but a wrapped total, which would
    /// read back as an ordinary, wrong report length.
    #[test]
    fn absurd_report_size_and_count_saturate() {
        let max32 = |tag: u8| {
            let mut v = vec![(tag << 4) | (0b01 << 2) | 0b11]; // Global, 4 bytes of data
            v.extend_from_slice(&u32::MAX.to_le_bytes());
            v
        };
        let mut desc = vec![0x05, 0x01, 0x09, 0x01, 0xA1, 0x01]; // Usage Page 1, Usage 1, Application
        for _ in 0..2 {
            desc.extend(max32(TAG_REPORT_SIZE));
            desc.extend(max32(TAG_REPORT_COUNT));
            desc.extend_from_slice(&[0x81, 0x02]); // Input
        }
        desc.push(0xC0); // End Collection

        let caps = parse(&desc);
        assert_eq!(caps.len(), 1);
        assert_eq!(
            caps[0].input_len,
            u16::MAX,
            "an absurd declared length clamps to an absurd value, rather than wrapping into a \
             plausible one"
        );
    }

    // The cross-check against Windows HidP_GetCaps lives in `super::parity`, which owns the
    // fixture format and the capture tests that write each half.

    proptest::proptest! {
        /// However hostile the bytes, the parser must return, never panic. This is the exit
        /// criterion for "arbitrary or truncated bytes must never panic" — the assertion is
        /// simply that `parse` returns at all.
        #[test]
        fn parser_never_panics(bytes in proptest::collection::vec(proptest::prelude::any::<u8>(), 0..1024)) {
            let _ = parse(&bytes);
        }
    }
}
