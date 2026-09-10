use std::time::Duration;

use anyhow::Result;
use eldenring::{
    cs::{CSFeManHudState, CSFeManImp, CSTaskGroupIndex, CSTaskImp, MapItemMan},
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
        // If MapItemMan isn't available, that usually means we're on the
        // main menu. There's probably a better way to detect that but we
        // don't know it yet.
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
}

pub struct EldenRingInputBlocker(pub &'static eldenring_extra::input::InputBlocker);

impl shared::InputBlocker for EldenRingInputBlocker {
    fn block_only(&self, input_flags: shared::InputFlags) {
        self.0
            .block_only(eldenring_extra::input::InputFlags::from_bits(input_flags.bits()).unwrap())
    }
}
