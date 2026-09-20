use std::collections::HashSet;
use std::sync::{LazyLock, Mutex};

use eldenring::cs::{
    GameDataMan, ItemBuffer, ItemBufferEntry, ItemCategory, ItemId, MAP_ITEM_MAN_GRANT_ITEM_VA,
    MapItemMan, SoloParamRepository,
};
use eldenring::param::{EquipParamPassive, EquipParamStruct, EquipParamStructMut};
use fromsoftware_shared::FromStatic;
use ilhook::x64::*;
use log::*;

use windows::Win32::Foundation::HMODULE;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::ProcessStatus::{GetModuleInformation, MODULEINFO};
use windows::Win32::System::Threading::GetCurrentProcess;

/// A stripped-down stand-in for the game's `MenuGaitem` struct, with just the
/// two fields the native drop function reads. Not the real type, only enough
/// layout to make the call work.
#[repr(C)]
struct FakeMenuGaitem {
    unk0: [u8; 0x48],
    inventory_index: i32,
    item_id: ItemId,
}

type DropItemFn = unsafe extern "C" fn(&FakeMenuGaitem, i32, bool);

/// Address of the native function the inventory menu uses to drop an item.
/// Found with an AOB scan on first use, then cached.
///
/// TODO: if the mod ever gets a generated VA for this (like
/// `MAP_ITEM_MAN_GRANT_ITEM_VA` from `eldenring::cs`), use that instead of
/// scanning here.
static DROP_ITEM_FN: LazyLock<DropItemFn> = LazyLock::new(|| {
    // The pattern matches partway into the function, so the function starts
    // `OFFSET` bytes earlier. It's core engine code and has been stable across
    // patches, but if item removal silently stops working after a game update,
    // re-check it in Ghidra.
    const PATTERN: &[Option<u8>] = &[
        Some(0x45), Some(0x0F), Some(0xB6), Some(0xF0),
        Some(0x8B), Some(0xDA), Some(0x48), Some(0x8B), Some(0xE9),
    ];
    const OFFSET: usize = 0x1C;

    let match_addr =
        unsafe { find_pattern(PATTERN) }.expect("Failed to find native drop-item function pattern");
    unsafe { std::mem::transmute::<usize, DropItemFn>(match_addr - OFFSET) }
});

/// Scans the main module's image for `pattern` (`None` is a wildcard) and
/// returns the address of the first match.
///
/// # Safety
/// Reads raw process memory across the main module. That's fine as long as the
/// module is fully loaded (not before or during `DllMain`), which is always
/// true for our lazy call sites.
unsafe fn find_pattern(pattern: &[Option<u8>]) -> Option<usize> {
    let module: HMODULE = unsafe { GetModuleHandleW(None) }.ok()?;
    let mut info = MODULEINFO::default();
    unsafe {
        GetModuleInformation(
            GetCurrentProcess(),
            module,
            &mut info,
            std::mem::size_of::<MODULEINFO>() as u32,
        )
    }
    .ok()?;

    let base = info.lpBaseOfDll as *const u8;
    let size = info.SizeOfImage as usize;
    let haystack = unsafe { std::slice::from_raw_parts(base, size) };

    haystack
        .windows(pattern.len())
        .position(|window| {
            window
                .iter()
                .zip(pattern)
                .all(|(byte, expected)| expected.map_or(true, |expected| *byte == expected))
        })
        .map(|offset| base as usize + offset)
}

/// Finds `item_id` in the player's inventory, using the combined key/normal
/// index the native drop function expects: key items by their position in
/// `key_entries`, normal items continuing after `key_items_capacity`.
///
/// TODO(verify): check these field names (`key_entries`, `normal_entries`,
/// `key_items_capacity`) against the `eldenring` crate. They're based on the
/// `EquipInventoryDataListEntry`/`InventoryItemsData` shapes, not the actual
/// source.
fn combined_inventory_index(
    items_data: &eldenring::cs::InventoryItemsData,
    item_id: ItemId,
) -> Option<i32> {
    if let Some(index) = items_data.key_entries().iter().position(|entry| {
        entry
            .as_option()
            .is_some_and(|entry| entry.item_id == item_id && entry.quantity > 0)
    }) {
        return Some(index as i32);
    }

    items_data
        .normal_entries()
        .iter()
        .position(|entry| {
            entry
                .as_option()
                .is_some_and(|entry| entry.item_id == item_id && entry.quantity > 0)
        })
        .map(|index| index as i32 + items_data.key_items_capacity as i32)
}

