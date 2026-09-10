use std::{
    collections::HashSet,
    str::FromStr,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use archipelago_rs as ap;
use archipelago_rs::RichText;
use eldenring::cs::*;
use eldenring::param::EquipParamStructMut;
use fromsoftware_shared::FromStatic;
use log::*;
#[cfg(debug_assertions)]
use regex_macro::regex;

use crate::item::{EquipParamExt, ItemIdExt, RegulationManager, remove_queued_display_items};
use crate::save_data::*;
use crate::slot_data::{EventFlagId, I64Key, SlotData};
use shared::{Core as SharedCore, CoreBase};

const RECEIVED_ITEM_GRANT_INTERVAL: Duration = Duration::from_millis(33);
const ITEM_GET_DISPLAY_SUPPRESSION_DURATION: Duration = Duration::from_millis(75);

/// The core of the Archipelago mod. This is responsible for running the
/// non-UI-related game logic and interacting with the Archipelago client.
pub struct Core {
    /// The cross-game core.
    base: CoreBase<crate::game::EldenRing, SlotData>,

    /// The time we last granted an item to the player. Used to throttle incoming
    /// item bursts without making release queues take minutes to drain.
    last_item_time: Instant,

    /// The number of locations sent to the server in this session. This always
    /// starts at 0 when the player boots the game again to ensure that they
    /// resend any locations that may have been missed.
    locations_sent: usize,

    /// Whether the player has achieved their goal and sent that information to
    /// the Archipelago server. This is stored here rather than in the save data
    /// so that it's resent every time the player starts the game, just in case
    /// it got lost in transit.
    sent_goal: bool,

    /// Item display param rows that are currently suppressed for incoming
    /// Archipelago grants and need to be restored later.
    item_get_display_restores: Vec<ItemGetDisplayRestore>,
}

struct LocalVirtualItemGrant {
    location_id: i64,
    location_name: String,
    ap_item_id: i64,
    item_name: String,
    er_id: ItemId,
    quantity: u32,
}

impl shared::Core for Core {
    type SlotData = SlotData;
    type Game = crate::game::EldenRing;

    /// Creates a new instance of the mod.
    fn new() -> Result<Self> {
        Ok(Self {
            base: CoreBase::new("EldenRing")?,
            last_item_time: Instant::now(),
            locations_sent: 0,
            sent_goal: false,
            item_get_display_restores: Vec::new(),
        })
    }

    fn base(&self) -> &CoreBase<Self::Game, SlotData> {
        &self.base
    }

    fn base_mut(&mut self) -> &mut CoreBase<Self::Game, SlotData> {
        &mut self.base
    }

    /// Updates the game logic and checks for common errors. This does nothing
    /// if we're not currently connected to the Archipelago server or if the mod
    /// has encountered a fatal error.
    fn update_live(&mut self) -> Result<()> {
        self.check_seed_conflict()?;
        if let Some(save_data) = SaveData::instance_mut().as_mut()
            && save_data.seed.is_none()
        {
            save_data.seed = Some(self.seed().to_string());
        };

        self.check_dlc_error()?;
        self.restore_expired_item_get_displays();

        // Process events that should only happen when the player has a save
        // loaded and is actively playing.
        self.take_events();

        self.process_incoming_items();
        self.process_inventory_items()?;
        remove_queued_display_items();
        self.sync_region_lock_flags();
        self.sync_priority_location_markers();
        self.handle_goal()?;

        Ok(())
    }

    fn handle_command(&mut self, command: &str, arg: Option<&str>) -> bool {
        let mut arg_error = |usage: &str| {
            self.log(vec![
                RichText::Color {
                    text: format!("Invalid {}.", command),
                    color: ap::TextColor::Red,
                },
                " Usage:\n".into(),
                usage.into(),
            ]);
        };

        match command {
            "!getevent" => {
                let Some(flag) = arg.and_then(|f| u32::from_str(f).ok()) else {
                    arg_error("!getevent EVENT_FLAG");
                    return true;
                };

                let Some(value) = get_event_flag(EventFlagId(flag)) else {
                    self.log(RichText::Color {
                        text: "CSEventFlagMan not loaded".into(),
                        color: ap::TextColor::Red,
                    });
                    return true;
                };

                self.log(vec![
                    "Event ".into(),
                    RichText::Color {
                        text: format!("{:?}", flag),
                        color: ap::TextColor::Blue,
                    },
                    ": ".into(),
                    RichText::Color {
                        text: format!("{:?}", value),
                        color: if value {
                            ap::TextColor::Green
                        } else {
                            ap::TextColor::Red
                        },
                    },
                ]);

                true
            }

            #[cfg(debug_assertions)]
            "!setevent" => {
                let Some((flag, value)) = arg.and_then(|a| {
                    let args = regex!(" +").split(a).collect::<Vec<_>>();
                    if args.len() == 2 {
                        Some((u32::from_str(args[0]).ok()?, bool::from_str(args[1]).ok()?))
                    } else {
                        None
                    }
                }) else {
                    arg_error("!setevent EVENT_FLAG BOOL");
                    return true;
                };

                if !set_event_flag(EventFlagId(flag), value) {
                    self.log(RichText::Color {
                        text: "CSEventFlagMan not loaded".into(),
                        color: ap::TextColor::Red,
                    });
                    return true;
                };

                self.log(vec![
                    "Set event ".into(),
                    RichText::Color {
                        text: format!("{:?}", flag),
                        color: ap::TextColor::Blue,
                    },
                    " to ".into(),
                    RichText::Color {
                        text: format!("{:?}", value),
                        color: if value {
                            ap::TextColor::Green
                        } else {
                            ap::TextColor::Red
                        },
                    },
                ]);

                true
            }

            _ => false,
        }
    }

    fn screen_effect(&self) -> Option<shared::ScreenEffect> {
        None
    }
}

impl Core {
    /// Returns an error if there's a conflict between the notion of the current
    /// seed in the server, the save, and/or the config. Also updates the save
    /// data's notion based on whatever is available if it doesn't exist yet.
    fn check_seed_conflict(&mut self) -> Result<()> {
        let client_seed = self.client().map(|c| c.seed_name());
        let save = SaveData::instance();
        let save_seed = save.as_ref().and_then(|s| s.seed.as_ref());

        match (client_seed, save_seed) {
            (Some(client_seed), _) if client_seed != self.seed() => bail!(
                "You've connected to a different Archipelago multiworld than the one that \
                 EldenRingArchipelagoRandomizer.exe used!\n\
                 \n\
		 Connected room seed: {}\n\
                 EldenRingArchipelagoRandomizer.exe seed: {}",
                client_seed,
                self.seed()
            ),
            (Some(client_seed), Some(save_seed)) if client_seed != save_seed => bail!(
                "You've connected to a different Archipelago multiworld than the one that \
                 you used before with this save!\n\
                 \n\
		 Connected room seed: {}\n\
		 Save file seed: {}",
                client_seed,
                save_seed
            ),
            (_, Some(save_seed)) if self.seed() != save_seed => bail!(
                "Your most recent EldenRingArchipelagoRandomizer.exe invocation connected to a \
                 different Archipealgo multiworld than the one that you used before with this \
                 save!\n\
                 \n\
                 EldenRingArchipelagoRandomizer.exe seed: {}\n\
                 Save file seed: {}",
                self.seed(),
                save_seed
            ),
            _ => Ok(()),
        }
    }

    /// Returns an error if [config] expects DLC to be installed.
    fn check_dlc_error(&self) -> Result<()> {
        if self
            .client()
            .is_some_and(|c| c.slot_data().options.enable_dlc)
        {
            bail!(
                "DLC is enabled for this seed, but this Elden Ring Archipelago client milestone \
                 only supports base-game seeds."
            );
        } else {
            Ok(())
        }
    }

    /// Handle new items, distributing them to the player when appropriate. This
    /// also initializes the [SaveData] for a new file.
    fn process_incoming_items(&mut self) {
        let show_item_popups = self.base().show_item_popups();
        let show_progression_item_popups = self.base().show_progression_item_popups();
        let Some(client) = self.client() else {
            return;
        };
        let Ok(item_man) = (unsafe { MapItemMan::instance_mut() }) else {
            return;
        };
        let mut save_data = SaveData::instance_mut();
        let Some(save_data) = save_data.as_mut() else {
            return;
        };

        // Avoid granting multiple received items in one burst, but keep release
        // queues moving at a reasonable pace.
        if self.last_item_time.elapsed() < RECEIVED_ITEM_GRANT_INTERVAL {
            return;
        }

        if let Some(item) = client
            .received_items()
            .iter()
            .find(|item| item.index() >= save_data.items_granted)
        {
            let source_location = item.location();
            let source_location_name = source_location.name().to_string();
            let id_key = I64Key(item.item().id());
            let er_id = client
                .slot_data()
                .ap_ids_to_item_ids
                .get(&id_key)
                .unwrap_or_else(|| {
                    panic!(
                        "Archipelago item {:?} should have an ER ID defined in slot data",
                        item.item()
                    )
                })
                .0;
            let quantity = client
                .slot_data()
                .item_counts
                .get(&id_key)
                .copied()
                .unwrap_or(1);
            let source_display_name = received_item_location_display_name(
                client,
                item,
                source_location,
                &source_location_name,
            );

            info!(
                "Granting {} (AP ID {}, ER ID {:?} from {})",
                item.item().name(),
                item.item().id(),
                er_id,
                source_display_name
            );

            let show_item_popup = show_item_popups
                || (show_progression_item_popups
                    && client
                        .slot_data()
                        .non_deprioritized_progression_item_ids
                        .contains(&id_key.0));
            let companion_goods = companion_region_lock_goods(&item.item().name());
            self.grant_received_item(item_man, er_id, quantity, show_item_popup);
            for goods_id in companion_goods {
                let companion_id = goods_item_id(*goods_id);
                self.grant_received_item(item_man, companion_id, 1, show_item_popup);
            }

            save_data.items_granted += 1;
            self.last_item_time = Instant::now();
        }
    }

    fn grant_received_item(
        &mut self,
        item_man: &mut MapItemMan,
        id: ItemId,
        quantity: u32,
        show_item_popups: bool,
    ) {
        if !show_item_popups && let Err(err) = self.suppress_item_get_display(id) {
            warn!(
                "Failed to suppress item pickup popup for {:?}; granting normally: {:#}",
                id, err
            );
        }

        item_man.grant_item(ItemBufferEntry::new(id, quantity));
    }

    fn suppress_item_get_display(&mut self, id: ItemId) -> Result<()> {
        let restore_at = Instant::now() + ITEM_GET_DISPLAY_SUPPRESSION_DURATION;

        if let Some(restore) = self
            .item_get_display_restores
            .iter_mut()
            .find(|restore| restore.id == id)
        {
            set_item_get_display(id, ItemGetDisplay::suppressed())?;
            restore.restore_at = restore_at;
            return Ok(());
        }

        let previous = set_item_get_display(id, ItemGetDisplay::suppressed())?;
        self.item_get_display_restores.push(ItemGetDisplayRestore {
            id,
            previous,
            restore_at,
        });
        Ok(())
    }

    fn restore_expired_item_get_displays(&mut self) {
        let now = Instant::now();
        let mut index = 0;

        while index < self.item_get_display_restores.len() {
            if self.item_get_display_restores[index].restore_at > now {
                index += 1;
                continue;
            }

            let restore = self.item_get_display_restores.swap_remove(index);
            if let Err(err) = set_item_get_display(restore.id, restore.previous) {
                error!(
                    "Failed to restore item pickup display settings for {:?}: {:#}",
                    restore.id, err
                );
            }
        }
    }

    fn sync_priority_location_markers(&self) {
        let Some(client) = self.client() else {
            return;
        };

        for (location, flag) in &client.slot_data().priority_marker_flags {
            let visible = !client.is_local_location_checked(location.0)
                && client
                    .slot_data()
                    .marker_requirement_met(client, location.0);
            if get_event_flag(*flag) != Some(visible) {
                set_event_flag(*flag, visible);
            }
        }
    }

    fn sync_region_lock_flags(&mut self) {
        let Some(client) = self.client() else {
            return;
        };

        let slot_data = client.slot_data();
        if slot_data.region_lock_item_flags.is_empty() {
            return;
        }

        let received_items = client
            .received_items()
            .iter()
            .map(|received| received.item().id())
            .collect::<HashSet<_>>();

        // Region-lock items obtained from local virtual locations (e.g. "Priority
        // Reserve" / own-world placements) are granted client-side and never enter
        // `received_items`, so include them explicitly. Without this, a lock item
        // found at such a location would never unlock its region.
        let local_virtual_granted: HashSet<i64> = SaveData::instance()
            .map(|save_data| {
                save_data
                    .local_virtual_items_granted
                    .iter()
                    .filter_map(|location_id| slot_data.local_virtual_location_item(*location_id))
                    .collect()
            })
            .unwrap_or_default();

        for (item_id, flag) in &slot_data.region_lock_item_flags {
            let ap_code = item_id.0;
            let owned = received_items.contains(&ap_code)
                || local_virtual_granted.contains(&ap_code)
                || slot_data.has_er_inventory_item(ap_code);

            // Region locks are one-way: only ever SET the flag once the item is owned,
            // never clear it. Clearing on a transient "not owned" (before the AP client
            // has synced, or playing offline) would re-lock an already-opened region.
            if owned && get_event_flag(*flag) != Some(true) {
                set_event_flag(*flag, true);

                if let Some(er_id_key) = slot_data.ap_ids_to_item_ids.get(&I64Key(ap_code)) {
                    let companions = companion_goods_for_er_lock_id(er_id_key.0.param_id());
                    if !companions.is_empty()
                        && let Ok(item_man) = unsafe { MapItemMan::instance_mut() }
                    {
                        for goods_id in companions {
                            let companion_id = goods_item_id(*goods_id);
                            if !has_goods_in_inventory(companion_id) {
                                item_man.grant_item(ItemBufferEntry::new(companion_id, 1));
                            }
                        }
                    }
                }
            }
        }
    }

    /// Removes any placeholder items from the player's inventory and notifies
    /// the server that they've been accessed.
    fn process_inventory_items(&mut self) -> Result<()> {
        let Some(ref mut save_data) = SaveData::instance_mut() else {
            return Ok(());
        };
        let Ok(game_data_man) = (unsafe { GameDataMan::instance_mut() }) else {
            return Ok(());
        };
        let Ok(item_man) = (unsafe { MapItemMan::instance_mut() }) else {
            return Ok(());
        };
        let Some(regulation_manager) = RegulationManager::instance() else {
            return Ok(());
        };

        // We have to make a separate vector here so we aren't borrowing while
        // we make mutations.
        let ids = game_data_man
            .main_player_game_data
            .equipment
            .equip_inventory_data
            .items_data
            .items()
            .map(|e| e.item_id)
            .collect::<Vec<_>>();
        for id in ids {
            if !id.is_archipelago() {
                continue;
            }

            info!("Inventory contains Archipelago item {:?}", id);
            let row = regulation_manager
                .get_equip_param(id)
                .unwrap_or_else(|| panic!("no row defined for Archipelago ID {:?}", id));
            let row = row
                .as_dyn()
                .as_goods()
                .unwrap_or_else(|| panic!("Archipelago ID {:?} should be Goods", id));

            info!("  Archipelago location: {}", row.archipelago_location_id());
            save_data.locations.insert(row.archipelago_location_id());

            if let Some((real_id, quantity)) = row.archipelago_item() {
                info!("  Converting to {}x {:?}", quantity, real_id);
                game_data_man.give_item_directly(real_id, quantity);
            } else {
                // Presumably any item without local item data is a foreign
                // item, but we'll log a bunch of extra data in case there's a
                // bug we need to track down.
                info!(
                    "  Item has no local item data. Basic price: {}, sell value: {}",
                    row.basic_price(),
                    row.sell_value()
                );
            }
            info!("  Removing from inventory");
            game_data_man.remove_item(id, 1);
        }

        if let Some(client) = self.client() {
            client
                .slot_data()
                .expand_virtual_location_checks(&mut save_data.locations);
        }
        self.grant_local_virtual_location_items(item_man, save_data);
        self.notify_foreign_virtual_location_items(item_man, save_data);

        if save_data.locations.len() > self.locations_sent
            && let Some(client) = self.client_mut()
        {
            client.mark_checked(save_data.locations.iter().copied())?;
            self.locations_sent = save_data.locations.len();
        }
        Ok(())
    }

    fn grant_local_virtual_location_items(
        &mut self,
        item_man: &mut MapItemMan,
        save_data: &mut SaveData,
    ) {
        let pending_grants = {
            let Some(client) = self.client() else {
                return;
            };
            let slot_data = client.slot_data();

            save_data
                .locations
                .iter()
                .copied()
                .filter(|location_id| {
                    !save_data
                        .local_virtual_items_granted
                        .contains(location_id)
                })
                .filter_map(|location_id| {
                    let ap_item_id = slot_data.local_virtual_location_item(location_id)?;
                    let Some(er_id) = slot_data.ap_ids_to_item_ids.get(&I64Key(ap_item_id)) else {
                        warn!(
                            "Local virtual location {} contains AP item {}, but slot data has no ER item ID",
                            location_id, ap_item_id
                        );
                        return None;
                    };
                    let quantity = slot_data
                        .item_counts
                        .get(&I64Key(ap_item_id))
                        .copied()
                        .unwrap_or(1);
                    let item_name = client
                        .this_game()
                        .item(ap_item_id)
                        .map(|item| item.name().to_owned())
                        .unwrap_or_else(|| format!("<item #{}>", ap_item_id));
                    let location_name = client
                        .this_game()
                        .location(location_id)
                        .map(|location| location.name().to_owned())
                        .unwrap_or_else(|| format!("<location #{}>", location_id));

                    Some(LocalVirtualItemGrant {
                        location_id,
                        location_name,
                        ap_item_id,
                        item_name,
                        er_id: er_id.0,
                        quantity,
                    })
                })
                .collect::<Vec<_>>()
        };

        for grant in pending_grants {
            info!(
                "Granting local virtual {} (AP ID {}, ER ID {:?} from {})",
                grant.item_name, grant.ap_item_id, grant.er_id, grant.location_name
            );
            item_man.grant_item(ItemBufferEntry::new(grant.er_id, grant.quantity));
            save_data
                .local_virtual_items_granted
                .insert(grant.location_id);
            self.last_item_time = Instant::now();
        }
    }

    /// Shows a pickup pop-up for virtual locations whose item is for another
    /// player. These have no physical world item, so without this the picker
    /// gets no feedback for what they sent. Local-destined virtual items are
    /// handled in [grant_local_virtual_location_items](Self::grant_local_virtual_location_items).
    fn notify_foreign_virtual_location_items(
        &mut self,
        item_man: &mut MapItemMan,
        save_data: &mut SaveData,
    ) {
        let pending_notifications = {
            let Some(client) = self.client() else {
                return;
            };
            let slot_data = client.slot_data();

            save_data
                .locations
                .iter()
                .copied()
                .filter(|location_id| {
                    !save_data
                        .foreign_virtual_items_notified
                        .contains(location_id)
                })
                .filter_map(|location_id| {
                    let display_item = slot_data.virtual_location_display_item(location_id)?;
                    Some((location_id, display_item))
                })
                .collect::<Vec<_>>()
        };

        for (location_id, display_item) in pending_notifications {
            info!(
                "Showing foreign virtual location pickup with {:?} from {}",
                display_item, location_id
            );
            crate::item::show_virtual_location_display_item(item_man, display_item);
            save_data.foreign_virtual_items_notified.insert(location_id);
            self.last_item_time = Instant::now();
        }
    }

    /// Detects when the player has won the game and notifies the server.
    fn handle_goal(&mut self) -> Result<()> {
        if !self.sent_goal
            && let Some(client) = self.client_mut()
            && client
                .slot_data()
                .goal
                .iter()
                .all(|flag| get_event_flag(*flag).unwrap_or(false))
        {
            client.set_status(ap::ClientStatus::Goal)?;
            self.sent_goal = true;
        }

        Ok(())
    }
}
/// A region-lock item and the companion goods it should also grant.
struct RegionLock {
    /// Archipelago item name for the lock.
    name: &'static str,
    /// Elden Ring goods param id for the lock.
    er_param_id: u32,
    /// Goods ids granted alongside the lock.
    companion_goods: &'static [u32],
}

const REGION_LOCKS: &[RegionLock] = &[
    // Dectus Medallion Left, Right
    RegionLock {
        name: "Altus Lock",
        er_param_id: 60003,
        companion_goods: &[8105, 8106],
    },
    // Rold Medallion
    RegionLock {
        name: "Mountaintops Lock",
        er_param_id: 60007,
        companion_goods: &[8107],
    },
    // Haligtree Secret Medallion Left, Right
    RegionLock {
        name: "Consecrated Snowfield Lock",
        er_param_id: 60009,
        companion_goods: &[8175, 8176],
    },
    // Pureblood Knight's Medal
    RegionLock {
        name: "Mohgwyn Lock",
        er_param_id: 60010,
        companion_goods: &[2160],
    },
];

fn companion_region_lock_goods(item_name: &str) -> &'static [u32] {
    REGION_LOCKS
        .iter()
        .find(|lock| lock.name == item_name)
        .map_or(&[], |lock| lock.companion_goods)
}

