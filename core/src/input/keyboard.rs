use std::collections::HashMap;

use crate::KeystrokeData;

#[cfg(target_os = "macos")]
#[path = "keyboard_macos.rs"]
mod platform;

#[cfg(target_os = "windows")]
#[path = "keyboard_windows.rs"]
mod platform;

#[cfg(target_os = "linux")]
#[path = "keyboard_linux.rs"]
mod platform;

pub use platform::{KeyboardEvent, KeyboardLayout};

use super::diagnostics;
use super::key_rules::{plan_key, swapped_case, HeldModifiers, KeyFacts, KeyPlan, ModifierKey};

// See https://github.com/phracker/MacOSX-SDKs/blob/master/MacOSX10.6.sdk/System/Library/Frameworks/Carbon.framework/Versions/A/Frameworks/HIToolbox.framework/Versions/A/Headers/Events.h
pub enum KeyModifier {
    Cmd = 1 << 8,
    Shift = 1 << 9,
    AlphaLock = 1 << 10,
    Option = 1 << 11,
    Ctrl = 1 << 12,
    RightShift = 1 << 13,
    RightOption = 1 << 14,
    RightCtrl = 1 << 15,
}

#[macro_export]
macro_rules! combine_modifiers {
    ($($modifier:ident),*) => {
        {
            let mut result = 0;
            $(
                result |= KeyModifier::$modifier as u32;
            )*
            result
        }
    };
}

pub fn extend_modifier(modifier: u32, new_modifier: KeyModifier) -> u32 {
    modifier | new_modifier as u32
}

pub fn display_mod(modifier: u32) -> String {
    let mut result = String::new();
    if modifier & KeyModifier::Cmd as u32 != 0 {
        result.push_str("Cmd ");
    }
    if modifier & KeyModifier::Shift as u32 != 0 {
        result.push_str("Shift ");
    }
    if modifier & KeyModifier::AlphaLock as u32 != 0 {
        result.push_str("AlphaLock ");
    }
    if modifier & KeyModifier::Option as u32 != 0 {
        result.push_str("Option ");
    }
    if modifier & KeyModifier::Ctrl as u32 != 0 {
        result.push_str("Ctrl ");
    }
    if modifier & KeyModifier::RightShift as u32 != 0 {
        result.push_str("RightShift ");
    }
    if modifier & KeyModifier::RightOption as u32 != 0 {
        result.push_str("RightOption ");
    }
    if modifier & KeyModifier::RightCtrl as u32 != 0 {
        result.push_str("RightCtrl ");
    }
    result
}

pub fn get_modifiers(modifier: u32) -> Vec<KeyModifier> {
    let mut result = Vec::new();
    if modifier & KeyModifier::Cmd as u32 != 0 {
        result.push(KeyModifier::Cmd);
    }
    if modifier & KeyModifier::Shift as u32 != 0 {
        result.push(KeyModifier::Shift);
    }
    if modifier & KeyModifier::AlphaLock as u32 != 0 {
        result.push(KeyModifier::AlphaLock);
    }
    if modifier & KeyModifier::Option as u32 != 0 {
        result.push(KeyModifier::Option);
    }
    if modifier & KeyModifier::Ctrl as u32 != 0 {
        result.push(KeyModifier::Ctrl);
    }
    if modifier & KeyModifier::RightShift as u32 != 0 {
        result.push(KeyModifier::RightShift);
    }
    if modifier & KeyModifier::RightOption as u32 != 0 {
        result.push(KeyModifier::RightOption);
    }
    if modifier & KeyModifier::RightCtrl as u32 != 0 {
        result.push(KeyModifier::RightCtrl);
    }
    result
}

/// Upper bound (exclusive) of the keycodes scanned when building the key map.
/// macOS keycodes are below 128. Windows virtual-key codes for punctuation
/// (VK_OEM_1..VK_OEM_8, VK_OEM_102, VK_OEM_PLUS/COMMA/MINUS/PERIOD: , ; : . - ' ` etc.)
/// are 0xBA..0xE2, so on Windows the whole 0..256 range is scanned (fork fix: these
/// keys were missing, logging "failed to get keycode for key: ,").
#[cfg(target_os = "windows")]
const MAX_KEYCODE: u16 = 256;
#[cfg(not(target_os = "windows"))]
const MAX_KEYCODE: u16 = 128;

/// A key mapping entry that associates a keycode with a key string and modifier combination.
pub struct KeyMapEntry {
    /// The platform-specific keycode that produces this key combination.
    pub keycode: u16,

    /// The resulting key string when this keycode is pressed with the specified modifiers.
    /// For example: "a", "A", "1", "!", etc.
    pub key: String,