/// Removes `quantity` of the inventory entry at `inventory_index` (combined
/// key/normal index) by calling the same native function as the in-game "drop"
/// action, instead of the id-based `GameDataMan::remove_item`.
///
/// # Safety
/// Call this on the main thread with `GameDataMan` loaded. `inventory_index`
/// must point at a real, non-empty slot.
unsafe fn drop_item(inventory_index: i32, item_id: ItemId, quantity: i32) {
    let gaitem = FakeMenuGaitem {
        unk0: [0u8; 0x48],
        inventory_index,
        item_id,
    };
    unsafe { (*DROP_ITEM_FN)(&gaitem, quantity, false) }
}


use crate::save_data::SaveData;

/// Where the static randomizer starts allocating Archipelago placeholder goods
/// IDs. Foreign ("Other <player>'s ...") placeholders start here and local ones
/// at `8_100_000`. Vanilla goods top out at 2_220_010, so anything from here up
/// is synthetic.
const ARCHIPELAGO_GOODS_ID_START: u32 = 8_000_000;

/// Where local placeholders start (items that belong to this game). Everything
/// between [ARCHIPELAGO_GOODS_ID_START] and this is a foreign placeholder,
/// which stays fake.
const ARCHIPELAGO_LOCAL_GOODS_ID_START: u32 = 8_100_000;

/// Location IDs of zero or less mean the row has no Archipelago location.
/// Untagged rows keep the vanilla `-1` in the vagrant fields (which decodes to
/// `-1`), and the `999999999` template row has `0`. Real location IDs start in
/// the millions, so neither can be mistaken for one.
const MIN_VALID_LOCATION_ID: i64 = 1;
const ARCHIPELAGO_PROGRESSION_ICON_ID: u16 = 15363;
const ARCHIPELAGO_USEFUL_ICON_ID: u16 = 15333;

/// A foreign pickup's display item ("rainbow stone"), waiting to be removed
/// once its location is confirmed sent to the server.
struct PendingDisplayItem {
    id: ItemId,
    location_id: i64,
}

static DISPLAY_ITEMS_TO_REMOVE: LazyLock<Mutex<Vec<PendingDisplayItem>>> =
    LazyLock::new(|| Mutex::new(Vec::new()));

