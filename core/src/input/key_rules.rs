//! fork: decides how one remote key event is replayed on the sharer's machine.
//!
//! A controller sends each key as a string: either the character the key produces
//! ("a", "A", ",", "é", " ") or the *name* of a non-printing key ("Enter",
//! "CapsLock", "F5", "AudioVolumeUp", "Unidentified", ...; these are winit's
//! `NamedKey` names, the same vocabulary as the web's `KeyboardEvent.key`).
//!
//! Characters are typed as text, so the sharer's keyboard layout does not matter.
//! The bug this module fixes: every key that was not on a short hard-coded list was
//! treated as text, so the *name* of a key was typed. On Windows only the first
//! UTF-16 unit of the string is sent, hence CapsLock typed "C", Insert "I",
//! NumLock "N", ContextMenu "C", Unidentified "U", and so on.
//!
//! The rule now: **a key name is never typed as text.** A named key is pressed by
//! its key code if the platform has one, otherwise it is ignored (and counted in
//! the log by name).
//!
//! This file is pure logic without platform dependencies and is unit tested.

use super::diagnostics::KeyIgnored;

/// What to do with one remote key event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyPlan {
    /// Type the key string as text. The key code (0 when unknown) is only the
    /// carrier event; the text overrides what it would produce.
    Text(u16),
    /// Press or release this platform key code.
    Code(u16),
    /// Do nothing.
    Ignore(KeyIgnored),
}

/// The facts `plan_key` needs about one remote key event.
#[derive(Debug, Clone, Copy)]
pub struct KeyFacts<'a> {
    /// The key string sent by the controller.
    pub key: &'a str,
    pub down: bool,
    pub meta: bool,
    pub alt: bool,
    pub ctrl: bool,
    /// Key code found for this key + modifiers on the sharer's layout, if any.
    pub code: Option<u16>,
    /// True when the key is in the platform's list of layout-independent keys.
    pub layout_independent: bool,
    /// True when the platform types text with a matching text key-up (Windows).
    pub text_key_up: bool,
}

/// True for the name of a non-printing key ("Enter", "CapsLock", "F12",
/// "AudioVolumeUp", "Unidentified", "Dead", "Process", ...), false for text a key
/// produces ("a", "A", "é", "ß", ",", " ", "^").
///
/// Key names are ASCII identifiers: an uppercase letter followed by at least one
/// more letter or digit. A printable key produces a single character (or, with a
/// composing layout, a short non-ASCII string), which never matches this.
pub fn is_named_key(key: &str) -> bool {
    let mut chars = key.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !first.is_ascii_uppercase() {
        return false;
    }
    let mut rest_len = 0;
    for character in chars {
        if !character.is_ascii_alphanumeric() {
            return false;
        }
        rest_len += 1;
    }
    rest_len >= 1
}

/// CapsLock, NumLock and ScrollLock are not replayed on the sharer.
///
/// Remote text arrives as the final characters ("A" or "a" as the controller's
/// own Caps Lock produced them) and is typed as text, which does not depend on
/// the sharer's lock state. Toggling the sharer's Caps Lock as well would add
/// nothing for the controller, but it would silently flip the lock for the
/// person sitting at the shared machine, and because it is a *toggle* the two
/// machines could end up in opposite states. So the lock keys stay local to
/// each keyboard.
pub fn is_lock_key(key: &str) -> bool {
    matches!(key, "CapsLock" | "NumLock" | "ScrollLock")
}

/// For a single cased letter, the same letter in the other case ("c" <-> "C").
///
/// Used when a shortcut arrives while the controller's Caps Lock is on: Ctrl+C is
/// then sent as key "C" without Shift, which is not in the layout table as such.
pub fn swapped_case(key: &str) -> Option<String> {
    let mut chars = key.chars();
    let first = chars.next()?;
    if chars.next().is_some() {
        return None;
    }
    let swapped: String = if first.is_lowercase() {
        first.to_uppercase().collect()
    } else if first.is_uppercase() {
        first.to_lowercase().collect()
    } else {
        return None;
    };
    // Letters whose other case is more than one character (ß -> SS) do not count.
    (swapped.chars().count() == 1 && swapped != key).then_some(swapped)
}