    /// The modifier combination (as a bitmask) that was active when this mapping was created.
    /// Uses the `KeyModifier` enum values combined with bitwise OR operations.
    pub modifiers: u32,
}

pub trait KeyboardEventTrait {
    fn override_utf(&mut self, key: &str);
    fn send(&self);
}

/// Defines the interface for platform-specific keyboard layout operations.
///
/// This trait abstracts keyboard layout functionality across different operating systems,
/// providing a unified interface for key translation, layout change detection, and
/// special key mapping. Each platform (macOS, Windows, Linux) implements this trait
/// to handle platform-specific keyboard APIs and behaviors.
///
/// The trait enables the keyboard simulation system to work consistently across platforms
/// while handling the underlying differences in how each OS manages keyboard layouts,
/// input methods, and key translation.
///
/// # Layout Change Detection
///
/// The trait includes functionality to detect when the user changes their keyboard
/// layout (e.g., switching from QWERTY to DVORAK, or changing language layouts).
/// This is crucial for maintaining accurate key mapping tables.
pub trait KeyboardLayoutTrait {
    /// Translates a platform-specific keycode and modifier combination into a Unicode string.
    ///
    /// This is the core method for understanding what character or string a keycode
    /// produces when pressed with specific modifiers. The translation respects the
    /// current keyboard layout, input method, and dead key states.
    ///
    /// # Arguments
    ///
    /// * `keycode` - Platform-specific virtual keycode (typically 0-127 range)
    /// * `modifier` - Bitmask of active modifiers (see `KeyModifier` enum)
    ///
    /// # Returns
    ///
    /// * `Some(String)` - The Unicode string this keycode+modifier produces
    /// * `None` - If the keycode doesn't produce a character, is invalid, or translation fails
    fn key_translate(&self, keycode: u16, modifier: u32) -> Option<String>;

    /// Checks if the keyboard layout has changed since the last call to this method.
    ///
    /// This method detects when the user switches keyboard layouts, input methods,
    /// or language settings. It's essential for maintaining accurate key mapping
    /// because different layouts can produce completely different characters for
    /// the same keycode.
    ///
    /// The method is stateful - it tracks layout changes internally and resets
    /// the "changed" flag after each call, so calling it multiple times in
    /// succession will only return `true` on the first call after a change.
    ///
    /// # State Management
    ///
    /// - **First call after change**: Returns `true` and resets change flag
    /// - **Subsequent calls**: Return `false` until next layout change
    /// - **Thread safety**: Implementation should handle concurrent access appropriately
    ///
    /// # Returns
    ///
    /// * `true` - Layout has changed since last call, key maps should be rebuilt
    /// * `false` - No layout change detected
    fn has_changed(&mut self) -> bool;

    /// Returns a map of layout-independent special keys to their platform-specific keycodes.
    ///
    /// Layout-independent keys are special keys that maintain consistent keycodes
    /// across different keyboard layouts. These include navigation keys, function
    /// keys, and other non-character keys that don't change based on language
    /// or layout selection.
    ///
    /// This map provides a fast lookup for keys that don't need layout-specific
    /// translation, bypassing the more expensive `key_translate` process for
    /// these common keys.
    /// # Returns
    ///
    /// A HashMap mapping standardized key names to platform-specific keycodes.
    /// The keys are static string references for efficiency.
    fn get_independent_codes(&self) -> HashMap<&'static str, u16>;
}

/// A comprehensive key mapping table that translates key strings and modifier combinations
/// to platform-specific keycodes.
///
/// The `KeyMap` struct builds a lookup table during initialization by querying the keyboard
/// layout for all possible keycode and modifier combinations. This allows for efficient
/// reverse lookups when simulating keystrokes - given a key string and modifiers, it can
/// quickly find the corresponding keycode.
///
/// The mapping handles two types of keys:
/// - **Layout-dependent keys**: Regular character keys that vary based on keyboard layout
/// - **Layout-independent keys**: Special keys (arrows, function keys, etc.) that have
///   consistent keycodes across layouts
struct KeyMap {
    /// Vector of all discovered key mappings from the keyboard layout.
    /// Each entry contains a keycode, the resulting key string, and the modifier state.
    entries: Vec<KeyMapEntry>,

    /// Map of layout-independent keys (like arrows, function keys) to their keycodes.
    /// These keys maintain consistent keycodes regardless of the keyboard layout.
    independent_codes: HashMap<&'static str, u16>,
}

