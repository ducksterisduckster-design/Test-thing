use std::time::Duration;

use anyhow::Result;
use eldenring::{
    cs::{CSFeManHudState, CSFeManImp, CSTaskGroupIndex, CSTaskImp, MapItemMan, PlayerIns},
    fd4::FD4TaskData,
};
use fromsoftware_shared::{FromStatic, SharedTaskImpExt};

pub struct EldenRing;

impl shared::Game for EldenRing {
    type Core = crate::core::Core;
    type GraphicsHooks = hudhook::hooks::dx12::ImguiDx12Hooks;
    type InputBlocker = EldenRingInputBlocker;
    const TYPE: shared::GameType = shared::GameType::EldenRing;
    const CLIENT_VERSION: &str = env!("CARGO_PKG_VERSION");
    const SUPPORTS_ITEM_POPUP_SETTING: bool = true;

    fn run_recurring_task(mut task: impl FnMut() + 'static + Send) -> Result<()> {
        CSTaskImp::wait_for_instance(Duration::MAX)?.run_recurring(
            move |_: &'_ FD4TaskData| task(),
            CSTaskGroupIndex::FrameBegin,
        );
        Ok(())
    }

    unsafe fn is_main_menu() -> bool {
        // If MapItemMan doesn't exist, we're probably on the main menu. There
        // may be a better check, but we haven't found one yet.
        unsafe { MapItemMan::instance() }.is_err()
    }

    unsafe fn is_menu_open() -> bool {
        if unsafe { Self::is_main_menu() } {
            return true;
        }

        unsafe { CSFeManImp::instance() }.is_ok_and(|fe_man| {
            matches!(
                fe_man.hud_state,
                CSFeManHudState::ShowAll | CSFeManHudState::PopupMenu
            )
        })
    }

    unsafe fn kill_player() {
        if let Ok(player) = unsafe { PlayerIns::local_player_mut() } {
            player.chr_ins.modules.data.hp = 0;
        }
    }
}

impl EldenRing {
    /// Whether the local player is dead. Returns `false` if there's no player
    /// loaded (like on the main menu).
    ///
    /// ## Safety
    ///
    /// Call this on the main thread, with no other references to the game's
    /// internal state alive.
    pub unsafe fn is_player_dead() -> bool {
        unsafe { PlayerIns::local_player() }
            .is_ok_and(|player| player.chr_ins.chr_flags1c5.death_flag())
    }
}

pub struct EldenRingInputBlocker(pub &'static eldenring_extra::input::InputBlocker);

impl shared::InputBlocker for EldenRingInputBlocker {
    fn block_only(&self, input_flags: shared::InputFlags) {
        self.0
            .block_only(eldenring_extra::input::InputFlags::from_bits(input_flags.bits()).unwrap())
    }
}