fn companion_goods_for_er_lock_id(er_item_id: u32) -> &'static [u32] {
    REGION_LOCKS
        .iter()
        .find(|lock| lock.er_param_id == er_item_id)
        .map_or(&[], |lock| lock.companion_goods)
}

fn has_goods_in_inventory(item_id: ItemId) -> bool {
    let Ok(game_data_man) = (unsafe { GameDataMan::instance() }) else {
        return false;
    };
    game_data_man
        .main_player_game_data
        .equipment
        .equip_inventory_data
        .items_data
        .items()
        .any(|entry| entry.item_id == item_id && entry.quantity > 0)
}

fn goods_item_id(goods_id: u32) -> ItemId {
    let encoded = (ItemCategory::Goods as u32)
        .checked_shl(28)
        .and_then(|category| category.checked_add(goods_id))
        .expect("goods item ID overflow");
    ItemId::try_from(encoded).expect("invalid goods item ID")
}

fn get_event_flag(flag: EventFlagId) -> Option<bool> {
    let events = unsafe { CSEventFlagMan::instance() }.ok()?;
    Some(events.virtual_memory_flag.get_flag(u32::from(flag)))
}

fn set_event_flag(flag: EventFlagId, value: bool) -> bool {
    let Ok(events) = (unsafe { CSEventFlagMan::instance_mut() }) else {
        return false;
    };
    events.virtual_memory_flag.set_flag(u32::from(flag), value);
    true
}