impl KeyMap {
    /// Creates a new `KeyMap` by building a comprehensive lookup table from the given keyboard layout.
    ///
    /// This method systematically queries the keyboard layout for all combinations of:
    /// - Keycodes 0-127 (covering the standard key range)
    /// - Common modifier combinations (None, Shift, Option, Cmd, and their combinations)
    ///
    /// The resulting entries are stored for fast reverse lookup during keystroke simulation.
    /// Layout-independent keys are also cached from the keyboard layout.
    ///
    /// # Arguments
    ///
    /// * `layout` - A keyboard layout that implements `KeyboardLayoutTrait`
    ///
    /// # Returns
    ///
    /// A new `KeyMap` instance with populated lookup tables.
    fn new(layout: &impl KeyboardLayoutTrait) -> Self {
        let modifiers = vec![
            0,
            combine_modifiers!(Shift),
            combine_modifiers!(Option),
            combine_modifiers!(Cmd),
            combine_modifiers!(Shift, Option),
            combine_modifiers!(Shift, Cmd),
            combine_modifiers!(Option, Cmd),
        ];
        let mut entries = Vec::new();
        for modifier in modifiers {
            for i in 0..MAX_KEYCODE {
                let result = layout.key_translate(i, modifier);
                if let Some(s) = result {
                    log::debug!(
                        "Keymap entry: code {} key {} modifiers {}",
                        i,
                        s,
                        display_mod(modifier)
                    );
                    entries.push(KeyMapEntry {
                        keycode: i,
                        key: s,
                        modifiers: modifier,
                    });
                }
            }
        }
        let independent_codes = layout.get_independent_codes();
        Self {
            entries,
            independent_codes,
        }
    }

    /// Finds the keycode for a given key string and modifier combination.
    ///
    /// This method performs a reverse lookup to find the keycode that would produce
    /// the specified key when pressed with the given modifiers. It handles both
    /// layout-independent keys (which have fixed keycodes) and layout-dependent
    /// keys (which are looked up in the entries table).
    ///
    /// # Special Handling
    ///
    /// - **Layout-independent keys**: Checked first in the `independent_codes` map
    /// - **Ctrl modifier**: When Ctrl is pressed, it's filtered out during lookup
    ///   because Ctrl+key combinations typically don't produce visible characters
    ///   (except for numbers), making the translation table less useful
    ///
    /// # Arguments
    ///
    /// * `key` - The key string to find (e.g., "a", "A", "Enter", "ArrowUp")
    /// * `modifier` - The modifier combination as a bitmask (see `KeyModifier` enum)
    ///
    /// # Returns
    ///
    /// * `Some(keycode)` - The keycode that produces this key with these modifiers
    /// * `None` - If no matching keycode is found
    pub fn get_code(&self, key: &str, mut modifier: u32) -> Option<u16> {
        let code = self.independent_codes.get(key);
        if code.is_some() {
            return code.cloned();
        }

        /*
         * When the Ctrl key is pressed the key codes are not producing
         * key characters except from numbers. Therefore we disable the
         * modifier when searching as the translation table doesn't provide
         * any useful information.
         */
        if modifier & KeyModifier::Ctrl as u32 != 0 {
            modifier &= !(KeyModifier::Ctrl as u32);
        }

        if let Some(code) = self.find_entry(key, modifier) {
            return Some(code);
        }

        /*
         * fork: with Caps Lock on at the controller a letter arrives in the other
         * case than its Shift state says (Ctrl+C is sent as "C" without Shift).
         * The key is the same one, so look the letter up in its other case.
         */
        if let Some(other_case) = swapped_case(key) {
            if let Some(code) = self.find_entry(&other_case, modifier) {
                return Some(code);
            }
        }

        None
    }

    /// Looks `key` up with exactly these modifiers; on Windows also without Alt.
    fn find_entry(&self, key: &str, modifier: u32) -> Option<u16> {
        for entry in self.entries.iter() {
            if entry.key == key && entry.modifiers == modifier {
                return Some(entry.keycode);
            }
        }

        /*
         * fork (Windows): the Option pass of the map is built with AltGr. For
         * Alt+letter shortcuts (Alt+F, Alt+Tab-like combos) the key is not an
         * AltGr character, so fall back to the same key without the modifier;
         * the Alt key itself is pressed by its own key event.
         */
        #[cfg(target_os = "windows")]
        if modifier & KeyModifier::Option as u32 != 0 {
            let modifier = modifier & !(KeyModifier::Option as u32);
            for entry in self.entries.iter() {
                if entry.key == key && entry.modifiers == modifier {
                    return Some(entry.keycode);
                }
            }
        }

        None
    }

