use std::sync::{LazyLock, Mutex};

use eldenring::cs::{
    GameDataMan, ItemBuffer, ItemBufferEntry, ItemCategory, ItemId, MAP_ITEM_MAN_GRANT_ITEM_VA,
    MapItemMan, SoloParamRepository,
};
use eldenring::param::{EquipParamPassive, EquipParamStruct, EquipParamStructMut};
use fromsoftware_shared::FromStatic;
use ilhook::x64::*;
use log::*;

use crate::save_data::SaveData;

const ARCHIPELAGO_GOODS_ID_START: u32 = 3_780_000;
const ARCHIPELAGO_PROGRESSION_ICON_ID: u16 = 15363;
const ARCHIPELAGO_USEFUL_ICON_ID: u16 = 15333;

static DISPLAY_ITEMS_TO_REMOVE: LazyLock<Mutex<Vec<ItemId>>> =
    LazyLock::new(|| Mutex::new(Vec::new()));

/// DS3-shaped wrapper around ER's param repository.
pub struct RegulationManager(&'static SoloParamRepository);

impl RegulationManager {
    pub fn instance() -> Option<Self> {
        Some(Self(unsafe { SoloParamRepository::instance() }.ok()?))
    }

    pub fn get_equip_param(&self, id: ItemId) -> Option<EquipParamStruct<'_>> {
        self.0.get_equip_param(id)
    }
}

/// Establishes hooks which ensure the items (which may be placeholders encoding
/// information relevant to Archipelago) are replaced by those which are correct
/// in-game.
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

/// A callback that's run when the player receives items in a way that would
/// make them pop up in a message on screen.
fn on_grant_items(items: &mut ItemBuffer) {
    let mut index = 0;
    while index < items.len() {
        let item = items[index];
        info!("Received {}x {:?}", item.quantity, item.id);

        if item.id.category() != ItemCategory::Goods
            || item.id.param_id() <= ARCHIPELAGO_GOODS_ID_START
        {
            // This is a vanilla item.
            index += 1;
            continue;
        }

        let (location_id, archipelago_item, foreign_display_item, basic_price, sell_value) = {
            // Replace placeholders with their real equivalents.
            let regulation_manager = RegulationManager::instance()
                .expect("SoloParamRepository should be available in on_grant_items");
            let row = regulation_manager
                .get_equip_param(item.id)
                .unwrap_or_else(|| panic!("Expected row to exist for {:?}", item.id.param_id()));
            let row = row
                .as_dyn()
                .as_goods()
                .unwrap_or_else(|| panic!("Archipelago ID {:?} should be Goods", item.id));
            (
                row.archipelago_location_id(),
                row.archipelago_item(),
                foreign_display_item_id(row.appearance_replace_item_id()),
                row.basic_price(),
                row.sell_value(),
            )
        };

        info!("  Archipelago location: {}", location_id);

        if let Some((real_id, quantity)) = archipelago_item {
            if let Some(ref mut save_data) = SaveData::instance_mut() {
                // Save data *should* always be loaded when the player gets an
                // item, but there's no need to crash if it's not.
                save_data.locations.insert(location_id);
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
                // grant entry prevents ER from trying to add the placeholder
                // as a real, carry-limited inventory item.
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
                    queue_display_item_cleanup(display_item);
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

fn queue_display_item_cleanup(id: ItemId) {
    DISPLAY_ITEMS_TO_REMOVE.lock().unwrap().push(id);
}

/// Shows a one-off pickup pop-up for a foreign virtual location's display item,
/// then queues the temporary item for removal from the inventory.
///
/// Virtual (priority) locations have no physical world item, so unlike normal
/// foreign pickups this doesn't pass through [on_grant_items]. We replicate the
/// same behaviour here: progression/useful display items get the dialog, others
/// only log, and the display item is removed again on the next frame.
pub(crate) fn show_virtual_location_display_item(item_man: &mut MapItemMan, display_item: ItemId) {
    // Guard against granting a display item that doesn't exist in regulation
    // which would feed ItemGive an invalid id.
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
    queue_display_item_cleanup(display_item);
}

pub fn remove_queued_display_items() {
    let display_items = {
        let mut queue = DISPLAY_ITEMS_TO_REMOVE.lock().unwrap();
        if queue.is_empty() {
            return;
        }
        queue.drain(..).collect::<Vec<_>>()
    };

    let Ok(game_data_man) = (unsafe { GameDataMan::instance_mut() }) else {
        DISPLAY_ITEMS_TO_REMOVE
            .lock()
            .unwrap()
            .extend(display_items);
        return;
    };

    for id in display_items {
        game_data_man.remove_item(id, 1);
    }
}

pub trait ItemIdExt {
    /// Returns whether this ID represents a placeholder item added specifically
    /// for Archipelago.
    fn is_archipelago(&self) -> bool;
}

impl ItemIdExt for ItemId {
    fn is_archipelago(&self) -> bool {
        self.category() == ItemCategory::Goods && self.param_id() > ARCHIPELAGO_GOODS_ID_START
    }
}

pub trait EquipParamExt {
    /// Returns the Archipelago location ID encoded in this item's unused
    /// params.
    fn archipelago_location_id(&self) -> i64;

    /// If this parameter represents a synthetic wrapper around a local item,
    /// returns the real item ID and the quantity that should be given to the
    /// player.
    fn archipelago_item(&self) -> Option<(ItemId, u32)>;
}

impl<T: ?Sized + EquipParamPassive> EquipParamExt for T {
    fn archipelago_location_id(&self) -> i64 {
        self.vagrant_item_lot_id() as i64
            + ((self.vagrant_bonus_ene_drop_item_lot_id() as i64) << 32)
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