#[derive(Clone, Copy)]
struct ItemGetDisplay {
    show_log: bool,
    show_dialog: u8,
}

impl ItemGetDisplay {
    fn suppressed() -> Self {
        Self {
            show_log: true,
            show_dialog: 0,
        }
    }
}

struct ItemGetDisplayRestore {
    id: ItemId,
    previous: ItemGetDisplay,
    restore_at: Instant,
}

fn set_item_get_display(id: ItemId, display: ItemGetDisplay) -> Result<ItemGetDisplay> {
    let row = unsafe { SoloParamRepository::instance_mut() }?
        .get_equip_param_mut(id)
        .with_context(|| format!("no row for item ID {:?}", id))?;

    macro_rules! set {
        ($row:expr) => {{
            let previous = ItemGetDisplay {
                show_log: $row.show_log_cond_type(),
                show_dialog: $row.show_dialog_cond_type(),
            };
            $row.set_show_log_cond_type(display.show_log);
            $row.set_show_dialog_cond_type(display.show_dialog);
            previous
        }};
    }

    Ok(match row {
        EquipParamStructMut::EQUIP_PARAM_ACCESSORY_ST(row) => set!(row),
        EquipParamStructMut::EQUIP_PARAM_GEM_ST(row) => set!(row),
        EquipParamStructMut::EQUIP_PARAM_GOODS_ST(row) => set!(row),
        EquipParamStructMut::EQUIP_PARAM_PROTECTOR_ST(row) => set!(row),
        EquipParamStructMut::EQUIP_PARAM_WEAPON_ST(row) => set!(row),
    })
}

fn received_item_location_display_name(
    client: &ap::Client<SlotData>,
    item: &ap::ReceivedItem,
    source_location: ap::Location,
    source_location_name: &str,
) -> String {
    if item.sender().team() == client.this_player().team()
        && item.sender().slot() == client.this_player().slot()
        && let Some(trigger_id) = client
            .slot_data()
            .virtual_location_trigger(source_location.id())
        && let Some(trigger_location) = client.this_game().location(trigger_id)
    {
        format!("{} [{}]", trigger_location.name(), source_location_name)
    } else {
        source_location_name.to_owned()
    }
}