    /// True for the platform's layout-independent keys (Enter, arrows, F-keys...).
    fn is_independent(&self, key: &str) -> bool {
        self.independent_codes.contains_key(key)
    }
}

/// High-level controller for keyboard input simulation across platforms.
///
/// The `KeyboardController` orchestrates keyboard simulation by managing layout detection,
/// key mapping, and event generation. It automatically handles layout changes and provides
/// a simple interface for simulating keystrokes from high-level keystroke data.
///
/// The controller maintains an internal key mapping table that's rebuilt when the keyboard
/// layout changes, ensuring accurate key simulation regardless of the user's current
/// input method or language settings.
///
/// # Type Parameter
///
/// * `T` - The keyboard layout implementation (must implement `KeyboardLayoutTrait`)
///
/// # Features
///
/// - Automatic layout change detection and key map rebuilding
/// - Cross-platform keystroke simulation
/// - Support for modifier keys (Shift, Ctrl, Alt/Option, Cmd/Meta)
/// - Layout-independent special key handling
/// - UTF string override for cross-layout compatibility
pub struct KeyboardController<T: KeyboardLayoutTrait> {
    /// Internal key mapping table for efficient keycode lookups.
    map: KeyMap,
    /// Platform-specific keyboard layout handler.
    layout: T,
    /// Whether keyboard simulation is currently enabled.
    enabled: bool,
    target_process_id: Option<i32>,
    /// fork: modifier keys currently held down here for a controller, so that one
    /// whose release never arrived can be let go (see `HeldModifiers`).
    held_modifiers: HeldModifiers,
}

/// fork: never leave Ctrl / Shift / Alt / Win held down on the sharer's keyboard
/// when the share (and with it this controller) ends in the middle of a shortcut.
impl<T: KeyboardLayoutTrait> Drop for KeyboardController<T> {
    fn drop(&mut self) {
        self.release_all_modifiers("the share ended");
    }
}

impl<T: KeyboardLayoutTrait> KeyboardController<T> {
    /// Creates a new keyboard controller with the default platform layout.
    ///
    /// This initializes the controller with a fresh keyboard layout and builds
    /// the initial key mapping table by querying all possible keycode and
    /// modifier combinations.
    ///
    /// # Returns
    ///
    /// A new `KeyboardController` instance ready for keystroke simulation.
    pub fn new(target_process_id: Option<i32>) -> KeyboardController<KeyboardLayout> {
        let layout = KeyboardLayout::new();
        let map = KeyMap::new(&layout);
        KeyboardController {
            map,
            layout,
            enabled: true,
            target_process_id,
            held_modifiers: HeldModifiers::default(),
        }
    }

    /// Enables or disables keyboard simulation.
    ///
    /// When disabled, calls to `simulate_keystrokes` will be ignored.
    /// This is useful for temporarily suspending keyboard simulation
    /// without destroying the controller state.
    ///
    /// # Arguments
    ///
    /// * `enabled` - `true` to enable simulation, `false` to disable
    pub fn set_enabled(&mut self, enabled: bool) {
        log::info!("set_enabled: {enabled}");
        if !enabled {
            // No further key events will be replayed, so no release would arrive.
            self.release_all_modifiers("remote control was switched off");
        }
        self.enabled = enabled;
    }

    /// fork: presses or releases a named key (a modifier) by its key code.
    fn send_named_key(&self, key: &str, down: bool) {
        let Some(keycode) = self.map.get_code(key, 0) else {
            return;
        };
        if let Some(event) = KeyboardEvent::new(keycode, 0, down, self.target_process_id) {
            event.send();
        }
    }

    fn release_modifiers(&self, modifiers: Vec<ModifierKey>, why: &str) {
        for modifier in modifiers {
            self.send_named_key(modifier.key(), false);
            diagnostics::modifier_released(modifier.key(), why);
        }
    }

    /// fork: lets go of every modifier key held down here for a controller.
    pub fn release_all_modifiers(&mut self, why: &str) {
        let held = self.held_modifiers.take_all();
        self.release_modifiers(held, why);
    }

    /// fork: lets go of modifier keys that are held down here although the
    /// controller is not holding them any more. `ctrl` / `shift` / `alt` / `meta`
    /// are the controller's own modifier state, which comes with every key and
    /// click event. Needed because a controller only forwards key events while its
    /// mouse is over the shared picture: a Ctrl pressed there and released
    /// elsewhere would stay down here (Space became Ctrl+Space, clicks Ctrl+clicks).
    pub fn release_stale_modifiers(&mut self, ctrl: bool, shift: bool, alt: bool, meta: bool) {
        if !self.enabled {
            return;
        }
        let stale = self.held_modifiers.take_stale(ctrl, shift, alt, meta);
        self.release_modifiers(stale, "its release never arrived");
    }