/// A DS3-style wrapper around ER's param repository.
pub struct RegulationManager(&'static SoloParamRepository);

impl RegulationManager {
    pub fn instance() -> Option<Self> {
        Some(Self(unsafe { SoloParamRepository::instance() }.ok()?))
    }

    pub fn get_equip_param(&self, id: ItemId) -> Option<EquipParamStruct<'_>> {
        self.0.get_equip_param(id)
    }

    /// Iterates over every row in a solo param table (like `ItemLotParam_map`
    /// or `ItemLotParam_enemy`), with each row's param ID, in ID order. Used to
    /// find rows the randomizer tagged with a location ID.
    pub fn rows<'a, P: eldenring::cs::SoloParam + 'a>(
        &'a self,
    ) -> impl Iterator<Item = (u32, &'a P::StructType)> + 'a {
        self.0.rows::<P>()
    }
}

/// Sets up the hooks that swap placeholder items (which may encode Archipelago
/// info) for the right in-game items.
pub unsafe fn hook_items() {
    let callback = |reg: *mut Registers| {
        let items = unsafe { &mut *((*reg).rdx as *mut ItemBuffer) };
        on_grant_items(items);
    };
    std::mem::forget(
        unsafe {
            hook_closure_jmp_back(
                *MAP_ITEM_MAN_GRANT_ITEM_VA as usize,
                callback,
                CallbackOption::None,
                HookFlags::empty(),
            )
        }
        .expect("Hooking MapItemMan::GrantItem failed"),
    );
}

/// Runs when the player receives items in a way that shows an on-screen
/// message.
fn on_grant_items(items: &mut ItemBuffer) {
    let mut index = 0;
    while index < items.len() {
        let item = items[index];
        info!("Received {}x {:?}", item.quantity, item.id);

        if item.id.category() != ItemCategory::Goods
            || item.id.param_id() < ARCHIPELAGO_GOODS_ID_START
        {
            // A vanilla item.
            index += 1;
            continue;
        }

        let (decoded, is_blank_local_placeholder) = {
            // Swap placeholders for their real items.
            let regulation_manager = RegulationManager::instance()
                .expect("SoloParamRepository should be available in on_grant_items");
            let row = regulation_manager
                .get_equip_param(item.id)
                .unwrap_or_else(|| panic!("Expected row to exist for {:?}", item.id.param_id()));
            let row = row
                .as_dyn()
                .as_goods()
                .unwrap_or_else(|| panic!("Archipelago ID {:?} should be Goods", item.id));
            let decoded = row.archipelago_location_id().map(|location_id| {
                (
                    location_id,
                    row.archipelago_item(),
                    foreign_display_item_id(row.appearance_replace_item_id()),
                    row.basic_price(),
                    row.sell_value(),
                )
            });
            // A local placeholder with neither a location nor a real item. The
            // `basic_price == 0` check keeps this to the randomizer's blank
            // rows.
            let is_blank_local_placeholder =
                item.id.is_local_archipelago() && row.basic_price() == 0;
            (decoded, is_blank_local_placeholder)
        };

        let Some((location_id, archipelago_item, foreign_display_item, basic_price, sell_value)) =
            decoded
        else {
            if is_blank_local_placeholder {
                // A local placeholder with no location or item data. Its pickup
                // is reported through the lot's event flag, and the real item
                // is granted by location (see
                // `Core::grant_local_location_items`), with its own pop-up. So
                // drop the placeholder from the grant completely, or you'd see
                // a second, fake pop-up.
                info!("  Local placeholder without item data; dropping it, the real item is granted by location");
                items.remove(index);
                continue;
            }

            // The ID is in the Archipelago range but the row has no location,
            // so this is either a foreign placeholder (reported through its
            // event flag) or a normal game item that just lives up there. Leave
            // it alone.
            info!("  Not an Archipelago placeholder; granting as-is");
            index += 1;
            continue;
        };

        info!("  Archipelago location: {}", location_id);

        if let Some((real_id, quantity)) = archipelago_item {
            if let Some(ref mut save_data) = SaveData::instance_mut() {
                // Save data should always be loaded when the player gets an
                // item, but there's no need to crash if it isn't.
                save_data.locations.insert(location_id);
                // The real item is handed over right here, so the by-location
                // grant mustn't give it again.
                save_data.local_virtual_items_granted.insert(location_id);
            }

            info!("  Converting to {}x {:?}", quantity, real_id);
            items[index].id = real_id;
            items[index].quantity = quantity;
            items[index].durability = u32::MAX;
            index += 1;
        } else {
            info!(
                "  Foreign Archipelago item has no local item data. Basic price: {}, sell value: {}",
                basic_price, sell_value
            );

            if let Some(ref mut save_data) = SaveData::instance_mut() {
                // Foreign items only need to check the location. Removing the
                // grant entry stops the game from adding the placeholder as a
                // real, carry-limited item.
                save_data.locations.insert(location_id);
                if let Some(display_item) = foreign_display_item {
                    info!("  Showing pickup notification with {:?}", display_item);
                    if should_show_item_get_dialog(display_item) {
                        show_item_get_dialog(display_item);
                    } else {
                        suppress_item_get_dialog(display_item);
                    }
                    items[index].id = display_item;
                    items[index].quantity = 1;
                    items[index].durability = u32::MAX;
                    items[index].gem = u32::MAX;
                    queue_display_item_cleanup(display_item, location_id);
                    index += 1;
                } else {
                    items.remove(index);
                }
            } else {
                warn!(
                    "  Save data is unavailable; leaving foreign Archipelago item in grant buffer"
                );
                index += 1;
            }
        }
    }
}

fn foreign_display_item_id(appearance_replace_item_id: i32) -> Option<ItemId> {
    if appearance_replace_item_id <= 0 {
        return None;
    }

    let id: ItemId = (ItemCategory::Goods as u32)
        .checked_shl(28)?
        .checked_add(appearance_replace_item_id as u32)?
        .try_into()
        .ok()?;
    (!id.is_archipelago()).then_some(id)
}

fn should_show_item_get_dialog(id: ItemId) -> bool {
    let Some(regulation_manager) = RegulationManager::instance() else {
        return false;
    };
    let Some(row) = regulation_manager.get_equip_param(id) else {
        return false;
    };
    let Some(row) = row.as_dyn().as_goods() else {
        return false;
    };

    matches!(
        row.icon_id(),
        ARCHIPELAGO_PROGRESSION_ICON_ID | ARCHIPELAGO_USEFUL_ICON_ID
    )
}

fn show_item_get_dialog(id: ItemId) -> bool {
    set_item_get_dialog(id, 2)
}

fn suppress_item_get_dialog(id: ItemId) -> bool {
    set_item_get_dialog(id, 0)
}

fn set_item_get_dialog(id: ItemId, show_dialog_cond_type: u8) -> bool {
    let Ok(solo_param_repository) = (unsafe { SoloParamRepository::instance_mut() }) else {
        return false;
    };
    let Some(row) = solo_param_repository.get_equip_param_mut(id) else {
        return false;
    };

    macro_rules! suppress {
        ($row:expr) => {{
            $row.set_show_log_cond_type(true);
            $row.set_show_dialog_cond_type(show_dialog_cond_type);
        }};
    }

    match row {
        EquipParamStructMut::EQUIP_PARAM_ACCESSORY_ST(row) => suppress!(row),
        EquipParamStructMut::EQUIP_PARAM_GEM_ST(row) => suppress!(row),
        EquipParamStructMut::EQUIP_PARAM_GOODS_ST(row) => suppress!(row),
        EquipParamStructMut::EQUIP_PARAM_PROTECTOR_ST(row) => suppress!(row),
        EquipParamStructMut::EQUIP_PARAM_WEAPON_ST(row) => suppress!(row),
    }

    true
}

fn queue_display_item_cleanup(id: ItemId, location_id: i64) {
    DISPLAY_ITEMS_TO_REMOVE
        .lock()
        .unwrap()
        .push(PendingDisplayItem { id, location_id });
}

/// Shows a one-off pickup pop-up for a foreign virtual location's display item,
/// then queues it for removal once `location_id` is confirmed sent to the
/// server.
///
/// Virtual (priority) locations have no world item, so unlike normal foreign
/// pickups they don't go through [on_grant_items]. We copy that behavior here:
/// progression/useful display items get the dialog, the rest only log.
pub(crate) fn show_virtual_location_display_item(
    item_man: &mut MapItemMan,
    display_item: ItemId,
    location_id: i64,
) {
    // Don't grant a display item that isn't in the regulation, or we'd feed
    // ItemGive an invalid ID.
    let display_item_exists = RegulationManager::instance()
        .is_some_and(|manager| manager.get_equip_param(display_item).is_some());
    if !display_item_exists {
        warn!(
            "Skipping foreign virtual location pop-up: display item {:?} is not in regulation",
            display_item
        );
        return;
    }

    if should_show_item_get_dialog(display_item) {
        show_item_get_dialog(display_item);
    } else {
        suppress_item_get_dialog(display_item);
    }

    item_man.grant_item(ItemBufferEntry::new(display_item, 1));
    queue_display_item_cleanup(display_item, location_id);
}

/// Removes display items (the "rainbow stones" shown for foreign pickups) once
/// their location is in `sent_locations`, meaning a `mark_checked` call to the
/// server included it.
///
/// Anything not sent yet (player offline, or this tick's send hasn't happened)
/// stays queued and is retried next call. That keeps the stand-in visible until
/// the server really has the check, instead of vanishing on a timer whether or
/// not it was sent.
pub fn remove_sent_display_items(sent_locations: &HashSet<i64>) {
    let (ready, still_pending): (Vec<_>, Vec<_>) = {
        let mut queue = DISPLAY_ITEMS_TO_REMOVE.lock().unwrap();
        if queue.is_empty() {
            return;
        }
        queue
            .drain(..)
            .partition(|item| sent_locations.contains(&item.location_id))
    };

    if !still_pending.is_empty() {
        DISPLAY_ITEMS_TO_REMOVE
            .lock()
            .unwrap()
            .extend(still_pending);
    }
    if ready.is_empty() {
        return;
    }

    for item in ready {
        // Re-borrow for each item, tightly scoped, so we never hold a reference
        // into the inventory across the native call that changes it (and a
        // mid-loop return to the main menu is handled cleanly).
        let index = {
            let Ok(game_data_man) = (unsafe { GameDataMan::instance() }) else {
                DISPLAY_ITEMS_TO_REMOVE.lock().unwrap().push(item);
                continue;
            };
            combined_inventory_index(
                &game_data_man
                    .main_player_game_data
                    .equipment
                    .equip_inventory_data
                    .items_data,
                item.id,
            )
        };

        match index {
            Some(index) => unsafe { drop_item(index, item.id, 1) },
            None => warn!(
                "Display item {:?} not found in inventory; skipping removal",
                item.id
            ),
        }
    }
}

pub trait ItemIdExt {
    /// Whether this ID is a placeholder item added just for Archipelago.
    fn is_archipelago(&self) -> bool;

    /// Whether this ID is in the range used for local (this game's own) item
    /// placeholders.
    fn is_local_archipelago(&self) -> bool;
}

impl ItemIdExt for ItemId {
    fn is_archipelago(&self) -> bool {
        self.category() == ItemCategory::Goods && self.param_id() >= ARCHIPELAGO_GOODS_ID_START
    }

    fn is_local_archipelago(&self) -> bool {
        self.category() == ItemCategory::Goods
            && self.param_id() >= ARCHIPELAGO_LOCAL_GOODS_ID_START
    }
}

pub trait EquipParamExt {
    /// The Archipelago location ID hidden in this item's unused params, or
    /// `None` if the randomizer never tagged the row.
    fn archipelago_location_id(&self) -> Option<i64>;

    /// If this param is a synthetic wrapper around a local item, returns the
    /// real item ID and how many to give.
    fn archipelago_item(&self) -> Option<(ItemId, u32)>;
}

impl<T: ?Sized + EquipParamPassive> EquipParamExt for T {
    fn archipelago_location_id(&self) -> Option<i64> {
        decode_location_id(
            self.vagrant_item_lot_id(),
            self.vagrant_bonus_ene_drop_item_lot_id(),
        )
    }

    fn archipelago_item(&self) -> Option<(ItemId, u32)> {
        if self.basic_price() == 0 {
            None
        } else {
            Some((
                (self.basic_price() as u32)
                    .try_into()
                    .unwrap_or_else(|err| {
                        panic!(
                            "invalid item ID {} found in synthetic item: {:?}",
                            self.basic_price(),
                            err
                        )
                    }),
                self.sell_value() as u32,
            ))
        }
    }
}

/// Decodes the location ID hidden across an equip param's two unused vagrant
/// fields, or `None` if the row was never tagged.
///
/// Both fields are signed in the bindings, but what's stored is the raw bit
/// pattern, so each half is reinterpreted as unsigned before widening.
/// Otherwise a half with its top bit set would be sign-extended into the result
/// (same fix as in checks.rs's `decode_archipelago_row`).
///
/// An untagged row keeps the vanilla `-1` in both fields, which decodes to
/// `-1`, and the `999999999` template row has `0`. Neither is a real location,
/// so the randomizer never wrote location data here and the row must be left
/// alone (see [MIN_VALID_LOCATION_ID]).
fn decode_location_id(
    vagrant_item_lot_id: i32,
    vagrant_bonus_ene_drop_item_lot_id: i32,
) -> Option<i64> {
    let low_bits = vagrant_item_lot_id as u32 as i64;
    let high_bits = vagrant_bonus_ene_drop_item_lot_id as u32 as i64;
    let location_id = (high_bits << 32) | low_bits;

    (location_id >= MIN_VALID_LOCATION_ID).then_some(location_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_a_tagged_row() {
        let location_id: i64 = 0x0000_0001_1234_5678;
        assert_eq!(
            decode_location_id(
                (location_id & 0xFFFF_FFFF) as i32,
                (location_id >> 32) as i32
            ),
            Some(location_id)
        );
    }

    #[test]
    fn decodes_a_location_id_with_high_bit_of_low_word_set() {
        // The low half goes through a signed field, so a location ID with bit
        // 31 set mustn't be sign-extended.
        let location_id: i64 = 0x0000_0001_8000_0000;
        assert_eq!(
            decode_location_id(
                (location_id & 0xFFFF_FFFF) as i32,
                (location_id >> 32) as i32
            ),
            Some(location_id)
        );
    }

    #[test]
    fn ignores_an_untagged_row() {
        // Vanilla rows leave both vagrant fields at -1. We only see them here
        // because their param ID is above the Archipelago range, and they must
        // pass through as normal items.
        assert_eq!(decode_location_id(-1, -1), None);
    }

    #[test]
    fn ignores_the_template_row() {
        // The `999999999` template row sits above the Archipelago range and
        // leaves its vagrant fields at 0, which would otherwise decode to a
        // believable location 0.
        assert_eq!(decode_location_id(0, 0), None);
    }
}