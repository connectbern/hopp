//! Windows only: write the laptop lid and screen on/off state into the log.
//!
//! When a laptop lid is closed (even with the lid action set to "Do nothing") Windows
//! usually switches the console display off. The desktop compositor then stops
//! producing new frames, so screen capture keeps delivering the last picture without
//! any error and the viewers see a frozen screen. These lines make that visible in
//! hopp.log instead of leaving a silent gap.

use std::sync::atomic::{AtomicBool, Ordering};

use windows::Win32::System::Power::{
    RegisterPowerSettingNotification, DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS, POWERBROADCAST_SETTING,
};
use windows::Win32::System::SystemServices::{
    GUID_CONSOLE_DISPLAY_STATE, GUID_LIDSWITCH_STATE_CHANGE,
};
use windows::Win32::UI::WindowsAndMessaging::{DEVICE_NOTIFY_CALLBACK, PBT_POWERSETTINGCHANGE};

static SCREEN_OFF: AtomicBool = AtomicBool::new(false);

/// True while Windows reports the console screen as off (e.g. lid closed).
pub fn screen_is_off() -> bool {
    SCREEN_OFF.load(Ordering::Relaxed)
}

unsafe extern "system" fn on_power_setting(
    _context: *const core::ffi::c_void,
    kind: u32,
    setting: *const core::ffi::c_void,
) -> u32 {
    if kind != PBT_POWERSETTINGCHANGE || setting.is_null() {
        return 0;
    }
    let setting = &*(setting as *const POWERBROADCAST_SETTING);
    if setting.DataLength < 4 {
        return 0;
    }
    let value = std::ptr::read_unaligned(setting.Data.as_ptr() as *const u32);
    if setting.PowerSetting == GUID_CONSOLE_DISPLAY_STATE {
        SCREEN_OFF.store(value == 0, Ordering::Relaxed);
        match value {
            0 => log::warn!(
                "display_power: screen OFF (Windows stopped drawing it; a shared screen freezes until it is back on)"
            ),
            1 => log::info!("display_power: screen ON"),
            2 => log::info!("display_power: screen DIMMED"),
            v => log::info!("display_power: screen state {v}"),
        }
    } else if setting.PowerSetting == GUID_LIDSWITCH_STATE_CHANGE {
        match value {
            0 => log::warn!("display_power: lid CLOSED"),
            1 => log::info!("display_power: lid OPEN"),
            v => log::info!("display_power: lid state {v}"),
        }
    }
    0
}

/// Registers for screen and lid notifications for the lifetime of the process.
/// Windows sends the current state right away, so the log starts with a baseline.
pub fn start_logging() {
    // The subscription parameters must stay valid while registered: leak them.
    let params: &'static mut DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS =
        Box::leak(Box::new(DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS {
            Callback: Some(on_power_setting),
            Context: std::ptr::null_mut(),
        }));
    let recipient = windows::Win32::Foundation::HANDLE(
        params as *mut DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS as *mut core::ffi::c_void,
    );
    for (name, guid) in [
        ("screen", &GUID_CONSOLE_DISPLAY_STATE),
        ("lid", &GUID_LIDSWITCH_STATE_CHANGE),
    ] {
        match unsafe { RegisterPowerSettingNotification(recipient, guid, DEVICE_NOTIFY_CALLBACK) } {
            // Never unregistered: the handle lives as long as hopp_core.
            Ok(_handle) => log::info!("display_power: watching {name} state"),
            Err(e) => log::warn!("display_power: cannot watch {name} state: {e:?}"),
        }
    }
}