    /// Decides how one remote key event is replayed: as text, by key code, or not
    /// at all. Pure (no key is pressed here), so it is unit tested.
    ///
    /// `text_key_up` is true on platforms where typed text gets a matching text
    /// key-up (Windows).
    fn plan_keystroke(
        &self,
        keystroke_data: &KeystrokeData,
        modifier: u32,
        text_key_up: bool,
    ) -> KeyPlan {
        plan_key(KeyFacts {
            key: &keystroke_data.key,
            down: keystroke_data.down,
            meta: keystroke_data.meta,
            alt: keystroke_data.alt,
            ctrl: keystroke_data.ctrl,
            code: self.map.get_code(&keystroke_data.key, modifier),
            layout_independent: self.map.is_independent(&keystroke_data.key),
            text_key_up,
        })
    }

    /// Simulates a keystroke from high-level keystroke data.
    ///
    /// This method translates the provided keystroke data into platform-specific
    /// key events and sends them to the system. It handles modifier key mapping,
    /// layout change detection, and UTF string override for cross-layout compatibility.
    ///
    /// # Key Processing Steps
    ///
    /// 1. Check if simulation is enabled (early return if disabled)
    /// 2. Convert boolean modifiers to platform modifier bitmask
    /// 3. Detect and handle layout changes (rebuild key map if needed)
    /// 4. Look up keycode for the key + modifier combination
    /// 5. Create platform-specific keyboard event
    /// 6. Override UTF string for layout-independent character input
    /// 7. Send the event to the system
    ///
    /// # UTF Override Logic
    ///
    /// For visible character keys (excluding special keys like arrows, function keys),
    /// the method overrides the platform's character translation with the original
    /// key string. This ensures proper cross-layout compatibility when the sender
    /// and receiver use different keyboard layouts.
    ///
    /// UTF override is skipped when:
    /// - Meta or Ctrl modifiers are active (typically non-character shortcuts)
    /// - Key is layout-independent (Enter, Tab, arrows, etc.)
    /// - Keystroke is a key release event (`down = false`)
    ///
    /// # Arguments
    ///
    /// * `keystroke_data` - High-level keystroke information including key, modifiers, and press state
    ///
    /// In case of error this function simply does nothing because we don't want to
    /// kill the session if a button is not working.
    pub fn simulate_keystrokes(&mut self, keystroke_data: KeystrokeData) {
        log::debug!("simulate_keystrokes: key: {keystroke_data:?}");
        // fork: counted for the rate-limited "remote_input" summary in the log
        // (a count only; the key itself is never written at info level).
        diagnostics::received(diagnostics::Received::Key, None);
        if !self.enabled {
            return;
        }

        let mut modifier = 0;
        if keystroke_data.shift {
            modifier = extend_modifier(modifier, KeyModifier::Shift);
        }
        if keystroke_data.meta {
            modifier = extend_modifier(modifier, KeyModifier::Cmd);
        }
        if keystroke_data.alt {
            modifier = extend_modifier(modifier, KeyModifier::Option);
        }
        if keystroke_data.ctrl {
            modifier = extend_modifier(modifier, KeyModifier::Ctrl);
        }

        let layout_changed = self.layout.has_changed();
        if layout_changed {
            log::info!("simulate_keystrokes: layout changed updating map");
            self.map = KeyMap::new(&self.layout);
        }

        // fork: a modifier whose release never arrived must not turn this key into
        // a shortcut. The modifier keys' own events are left alone here.
        let modifier_key = ModifierKey::from_key(&keystroke_data.key);
        if modifier_key.is_none() {
            self.release_stale_modifiers(
                keystroke_data.ctrl,
                keystroke_data.shift,
                keystroke_data.alt,
                keystroke_data.meta,
            );
        }

        let (keycode, as_text) =
            match self.plan_keystroke(&keystroke_data, modifier, cfg!(target_os = "windows")) {
                KeyPlan::Text(keycode) => (keycode, true),
                KeyPlan::Code(keycode) => (keycode, false),
                KeyPlan::Ignore(reason) => {
                    // Counted and logged rate limited; named keys by name, never a
                    // typed character.
                    diagnostics::key_ignored(reason, &keystroke_data.key);
                    return;
                }
            };

        let event = KeyboardEvent::new(
            keycode,
            modifier,
            keystroke_data.down,
            self.target_process_id,
        );
        if event.is_none() {
            log::error!("simulate_keystrokes: couldn't create keyboard event");
            return;
        }
        let mut event = event.unwrap();

        /*
         * Characters are typed as text (the utf string overrides what the key
         * code would produce) so that it does not matter whether the sharer uses
         * a different keyboard layout from the viewer. Which keys count as text
         * is decided in key_rules::plan_key: never a named key, never a shortcut.
         */
        if as_text {
            event.override_utf(&keystroke_data.key);
        }

        event.send();

        // fork: remember which modifiers are down here for the controller.
        if let Some(modifier_key) = modifier_key {
            self.held_modifiers.set(modifier_key, keystroke_data.down);
        }
    }
}

