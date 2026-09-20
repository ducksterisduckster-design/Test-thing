//! Flag-based check detection.

//! `process_inventory_items` in `core.rs` spots a check when a placeholder item
//! lands in the inventory. That works for normal pickups, but not for item lots
//! and shop purchases that only show a popup or shop listing without granting
//! anything. Those checks are found by watching the game's own event flags.

//! Flags and locations are many-to-many (one flag covers a whole armor set, and
//! an item with two sources, like the Talisman Pouch, can have two flags for
//! one location). So we build a `location -> flags` map, and a location counts
//! as checked if any of its flags is set.

use std::collections::{HashMap, HashSet};

use eldenring::cs::{ItemLotParam_enemy, ItemLotParam_map};
use eldenring::param::ITEMLOT_PARAM_ST;
use log::*;

use crate::item::RegulationManager;
use crate::slot_data::EventFlagId;

/// The top byte of `get_item_flag_id08` that marks a row as carrying an encoded
/// location ID instead of real 8th-slot data.
const LOCATION_TAG: u32 = 0xE000_0000;
const LOCATION_TAG_MASK: u32 = 0xFF00_0000;
const LOCATION_HIGH_BITS_MASK: u32 = 0x00FF_FFFF;

/// If the randomizer tagged `row` with a location ID, decodes it and returns it
/// with the event flag to poll for it.
fn decode_archipelago_row(row: &ITEMLOT_PARAM_ST) -> Option<(i64, EventFlagId)> {
    let high = row.get_item_flag_id08();
    if high & LOCATION_TAG_MASK != LOCATION_TAG {
        return None;
    }

    let high_bits = (high & LOCATION_HIGH_BITS_MASK) as i64;
    // The bindings make `lot_item_id08` signed, but what was stored is the raw
    // bit pattern. Reinterpret it as unsigned before widening.
    let low_bits = row.lot_item_id08() as u32 as i64;
    let location_id = (high_bits << 32) | low_bits;

    Some((location_id, EventFlagId(row.get_item_flag_id())))
}

/// Scans every row of one `ItemLotParam` table and records the
/// randomizer-tagged ones in `map`.
fn collect_location_flags<P>(regulation: &RegulationManager, map: &mut HashMap<i64, HashSet<EventFlagId>>)
where
    P: eldenring::cs::SoloParam<StructType = ITEMLOT_PARAM_ST>,
{
    for (_, row) in regulation.rows::<P>() {
        if let Some((location_id, flag)) = decode_archipelago_row(row) {
            map.entry(location_id).or_default().insert(flag);
        }
    }
}

/// Builds the `location ID -> flags` map from the tagged rows in
/// `ItemLotParam_map` and `ItemLotParam_enemy` (which also holds the fake rows standing in for shop entries).

/// Only call this once per session, since the regulation doesn't change while
/// the game runs. Cache the result instead of rebuilding it every tick.
pub fn build_location_flag_map(regulation: &RegulationManager) -> HashMap<i64, HashSet<EventFlagId>> {
    let mut map = HashMap::new();
    collect_location_flags::<ItemLotParam_map>(regulation, &mut map);
    collect_location_flags::<ItemLotParam_enemy>(regulation, &mut map);

    let flag_count: usize = map.values().map(HashSet::len).sum();
    info!(
        "Built Archipelago location/flag map: {} location(s), {} flag(s)",
        map.len(),
        flag_count
    );

    map
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Makes a zeroed `ITEMLOT_PARAM_ST` and applies the given setters. Zeroing
    /// is fine here: the struct is `#[repr(C)]` and all plain integers, so all-zero is always a valid value.
    
    fn row(set: impl FnOnce(&mut ITEMLOT_PARAM_ST)) -> ITEMLOT_PARAM_ST {
        let mut row = unsafe { std::mem::zeroed::<ITEMLOT_PARAM_ST>() };
        set(&mut row);
        row
    }

    #[test]
    fn decodes_a_tagged_row() {
        let location_id: i64 = 0x00AB_CDEF_1234_5678;
        let flag = 92_000u32;

        let row = row(|r| {
            r.set_lot_item_id08((location_id & 0xFFFF_FFFF) as i32);
            r.set_get_item_flag_id08(LOCATION_TAG | ((location_id >> 32) as u32));
            r.set_get_item_flag_id(flag);
        });

        let decoded = decode_archipelago_row(&row);
        assert_eq!(decoded, Some((location_id, EventFlagId(flag))));
    }

    #[test]
    fn decodes_a_location_id_with_high_bit_of_low_word_set() {
        // `lot_item_id08` is signed, so a location ID whose low 32 bits have
        // the top bit set comes back as a negative `i32`. Make sure that  doesn't get sign-extended.
        
        let location_id: i64 = 0x0000_0001_8000_0000;

        let row = row(|r| {
            r.set_lot_item_id08((location_id & 0xFFFF_FFFF) as i32); // negative as i32
            r.set_get_item_flag_id08(LOCATION_TAG | ((location_id >> 32) as u32));
        });

        assert_eq!(decode_archipelago_row(&row).map(|(id, _)| id), Some(location_id));
    }

    #[test]
    fn ignores_an_untagged_row() {
        // A row that happens to use a real flag ID in its 8th slot, with no
        // `0xE` tag, mustn't be read as an encoded location.
        let row = row(|r| {
            r.set_get_item_flag_id08(12_345);
            r.set_lot_item_id08(6789);
        });

        assert_eq!(decode_archipelago_row(&row), None);
    }

    #[test]
    fn ignores_a_zeroed_row() {
        // Most real rows have both fields at 0 (8th slot unused). Make sure
        // that isn't read as location 0 tagged with `0xE0000000`.
        let row = row(|_| {});
        assert_eq!(decode_archipelago_row(&row), None);
    }
}