/// A modifier key the controller can hold down.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModifierKey {
    Control,
    Shift,
    Alt,
    /// Right Alt on layouts where it is AltGr.
    AltGraph,
    Meta,
}

impl ModifierKey {
    pub const ALL: [ModifierKey; 5] = [
        ModifierKey::Control,
        ModifierKey::Shift,
        ModifierKey::Alt,
        ModifierKey::AltGraph,
        ModifierKey::Meta,
    ];

    /// The modifier a key string names, if it names one.
    pub fn from_key(key: &str) -> Option<Self> {
        match key {
            "Control" => Some(ModifierKey::Control),
            "Shift" => Some(ModifierKey::Shift),
            "Alt" => Some(ModifierKey::Alt),
            "AltGraph" => Some(ModifierKey::AltGraph),
            "Meta" | "Super" => Some(ModifierKey::Meta),
            _ => None,
        }
    }

    /// The key string that presses or releases this modifier.
    pub fn key(self) -> &'static str {
        match self {
            ModifierKey::Control => "Control",
            ModifierKey::Shift => "Shift",
            ModifierKey::Alt => "Alt",
            ModifierKey::AltGraph => "AltGraph",
            ModifierKey::Meta => "Meta",
        }
    }
}

/// The modifier keys that are held down on the sharer's machine because a
/// controller pressed them.
///
/// A controller only forwards key events while its mouse is over the shared
/// picture. If it presses Ctrl there and releases it somewhere else, the release
/// never arrives and Ctrl stays down on the sharer: the next Space is Ctrl+Space
/// (in a PowerShell terminal that completes `npm` to `npm.`), the next click is a
/// Ctrl+click, and the sharer's own typing turns into shortcuts.
///
/// Every key and click event carries the controller's real modifier state, so
/// the sharer can notice the disagreement and let go of the key.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct HeldModifiers {
    control: bool,
    shift: bool,
    alt: bool,
    alt_graph: bool,
    meta: bool,
}

impl HeldModifiers {
    /// Records that a modifier key was pressed or released for a controller.
    pub fn set(&mut self, modifier: ModifierKey, down: bool) {
        match modifier {
            ModifierKey::Control => self.control = down,
            ModifierKey::Shift => self.shift = down,
            ModifierKey::Alt => self.alt = down,
            ModifierKey::AltGraph => self.alt_graph = down,
            ModifierKey::Meta => self.meta = down,
        }
    }

    pub fn is_held(&self, modifier: ModifierKey) -> bool {
        match modifier {
            ModifierKey::Control => self.control,
            ModifierKey::Shift => self.shift,
            ModifierKey::Alt => self.alt,
            ModifierKey::AltGraph => self.alt_graph,
            ModifierKey::Meta => self.meta,
        }
    }

    /// Takes every modifier that is still held (for "release everything").
    pub fn take_all(&mut self) -> Vec<ModifierKey> {
        let held = ModifierKey::ALL
            .into_iter()
            .filter(|modifier| self.is_held(*modifier))
            .collect();
        *self = HeldModifiers::default();
        held
    }

    /// Takes the modifiers that are held here although the controller says, with
    /// the flags of a later key or click event, that it is not holding them.
    ///
    /// AltGr shows up in the flags as Alt (a Windows controller reports AltGr as
    /// Ctrl+Alt), so it counts as released when Alt is not reported.
    pub fn take_stale(
        &mut self,
        ctrl: bool,
        shift: bool,
        alt: bool,
        meta: bool,
    ) -> Vec<ModifierKey> {
        let mut stale = Vec::new();
        for modifier in ModifierKey::ALL {
            let reported_down = match modifier {
                ModifierKey::Control => ctrl,
                ModifierKey::Shift => shift,
                ModifierKey::Alt | ModifierKey::AltGraph => alt,
                ModifierKey::Meta => meta,
            };
            if self.is_held(modifier) && !reported_down {
                self.set(modifier, false);
                stale.push(modifier);
            }
        }
        stale
    }
}