#[cfg(test)]
mod keyboard_tests {
    use super::*;

    #[test]
    fn test_keyboard_simulator() {
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("debug")).init();
        let _ = KeyboardController::<KeyboardLayout>::new(None);
    }
}

/// fork: tests for how remote key events are planned, against fake keyboard
/// layouts (no OS keyboard API involved, so they run on any platform).
#[cfg(test)]
mod key_planning_tests {
    use super::*;
    use crate::input::diagnostics::KeyIgnored;

    // Key codes of the fake layouts. They mimic Windows virtual-key codes where
    // those are below 128 (the non-Windows scan range); the punctuation keys
    // (0xBA.. on Windows) get stand-in codes below 128.
    const KEY_SPACE: u16 = 0x20;
    const KEY_2: u16 = 0x32;
    const KEY_C: u16 = 0x43;
    const KEY_COMMA: u16 = 0x5C;
    const KEY_PERIOD: u16 = 0x5E;
    const KEY_QUOTE: u16 = 0x5F;
    const KEY_A_UMLAUT: u16 = 0x7B;
    const KEY_ENTER: u16 = 0x0D;
    const KEY_CAPS_LOCK: u16 = 0x14;
    const KEY_INSERT: u16 = 0x2D;
    const KEY_F5: u16 = 0x74;

    const NONE: u32 = 0;
    const SHIFT: u32 = KeyModifier::Shift as u32;
    const ALT_GR: u32 = KeyModifier::Option as u32;

    /// A layout described by (key code, modifiers, produced text) rows.
    struct FakeLayout {
        rows: Vec<(u16, u32, &'static str)>,
        /// Whether " " is listed as a layout-independent key (as on Windows).
        space_is_independent: bool,
    }

    impl KeyboardLayoutTrait for FakeLayout {
        fn key_translate(&self, keycode: u16, modifier: u32) -> Option<String> {
            self.rows
                .iter()
                .find(|(code, modifiers, _)| *code == keycode && *modifiers == modifier)
                .map(|(_, _, text)| text.to_string())
        }

        fn has_changed(&mut self) -> bool {
            false
        }

        fn get_independent_codes(&self) -> HashMap<&'static str, u16> {
            let mut codes = HashMap::new();
            codes.insert("Enter", KEY_ENTER);
            codes.insert("CapsLock", KEY_CAPS_LOCK);
            codes.insert("Insert", KEY_INSERT);
            codes.insert("F5", KEY_F5);
            if self.space_is_independent {
                codes.insert(" ", KEY_SPACE);
            }
            codes
        }
    }

    fn letters() -> Vec<(u16, u32, &'static str)> {
        vec![
            (KEY_C, NONE, "c"),
            (KEY_C, SHIFT, "C"),
            (KEY_SPACE, NONE, " "),
            (KEY_SPACE, SHIFT, " "),
        ]
    }

    /// US QWERTY, the keys that matter here.
    fn us_layout() -> FakeLayout {
        let mut rows = letters();
        rows.extend([
            (KEY_2, NONE, "2"),
            (KEY_2, SHIFT, "@"),
            (KEY_COMMA, NONE, ","),
            (KEY_COMMA, SHIFT, "<"),
            (KEY_PERIOD, NONE, "."),
            (KEY_PERIOD, SHIFT, ">"),
            (KEY_QUOTE, NONE, "'"),
            (KEY_QUOTE, SHIFT, "\""),
        ]);
        FakeLayout {
            rows,
            space_is_independent: true,
        }
    }

    /// Swiss German: ; and : are Shift+comma / Shift+period, @ is AltGr+2,
    /// ä has its own key with à on Shift.
    fn swiss_german_layout() -> FakeLayout {
        let mut rows = letters();
        rows.extend([
            (KEY_2, NONE, "2"),
            (KEY_2, SHIFT, "\""),
            (KEY_2, ALT_GR, "@"),
            (KEY_COMMA, NONE, ","),
            (KEY_COMMA, SHIFT, ";"),
            (KEY_PERIOD, NONE, "."),
            (KEY_PERIOD, SHIFT, ":"),
            (KEY_QUOTE, NONE, "'"),
            (KEY_QUOTE, SHIFT, "?"),
            (KEY_A_UMLAUT, NONE, "ä"),
            (KEY_A_UMLAUT, SHIFT, "à"),
        ]);
        FakeLayout {
            rows,
            space_is_independent: true,
        }
    }

