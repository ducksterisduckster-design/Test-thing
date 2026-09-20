use std::collections::HashSet;
use std::sync::{LazyLock, RwLock, RwLockReadGuard, RwLockWriteGuard};

use bincode::{Decode, Encode};
use eldenring::cs::MapItemMan;
use eldenring_extra::save;
use fromsoftware_shared::FromStatic;
use log::*;

/// The shared save data. It holds defaults until a save is loaded or it's set
/// directly.
static INSTANCE: LazyLock<RwLock<SaveData>> = LazyLock::new(|| RwLock::new(Default::default()));

/// How the save data is encoded to bytes.
const CONFIG: bincode::config::Configuration = bincode::config::standard();

/// Data that's saved and loaded along with the player's game save.
#[derive(Debug, Decode, Encode, Default)]
pub struct SaveData {
    /// How many Archipelago items from other worlds have been given to the
    /// player in this run.
    pub items_granted: usize,

    /// Locations the player has already hit in this game. We don't strictly
    /// need it, but it keeps us from spamming the server.
    pub locations: HashSet<i64>,

    /// The seed this save was last connected to. Lets us catch someone loading
    /// a save while connected to the wrong multiworld.
    pub seed: Option<String>,

    /// Locations whose own-item grant has already been given: virtual ones,
    /// plus real ones for the by-location grant. Also holds `-1` once the save
    /// has been baselined for that.
    pub local_virtual_items_granted: HashSet<i64>,

    /// Virtual locations whose "sent to someone else" pop-up has already been
    /// shown, so we don't show it twice.
    pub foreign_virtual_items_notified: HashSet<i64>,
}

impl SaveData {
    /// Sets up the hooks for saving and loading. They're never removed.
    ///
    /// Safety: follow ilhook's safety rules.
    pub unsafe fn hook() {
        unsafe {
            std::mem::forget(save::on_save_load(
                || {
                    Self::instance().and_then(|data| match bincode::encode_to_vec(&*data, CONFIG) {
                        Ok(bytes) => Some(bytes),
                        Err(err) => {
                            warn!("Failed to encode save data: {}", err);
                            None
                        }
                    })
                },
                |load_type| {
                    use save::OnLoadType::*;
                    let bytes = match load_type {
                        SavedData(bytes) => bytes,
                        MainMenu => {
                            // Back on the main menu: reset the granted count
                            // and seed, so a new file starts fresh with no seed
                            // conflict.
                            let mut save = INSTANCE.write().unwrap();
                            save.items_granted = 0;
                            save.seed = None;
                            return;
                        }
                        _ => return,
                    };

                    match decode_exact::<SaveData>(&bytes) {
                        Ok(data) => *INSTANCE.write().unwrap() = data,
                        Err(err) => warn!("Failed to load save data: {}", err),
                    }
                },
            ));
        }
    }

    /// Read-only access to the [SaveData] singleton, or `None` if the player
    /// isn't in a game.
    pub fn instance<'a>() -> Option<RwLockReadGuard<'a, Self>> {
        // MapItemMan only exists once the player is in a game, not on the main
        // menu. That's a better test than "is a save loaded", since a new game
        // has no save file yet.
        //
        // Safety: we never use the man, we only check that it exists.
        if unsafe { MapItemMan::instance() }.is_ok() {
            Some(INSTANCE.read().unwrap())
        } else {
            None
        }
    }

    /// Write access to the [SaveData] singleton, or `None` if the player isn't
    /// in a game.
    pub fn instance_mut<'a>() -> Option<RwLockWriteGuard<'a, Self>> {
        // Same as above.
        if unsafe { MapItemMan::instance() }.is_ok() {
            Some(INSTANCE.write().unwrap())
        } else {
            None
        }
    }
}

fn decode_exact<T>(bytes: &[u8]) -> Result<T, String>
where
    T: Decode<()>,
{
    match bincode::decode_from_slice::<T, _>(bytes, CONFIG) {
        Ok((data, size)) if size == bytes.len() => Ok(data),
        Ok((_, size)) => Err(format!(
            "Archipelago save data had {} extra bytes; this probably means \
             that you tried to load a save file created by a different \
             version of the Archipelago mod, or by a different mod entirely",
            bytes.len() - size
        )),
        Err(err) => Err(err.to_string()),
    }
}
