use std::{
    collections::{HashMap, HashSet},
    hash::Hash,
    str::FromStr,
};

use archipelago_rs as ap;
use eldenring::cs::GameDataMan;
use eldenring::cs::ItemId;
use fromsoftware_shared::FromStatic;
use serde::{Deserialize, Deserializer};

/// Setup info the Archipelago server sends for this game.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SlotData {
    /// Event flags that must all be set for the player to count as having won.
    pub goal: Vec<EventFlagId>,

    /// Archipelago item IDs to Elden Ring item IDs.
    pub ap_ids_to_item_ids: HashMap<I64Key, DeserializableItemId>,

    /// How many of an item each Archipelago item ID grants.
    pub item_counts: HashMap<I64Key, u32>,

    /// Progression item IDs that aren't deprioritized.
    #[serde(default)]
    pub non_deprioritized_progression_item_ids: HashSet<i64>,

    /// Location IDs to the event flags that control their map markers.
    #[serde(default)]
    pub priority_marker_flags: HashMap<I64Key, EventFlagId>,

    /// Location IDs to what has to be true before their map marker shows.
    #[serde(default)]
    pub priority_marker_requirements: HashMap<I64Key, MarkerRequirement>,

    /// Real locations to the AP-only virtual locations that get checked along
    /// with them.
    #[serde(default)]
    pub virtual_location_triggers: HashMap<I64Key, Vec<i64>>,

    /// Virtual locations that hold this player's own items. The server doesn't
    /// send those back, so the client grants them itself when the location is
    /// checked.
    #[serde(default)]
    pub local_virtual_location_items: HashMap<I64Key, i64>,

    /// Virtual locations that hold another player's item, mapped to a display
    /// item (a goods row baked into regulation.bin by the static randomizer) so
    /// the picker still sees a pickup pop-up.
    #[serde(default)]
    pub virtual_location_display_items: HashMap<I64Key, DeserializableItemId>,

    /// Region-lock item IDs to the event flags the region barriers check.
    #[serde(default)]
    pub region_lock_item_flags: HashMap<I64Key, EventFlagId>,

    /// This player's options.
    pub options: Options,
}

impl SlotData {
    pub fn expand_virtual_location_checks(&self, locations: &mut HashSet<i64>) -> bool {
        expand_virtual_location_checks(locations, &self.virtual_location_triggers)
    }

    pub fn virtual_location_trigger(&self, virtual_location_id: i64) -> Option<i64> {
        self.virtual_location_triggers
            .iter()
            .find_map(|(trigger, children)| {
                children.contains(&virtual_location_id).then_some(trigger.0)
            })
    }

    pub fn local_virtual_location_item(&self, virtual_location_id: i64) -> Option<i64> {
        self.local_virtual_location_items
            .get(&I64Key(virtual_location_id))
            .copied()
    }

    pub fn virtual_location_display_item(&self, virtual_location_id: i64) -> Option<ItemId> {
        self.virtual_location_display_items
            .get(&I64Key(virtual_location_id))
            .map(|display_item| display_item.0)
    }

    pub fn marker_requirement_met(
        &self,
        client: &ap::Client<SlotData>,
        location_id: i64,
        inventory: &InventorySnapshot,
    ) -> bool {
        self.priority_marker_requirements
            .get(&I64Key(location_id))
            .is_some_and(|requirement| {
                requirement.is_met_with(
                    &|item_id| self.has_ap_item(client, item_id, inventory),
                    &|location_id| client.is_local_location_checked(location_id),
                )
            })
    }

    fn has_ap_item(
        &self,
        client: &ap::Client<SlotData>,
        item_id: i64,
        inventory: &InventorySnapshot,
    ) -> bool {
        client
            .received_items()
            .iter()
            .any(|received| received.item().id() == item_id)
            || self.has_er_inventory_item(item_id, inventory)
    }

    pub fn has_er_inventory_item(&self, ap_item_id: i64, inventory: &InventorySnapshot) -> bool {
        let Some(er_id) = self
            .ap_ids_to_item_ids
            .get(&I64Key(ap_item_id))
            .map(|id| id.0)
        else {
            return false;
        };

        inventory.contains(er_id)
    }
}