    fn controller(layout: FakeLayout) -> KeyboardController<FakeLayout> {
        KeyboardController {
            map: KeyMap::new(&layout),
            layout,
            enabled: true,
            target_process_id: None,
            held_modifiers: HeldModifiers::default(),
        }
    }

    fn keystroke(key: &str, down: bool) -> KeystrokeData {
        KeystrokeData {
            key: key.to_string(),
            meta: false,
            shift: false,
            ctrl: false,
            alt: false,
            down,
        }
    }

    /// Plans a key the way `simulate_keystrokes` does on Windows.
    fn plan(
        controller: &KeyboardController<FakeLayout>,
        keystroke_data: &KeystrokeData,
    ) -> KeyPlan {
        let mut modifier = 0;
        if keystroke_data.shift {
            modifier = extend_modifier(modifier, KeyModifier::Shift);
        }
        if keystroke_data.meta {
            modifier = extend_modifier(modifier, KeyModifier::Cmd);
        }
        if keystroke_data.alt {
            modifier = extend_modifier(modifier, KeyModifier::Option);
        }
        if keystroke_data.ctrl {
            modifier = extend_modifier(modifier, KeyModifier::Ctrl);
        }
        controller.plan_keystroke(keystroke_data, modifier, true)
    }

    #[test]
    fn a_plain_key_lets_go_of_a_ctrl_whose_release_never_arrived() {
        // "npm" + Space once produced "npm." in a PowerShell terminal: Space with
        // Ctrl still down is Ctrl+Space (complete -> npm.cmd / npm.ps1 -> "npm.").
        let mut controller = controller(us_layout());
        controller.held_modifiers.set(ModifierKey::Control, true);
        // Typing "n" with no modifier reported by the controller.
        controller.simulate_keystrokes(keystroke("n", true));
        assert!(!controller.held_modifiers.is_held(ModifierKey::Control));
    }

    #[test]
    fn a_shortcut_keeps_the_ctrl_the_controller_is_holding() {
        let mut controller = controller(us_layout());
        controller.held_modifiers.set(ModifierKey::Control, true);
        let mut ctrl_c = keystroke("c", true);
        ctrl_c.ctrl = true;
        controller.simulate_keystrokes(ctrl_c);
        assert!(controller.held_modifiers.is_held(ModifierKey::Control));
        // A click that reports no Ctrl (its release got lost) lets go of it.
        controller.release_stale_modifiers(false, false, false, false);
        assert!(!controller.held_modifiers.is_held(ModifierKey::Control));
    }

    #[test]
    fn switching_remote_control_off_lets_go_of_everything() {
        let mut controller = controller(us_layout());
        controller.held_modifiers.set(ModifierKey::Shift, true);
        controller.held_modifiers.set(ModifierKey::Meta, true);
        controller.set_enabled(false);
        assert_eq!(controller.held_modifiers, HeldModifiers::default());
    }

    #[test]
    fn caps_lock_types_nothing() {
        for layout in [us_layout(), swiss_german_layout()] {
            let controller = controller(layout);
            for down in [true, false] {
                assert_eq!(
                    plan(&controller, &keystroke("CapsLock", down)),
                    KeyPlan::Ignore(KeyIgnored::LockKey)
                );
            }
        }
    }

    #[test]
    fn typing_continues_normally_around_caps_lock() {
        // Controller: CapsLock, then the C key (arrives as "C" without Shift),
        // CapsLock again, then C (arrives as "c").
        let controller = controller(us_layout());
        assert_eq!(
            plan(&controller, &keystroke("CapsLock", true)),
            KeyPlan::Ignore(KeyIgnored::LockKey)
        );
        // "C" without Shift is not in the layout as such; the other-case lookup
        // still finds the C key, and it is typed as the text "C".
        assert_eq!(
            plan(&controller, &keystroke("C", true)),
            KeyPlan::Text(KEY_C)
        );
        assert_eq!(
            plan(&controller, &keystroke("c", true)),
            KeyPlan::Text(KEY_C)
        );
    }

    #[test]
    fn ctrl_c_works_with_caps_lock_on_at_the_controller() {
        let controller = controller(us_layout());
        let mut ctrl_c = keystroke("C", true);
        ctrl_c.ctrl = true;
        assert_eq!(plan(&controller, &ctrl_c), KeyPlan::Code(KEY_C));
    }