/// Decides how a remote key event is replayed.
pub fn plan_key(facts: KeyFacts) -> KeyPlan {
    let KeyFacts {
        key,
        down,
        meta,
        alt,
        ctrl,
        code,
        layout_independent,
        text_key_up,
    } = facts;

    if key.is_empty() {
        return KeyPlan::Ignore(KeyIgnored::Empty);
    }
    if is_lock_key(key) {
        return KeyPlan::Ignore(KeyIgnored::LockKey);
    }

    // Non-printing keys: by key code or not at all. Never as text.
    if layout_independent || is_named_key(key) {
        return match code {
            Some(code) => KeyPlan::Code(code),
            None => KeyPlan::Ignore(KeyIgnored::UnknownNamedKey),
        };
    }

    // Shortcuts (Ctrl / Alt / Cmd + character) insert no visible character: they
    // need the real key. Without one there is nothing sensible to press (key
    // code 0 is not "no key": on macOS it is the A key).
    if meta || alt || ctrl {
        return match code {
            Some(code) => KeyPlan::Code(code),
            None => KeyPlan::Ignore(KeyIgnored::ShortcutWithoutKey),
        };
    }

    // Text. The press types the character; the release is a text key-up where the
    // platform pairs them (single characters only), else a plain key-up.
    if down || (text_key_up && key.chars().count() == 1) {
        KeyPlan::Text(code.unwrap_or(0))
    } else {
        KeyPlan::Code(code.unwrap_or(0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts(key: &str) -> KeyFacts<'_> {
        KeyFacts {
            key,
            down: true,
            meta: false,
            alt: false,
            ctrl: false,
            code: None,
            layout_independent: false,
            text_key_up: true,
        }
    }

    /// Every winit `NamedKey` / web `KeyboardEvent.key` name a controller can
    /// plausibly send for a non-printing key.
    const NAMED_KEYS: &[&str] = &[
        "CapsLock",
        "NumLock",
        "ScrollLock",
        "Shift",
        "Control",
        "Alt",
        "AltGraph",
        "Meta",
        "Super",
        "Hyper",
        "Fn",
        "FnLock",
        "Symbol",
        "SymbolLock",
        "Dead",
        "Unidentified",
        "Process",
        "Compose",
        "Enter",
        "Tab",
        "Space",
        "Backspace",
        "Escape",
        "Delete",
        "Insert",
        "Home",
        "End",
        "PageUp",
        "PageDown",
        "ArrowUp",
        "ArrowDown",
        "ArrowLeft",
        "ArrowRight",
        "ContextMenu",
        "PrintScreen",
        "Pause",
        "Clear",
        "Help",
        "F1",
        "F12",
        "F13",
        "F24",
        "F35",
        "AudioVolumeUp",
        "AudioVolumeDown",
        "AudioVolumeMute",
        "MediaPlayPause",
        "MediaTrackNext",
        "MediaTrackPrevious",
        "MediaStop",
        "BrowserBack",
        "LaunchMail",
        "Convert",
        "NonConvert",
        "KanaMode",
        "HangulMode",
        "Eisu",
        "ZenkakuHankaku",
        "Power",
        "Sleep",
        "WakeUp",
        "Soft1",
        "TV3DMode",
    ];

    const TEXT_KEYS: &[&str] = &[
        "a", "A", "z", "Z", "0", "9", " ", ".", ",", ";", ":", "'", "\"", "-", "_", "@", "#", "\\",
        "|", "[", "]", "{", "}", "~", "^", "`", "´", "¨", "ä", "ö", "ü", "Ä", "Ö", "Ü", "é", "è",
        "à", "ç", "ß", "€", "£", "§", "°", "א", "ש", "я", "Я", "中", "😀", "\u{a0}",
    ];

    #[test]
    fn names_are_recognised_as_names() {
        for key in NAMED_KEYS {
            assert!(is_named_key(key), "{key} must count as a key name");
        }
    }

    #[test]
    fn characters_are_not_names() {
        for key in TEXT_KEYS {
            assert!(!is_named_key(key), "{key:?} must count as text");
        }
        assert!(!is_named_key(""));
        // A dead key followed by a letter it cannot combine with: two characters.
        assert!(!is_named_key("^q"));
        assert!(!is_named_key("´s"));
    }

    #[test]
    fn no_key_name_is_ever_typed_as_text() {
        // Whatever the modifiers, direction or lookup result: a name never
        // becomes KeyPlan::Text. This is the "CapsLock types C" class of bug.
        for key in NAMED_KEYS {
            for down in [true, false] {
                for code in [None, Some(0x14)] {
                    for layout_independent in [true, false] {
                        for modifiers in 0..8 {
                            for text_key_up in [true, false] {
                                let plan = plan_key(KeyFacts {
                                    key,
                                    down,
                                    meta: modifiers & 1 != 0,
                                    alt: modifiers & 2 != 0,
                                    ctrl: modifiers & 4 != 0,
                                    code,
                                    layout_independent,
                                    text_key_up,
                                });
                                assert!(
                                    !matches!(plan, KeyPlan::Text(_)),
                                    "{key} was planned as text: {plan:?}"
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn caps_lock_is_not_replayed_and_types_nothing() {
        // Reported: CapsLock on the controller typed "C" on the shared screen.
        for down in [true, false] {
            let plan = plan_key(KeyFacts {
                down,
                code: Some(0x14), // VK_CAPITAL is known on Windows...
                layout_independent: true,
                ..facts("CapsLock")
            });
            // ...and still it is neither typed nor toggled.
            assert_eq!(plan, KeyPlan::Ignore(KeyIgnored::LockKey));
        }
        assert_eq!(
            plan_key(facts("NumLock")),
            KeyPlan::Ignore(KeyIgnored::LockKey)
        );
        assert_eq!(
            plan_key(facts("ScrollLock")),
            KeyPlan::Ignore(KeyIgnored::LockKey)
        );
    }

    #[test]
    fn named_keys_use_their_key_code() {
        let plan = plan_key(KeyFacts {
            code: Some(0x2D),
            layout_independent: true,
            ..facts("Insert")
        });
        assert_eq!(plan, KeyPlan::Code(0x2D));
        // Release too.
        let plan = plan_key(KeyFacts {
            down: false,
            code: Some(0x74),
            layout_independent: true,
            ..facts("F5")
        });
        assert_eq!(plan, KeyPlan::Code(0x74));
    }

    #[test]
    fn unknown_named_keys_are_dropped() {
        for key in [
            "AudioVolumeUp",
            "Unidentified",
            "Dead",
            "Process",
            "Hyper",
            "F35",
        ] {
            assert_eq!(
                plan_key(facts(key)),
                KeyPlan::Ignore(KeyIgnored::UnknownNamedKey),
                "{key}"
            );
        }
    }

    #[test]
    fn space_is_a_real_key_where_the_platform_lists_it() {
        // Windows lists " " as a layout-independent key (VK_SPACE = 0x20): Space
        // is then pressed as the real key, down and up, exactly like the
        // sharer's own space bar - not injected as text.
        for down in [true, false] {
            let plan = plan_key(KeyFacts {
                down,
                code: Some(0x20),
                layout_independent: true,
                ..facts(" ")
            });
            assert_eq!(plan, KeyPlan::Code(0x20));
        }
        // Two quick presses are two identical, independent key presses.
        let press = |down| {
            plan_key(KeyFacts {
                down,
                code: Some(0x20),
                layout_independent: true,
                ..facts(" ")
            })
        };
        assert_eq!(
            [press(true), press(false), press(true), press(false)],
            [KeyPlan::Code(0x20); 4]
        );
        // Shift+Space is still Space.
        let plan = plan_key(KeyFacts {
            code: Some(0x20),
            layout_independent: true,
            ..facts(" ")
        });
        assert_eq!(plan, KeyPlan::Code(0x20));
        // Where the platform does not list it (macOS), it stays text as before.
        assert_eq!(
            plan_key(KeyFacts {
                code: Some(0x31),
                text_key_up: false,
                ..facts(" ")
            }),
            KeyPlan::Text(0x31)
        );
    }

    #[test]
    fn characters_are_typed_as_text() {
        for key in TEXT_KEYS {
            if *key == " " {
                continue;
            }
            // Press: text, whether or not the sharer's layout has the character.
            assert_eq!(plan_key(facts(key)), KeyPlan::Text(0), "{key:?}");
            assert_eq!(
                plan_key(KeyFacts {
                    code: Some(0x41),
                    ..facts(key)
                }),
                KeyPlan::Text(0x41),
                "{key:?}"
            );
        }
    }

    #[test]
    fn text_release_matches_the_platform() {
        // Windows: single characters get a text key-up.
        assert_eq!(
            plan_key(KeyFacts {
                down: false,
                code: Some(0xBC),
                ..facts(",")
            }),
            KeyPlan::Text(0xBC)
        );
        // macOS: plain key-up of the carrier key.
        assert_eq!(
            plan_key(KeyFacts {
                down: false,
                code: Some(0x2B),
                text_key_up: false,
                ..facts(",")
            }),
            KeyPlan::Code(0x2B)
        );
    }

    #[test]
    fn shortcuts_press_the_real_key_or_nothing() {
        // Ctrl+C.
        assert_eq!(
            plan_key(KeyFacts {
                ctrl: true,
                code: Some(0x43),
                ..facts("c")
            }),
            KeyPlan::Code(0x43)
        );
        // AltGr character found on the sharer's layout.
        assert_eq!(
            plan_key(KeyFacts {
                alt: true,
                code: Some(0x32),
                ..facts("@")
            }),
            KeyPlan::Code(0x32)
        );
        // Cmd + a character the sharer's layout does not have: nothing, instead
        // of key code 0 (which is the A key on macOS, i.e. Cmd+A).
        assert_eq!(
            plan_key(KeyFacts {
                meta: true,
                ..facts("ש")
            }),
            KeyPlan::Ignore(KeyIgnored::ShortcutWithoutKey)
        );
    }

    #[test]
    fn modifier_keys_are_recognised() {
        assert_eq!(ModifierKey::from_key("Control"), Some(ModifierKey::Control));
        assert_eq!(ModifierKey::from_key("Shift"), Some(ModifierKey::Shift));
        assert_eq!(ModifierKey::from_key("Alt"), Some(ModifierKey::Alt));
        assert_eq!(
            ModifierKey::from_key("AltGraph"),
            Some(ModifierKey::AltGraph)
        );
        assert_eq!(ModifierKey::from_key("Meta"), Some(ModifierKey::Meta));
        assert_eq!(ModifierKey::from_key("Super"), Some(ModifierKey::Meta));
        for key in ["c", " ", "Enter", "CapsLock", "control", ""] {
            assert_eq!(ModifierKey::from_key(key), None, "{key:?}");
        }
        for modifier in ModifierKey::ALL {
            assert_eq!(ModifierKey::from_key(modifier.key()), Some(modifier));
        }
    }

    #[test]
    fn ctrl_left_down_is_released_before_the_next_plain_key() {
        // Reported once: typing "npm" then Space produced "npm." in a terminal.
        // Space with Ctrl down is Ctrl+Space = "complete" in PowerShell, which
        // turns "npm" into "npm." (npm.cmd / npm.ps1).
        let mut held = HeldModifiers::default();
        // The controller pressed Ctrl over the shared picture...
        held.set(ModifierKey::Control, true);
        // ...released it elsewhere (no event), then types n, p, m, Space: every
        // one of those events says "Ctrl is not held".
        assert_eq!(
            held.take_stale(false, false, false, false),
            vec![ModifierKey::Control]
        );
        // Released once; the following keys find nothing left to release.
        assert_eq!(held.take_stale(false, false, false, false), vec![]);
        assert!(!held.is_held(ModifierKey::Control));
    }

    #[test]
    fn modifiers_the_controller_really_holds_are_kept() {
        let mut held = HeldModifiers::default();
        held.set(ModifierKey::Control, true);
        held.set(ModifierKey::Shift, true);
        // Ctrl+Shift+T: both reported as held.
        assert_eq!(held.take_stale(true, true, false, false), vec![]);
        assert!(held.is_held(ModifierKey::Control));
        assert!(held.is_held(ModifierKey::Shift));
        // Shift let go (its release got lost), Ctrl still held.
        assert_eq!(
            held.take_stale(true, false, false, false),
            vec![ModifierKey::Shift]
        );
        assert!(held.is_held(ModifierKey::Control));
    }

    #[test]
    fn alt_gr_follows_the_alt_flag() {
        let mut held = HeldModifiers::default();
        // A Windows controller holding AltGr: Ctrl (fake) + AltGr, reported as
        // Ctrl+Alt with every key.
        held.set(ModifierKey::Control, true);
        held.set(ModifierKey::AltGraph, true);
        assert_eq!(held.take_stale(true, false, true, false), vec![]);
        // Both releases lost: the next plain key lets go of both.
        assert_eq!(
            held.take_stale(false, false, false, false),
            vec![ModifierKey::Control, ModifierKey::AltGraph]
        );
    }

    #[test]
    fn nothing_held_means_nothing_to_release() {
        let mut held = HeldModifiers::default();
        for flags in 0..16 {
            assert_eq!(
                held.take_stale(
                    flags & 1 != 0,
                    flags & 2 != 0,
                    flags & 4 != 0,
                    flags & 8 != 0
                ),
                vec![]
            );
        }
        assert_eq!(held.take_all(), vec![]);
    }

    #[test]
    fn take_all_releases_everything_once() {
        let mut held = HeldModifiers::default();
        held.set(ModifierKey::Meta, true);
        held.set(ModifierKey::Alt, true);
        held.set(ModifierKey::Alt, false);
        held.set(ModifierKey::Shift, true);
        assert_eq!(held.take_all(), vec![ModifierKey::Shift, ModifierKey::Meta]);
        assert_eq!(held.take_all(), vec![]);
    }

    #[test]
    fn empty_key_is_ignored() {
        assert_eq!(plan_key(facts("")), KeyPlan::Ignore(KeyIgnored::Empty));
    }

    #[test]
    fn swapped_case_only_for_single_cased_letters() {
        assert_eq!(swapped_case("c").as_deref(), Some("C"));
        assert_eq!(swapped_case("C").as_deref(), Some("c"));
        assert_eq!(swapped_case("ä").as_deref(), Some("Ä"));
        assert_eq!(swapped_case("Я").as_deref(), Some("я"));
        assert_eq!(swapped_case("1"), None);
        assert_eq!(swapped_case(","), None);
        assert_eq!(swapped_case(" "), None);
        assert_eq!(swapped_case("ß"), None);
        assert_eq!(swapped_case("ab"), None);
        assert_eq!(swapped_case(""), None);
        assert_eq!(swapped_case("Enter"), None);
    }
}