/// A snapshot of the item IDs the player is carrying.
///
/// Walking the inventory isn't free and marker requirements ask about lots of
/// items at once, so we capture it once per sync pass and answer everything
/// from the snapshot. (It used to be re-walked for every marker, every frame.)
#[derive(Default)]
pub struct InventorySnapshot {
    /// Held item IDs with a quantity above zero. A plain `Vec` because `ItemId`
    /// isn't hashable, and we only ever look up a handful of items.
    items: Vec<ItemId>,
}

impl InventorySnapshot {
    /// Captures the current inventory, or an empty snapshot if there's no game
    /// data (like on the main menu).
    pub fn capture() -> Self {
        let Ok(game_data_man) = (unsafe { GameDataMan::instance() }) else {
            return Self::default();
        };

        Self {
            items: game_data_man
                .main_player_game_data
                .equipment
                .equip_inventory_data
                .items_data
                .items()
                .filter(|entry| entry.quantity > 0)
                .map(|entry| entry.item_id)
                .collect(),
        }
    }

    pub fn contains(&self, item_id: ItemId) -> bool {
        self.items.contains(&item_id)
    }
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum MarkerRequirement {
    Item(i64),
    Location(i64),
    All(Vec<MarkerRequirement>),
    Any(Vec<MarkerRequirement>),
    Never,
}

impl MarkerRequirement {
    fn is_met_with(
        &self,
        has_item: &impl Fn(i64) -> bool,
        has_location: &impl Fn(i64) -> bool,
    ) -> bool {
        match self {
            MarkerRequirement::Item(item_id) => has_item(*item_id),
            MarkerRequirement::Location(location_id) => has_location(*location_id),
            MarkerRequirement::All(requirements) => requirements
                .iter()
                .all(|requirement| requirement.is_met_with(has_item, has_location)),
            MarkerRequirement::Any(requirements) => requirements
                .iter()
                .any(|requirement| requirement.is_met_with(has_item, has_location)),
            MarkerRequirement::Never => false,
        }
    }
}

fn expand_virtual_location_checks(
    locations: &mut HashSet<i64>,
    virtual_location_triggers: &HashMap<I64Key, Vec<i64>>,
) -> bool {
    let mut changed = false;
    let mut pending = locations.iter().copied().collect::<Vec<_>>();

    while let Some(location_id) = pending.pop() {
        let Some(children) = virtual_location_triggers.get(&I64Key(location_id)) else {
            continue;
        };

        for &child in children {
            if locations.insert(child) {
                changed = true;
                pending.push(child);
            }
        }
    }

    changed
}

/// An Elden Ring event flag ID.
#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq, Hash)]
#[repr(transparent)]
pub struct EventFlagId(pub u32);

impl From<EventFlagId> for u32 {
    fn from(flag: EventFlagId) -> u32 {
        flag.0
    }
}

#[derive(Debug, Deserialize)]
pub struct Options {
    /// Whether the player expects the Elden Ring DLC to be installed.
    #[serde(deserialize_with = "int_to_bool")]
    pub enable_dlc: bool,
}

/// Reads an integer as a bool.
fn int_to_bool<'de, D>(deserializer: D) -> Result<bool, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(u64::deserialize(deserializer)? != 0)
}

#[derive(Debug, Clone, Copy, Deserialize, Hash, PartialEq, Eq)]
#[serde(try_from = "&str")]
#[repr(transparent)]
pub struct I64Key(pub i64);

impl TryFrom<&str> for I64Key {
    type Error = <i64 as FromStr>::Err;

    fn try_from(value: &str) -> Result<I64Key, Self::Error> {
        Ok(I64Key(i64::from_str(value)?))
    }
}

/// A deserializable wrapper around [ItemId].
#[derive(Debug, Deserialize)]
#[serde(try_from = "u32")]
#[repr(transparent)]
pub struct DeserializableItemId(pub ItemId);

impl TryFrom<u32> for DeserializableItemId {
    type Error = <ItemId as TryFrom<u32>>::Error;

    fn try_from(value: u32) -> Result<DeserializableItemId, Self::Error> {
        Ok(DeserializableItemId(value.try_into()?))
    }
}