    #[test]
    fn space_is_pressed_as_the_space_key() {
        for layout in [us_layout(), swiss_german_layout()] {
            let controller = controller(layout);
            // Single, double and shifted presses all plan the same real key.
            let mut events = Vec::new();
            for _ in 0..2 {
                events.push(plan(&controller, &keystroke(" ", true)));
                events.push(plan(&controller, &keystroke(" ", false)));
            }
            let mut shift_space = keystroke(" ", true);
            shift_space.shift = true;
            events.push(plan(&controller, &shift_space));
            assert_eq!(events, vec![KeyPlan::Code(KEY_SPACE); 5]);
        }
    }

    #[test]
    fn space_never_maps_to_the_period_key() {
        for layout in [us_layout(), swiss_german_layout()] {
            let controller = controller(layout);
            for modifiers in 0..16 {
                let mut space = keystroke(" ", true);
                space.shift = modifiers & 1 != 0;
                space.ctrl = modifiers & 2 != 0;
                space.alt = modifiers & 4 != 0;
                space.meta = modifiers & 8 != 0;
                assert_eq!(plan(&controller, &space), KeyPlan::Code(KEY_SPACE));
            }
        }
    }

    #[test]
    fn space_stays_text_where_the_platform_does_not_list_it() {
        let mut layout = us_layout();
        layout.space_is_independent = false;
        let controller = controller(layout);
        assert_eq!(
            plan(&controller, &keystroke(" ", true)),
            KeyPlan::Text(KEY_SPACE)
        );
    }

    #[test]
    fn named_keys_are_pressed_by_code_or_ignored() {
        let controller = controller(us_layout());
        assert_eq!(
            plan(&controller, &keystroke("Enter", true)),
            KeyPlan::Code(KEY_ENTER)
        );
        assert_eq!(
            plan(&controller, &keystroke("Insert", true)),
            KeyPlan::Code(KEY_INSERT)
        );
        assert_eq!(
            plan(&controller, &keystroke("F5", false)),
            KeyPlan::Code(KEY_F5)
        );
        for key in [
            "Unidentified",
            "Dead",
            "Process",
            "AudioVolumeUp",
            "MediaPlayPause",
            "PrintScreen",
            "ContextMenu",
            "Pause",
            "Hyper",
        ] {
            for down in [true, false] {
                assert_eq!(
                    plan(&controller, &keystroke(key, down)),
                    KeyPlan::Ignore(KeyIgnored::UnknownNamedKey),
                    "{key}"
                );
            }
        }
    }

    #[test]
    fn punctuation_is_typed_as_text_on_both_layouts() {
        for key in [",", ";", ":", ".", "'", "\"", "?", "ä", "à", "@", "<", ">"] {
            for layout in [us_layout(), swiss_german_layout()] {
                let controller = controller(layout);
                let planned = plan(&controller, &keystroke(key, true));
                assert!(
                    matches!(planned, KeyPlan::Text(_)),
                    "{key:?} must be typed as text, got {planned:?}"
                );
            }
        }
    }

    #[test]
    fn swiss_german_alt_gr_character_uses_its_key() {
        // A Windows controller holding AltGr reports Ctrl+Alt.
        let controller = controller(swiss_german_layout());
        let mut at_sign = keystroke("@", true);
        at_sign.alt = true;
        at_sign.ctrl = true;
        assert_eq!(plan(&controller, &at_sign), KeyPlan::Code(KEY_2));
    }

    #[test]
    fn shifted_punctuation_shortcut_finds_the_right_key() {
        // Ctrl+Shift+; on Swiss German is the comma key.
        let controller = controller(swiss_german_layout());
        let mut shortcut = keystroke(";", true);
        shortcut.ctrl = true;
        shortcut.shift = true;
        assert_eq!(plan(&controller, &shortcut), KeyPlan::Code(KEY_COMMA));
        // Ctrl+. is the period key on both.
        let mut shortcut = keystroke(".", true);
        shortcut.ctrl = true;
        assert_eq!(plan(&controller, &shortcut), KeyPlan::Code(KEY_PERIOD));
    }

    #[test]
    fn shortcut_with_a_character_the_layout_lacks_presses_nothing() {
        let controller = controller(us_layout());
        let mut shortcut = keystroke("ä", true);
        shortcut.ctrl = true;
        assert_eq!(
            plan(&controller, &shortcut),
            KeyPlan::Ignore(KeyIgnored::ShortcutWithoutKey)
        );
    }
}
