//! Persistent host-key → Windows Mobile button map.
//!
//! The launchers present the guest with a fixed set of *buttons* — the
//! Pocket PC D-pad, the three face buttons a GAPI title asks
//! `GXGetDefaultKeys` about, the two soft keys, and the Gizmondo turbo
//! key. Which host key produces which button is a user preference, so
//! it lives here, in the same `config.json` the rest of the launcher
//! settings do, rather than in a hard-coded table inside one frontend.
//!
//! Host keys are stored by *name*, not by keycode: `"Left"`,
//! `"Space"`, `"1"`. The names are exactly `egui::Key::name()` (and
//! `egui::Key::from_name` parses them back), which is what the desktop
//! launcher's keyboard handler works in. Storing names rather than
//! numbers keeps `config.json` readable and survives an egui upgrade
//! renumbering its enum.
//!
//! egui spells the arrow keys `"Up"`/`"Down"`/`"Left"`/`"Right"` in
//! `name()` but also parses `"ArrowUp"` and friends in `from_name`, so
//! [`keys_match`] treats the two spellings as the same key. Otherwise a
//! `config.json` hand-edited (or written by a build that stored the
//! `ArrowUp` spelling) would silently lose its D-pad.
//!
//! The virtual-key codes each button sends are *not* a preference —
//! they are the codes `gx.dll` hands the guest, so they have to match
//! `pocket_core::kernel::gapi` and the on-screen pads in both
//! frontends. See [`GuestButton::vk`].

use serde::{Deserialize, Serialize};

/// Win32 virtual-key codes, mirrored from `pocket_kernel::gapi` and
/// `winuser.h`. This crate deliberately does not depend on the kernel
/// (the Android launcher links it without the emulator), so the codes
/// are repeated here; `guest_button_vks_match_the_gapi_table` in
/// `pocket-desktop` pins them against the real table.
mod vk {
    pub const UP: u16 = 0x26;
    pub const DOWN: u16 = 0x28;
    pub const LEFT: u16 = 0x25;
    pub const RIGHT: u16 = 0x27;
    pub const RETURN: u16 = 0x0D;
    pub const SPACE: u16 = 0x20;
    pub const TAB: u16 = 0x09;
    pub const SHIFT: u16 = 0x10;
    pub const CTRL: u16 = 0x11;
    pub const ESCAPE: u16 = 0x1B;
    pub const F3: u16 = 0x72;
}

/// One button on the emulated device.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GuestButton {
    DpadUp,
    DpadDown,
    DpadLeft,
    DpadRight,
    /// The confirm key. `VK_RETURN` on the wire, rewritten to `vkA` for
    /// a guest that asked for the GAPI key list — see
    /// `pocket_kernel::gapi::remap_host_key`.
    Action,
    ButtonA,
    ButtonB,
    ButtonC,
    Soft1,
    Soft2,
    /// Gizmondo's turbo key (`VK_F3`).
    Turbo,
}

impl GuestButton {
    /// Every button, in the order the settings UI lists them.
    pub const ALL: [GuestButton; 11] = [
        GuestButton::DpadUp,
        GuestButton::DpadDown,
        GuestButton::DpadLeft,
        GuestButton::DpadRight,
        GuestButton::Action,
        GuestButton::ButtonA,
        GuestButton::ButtonB,
        GuestButton::ButtonC,
        GuestButton::Soft1,
        GuestButton::Soft2,
        GuestButton::Turbo,
    ];

    /// The virtual-key code this button sends into the guest.
    pub fn vk(self) -> u16 {
        match self {
            GuestButton::DpadUp => vk::UP,
            GuestButton::DpadDown => vk::DOWN,
            GuestButton::DpadLeft => vk::LEFT,
            GuestButton::DpadRight => vk::RIGHT,
            GuestButton::Action => vk::RETURN,
            GuestButton::ButtonA => vk::CTRL,
            GuestButton::ButtonB => vk::SPACE,
            GuestButton::ButtonC => vk::SHIFT,
            GuestButton::Soft1 => vk::TAB,
            GuestButton::Soft2 => vk::ESCAPE,
            GuestButton::Turbo => vk::F3,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            GuestButton::DpadUp => "D-pad up",
            GuestButton::DpadDown => "D-pad down",
            GuestButton::DpadLeft => "D-pad left",
            GuestButton::DpadRight => "D-pad right",
            GuestButton::Action => "Action / confirm",
            GuestButton::ButtonA => "Button A",
            GuestButton::ButtonB => "Button B",
            GuestButton::ButtonC => "Button C",
            GuestButton::Soft1 => "Soft key 1",
            GuestButton::Soft2 => "Soft key 2",
            GuestButton::Turbo => "Turbo",
        }
    }

    /// Host keys bound to this button out of the box.
    ///
    /// These reproduce the table the desktop launcher used to hard-code,
    /// so a user who never opens the keybinding screen sees no change.
    pub fn default_keys(self) -> &'static [&'static str] {
        match self {
            GuestButton::DpadUp => &["Up"],
            GuestButton::DpadDown => &["Down"],
            GuestButton::DpadLeft => &["Left"],
            GuestButton::DpadRight => &["Right"],
            GuestButton::Action => &["Enter"],
            GuestButton::ButtonA => &["A"],
            GuestButton::ButtonB => &["B", "Space"],
            GuestButton::ButtonC => &["C"],
            GuestButton::Soft1 => &["Tab", "1"],
            GuestButton::Soft2 => &["Escape", "2"],
            GuestButton::Turbo => &["S", "F3"],
        }
    }
}

/// Whether two stored host-key names refer to the same key.
///
/// Case-insensitive, and tolerant of the two spellings egui accepts for
/// the arrow keys (`name()` yields `"Up"`, `from_name` also parses
/// `"ArrowUp"`).
pub fn keys_match(a: &str, b: &str) -> bool {
    canonical_key_name(a).eq_ignore_ascii_case(canonical_key_name(b))
}

fn canonical_key_name(key: &str) -> &str {
    for arrow in ["ArrowUp", "ArrowDown", "ArrowLeft", "ArrowRight"] {
        if key.eq_ignore_ascii_case(arrow) {
            return &arrow["Arrow".len()..];
        }
    }
    key
}

/// One button and the host keys bound to it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyBinding {
    pub button: GuestButton,
    /// Host key names (`egui::Key::name()`). Empty means "this button
    /// has no keyboard binding", which is a legitimate choice.
    #[serde(default)]
    pub keys: Vec<String>,
}

/// The whole map. Serialised as a list rather than a JSON object so the
/// Android side can round-trip it with `org.json` without having to
/// know the button names, and so the order the settings screen shows is
/// stable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct KeyBindings {
    pub bindings: Vec<KeyBinding>,
}

impl Default for KeyBindings {
    fn default() -> Self {
        Self {
            bindings: GuestButton::ALL
                .iter()
                .map(|&button| KeyBinding {
                    button,
                    keys: button
                        .default_keys()
                        .iter()
                        .map(|k| (*k).to_string())
                        .collect(),
                })
                .collect(),
        }
    }
}

impl KeyBindings {
    /// Host keys currently bound to `button`.
    pub fn keys_for(&self, button: GuestButton) -> &[String] {
        self.bindings
            .iter()
            .find(|b| b.button == button)
            .map(|b| b.keys.as_slice())
            .unwrap_or(&[])
    }

    /// Replace the keys bound to `button`, dropping duplicates and
    /// stealing each key from whichever button held it before. A host
    /// key that produced two different guest buttons would send both on
    /// every press, which is never what the user meant.
    pub fn set_keys(&mut self, button: GuestButton, keys: Vec<String>) {
        let mut deduped: Vec<String> = Vec::with_capacity(keys.len());
        for key in keys {
            if !deduped.iter().any(|k| keys_match(k, &key)) {
                deduped.push(key);
            }
        }
        for other in self.bindings.iter_mut() {
            if other.button != button {
                other
                    .keys
                    .retain(|k| !deduped.iter().any(|d| keys_match(d, k)));
            }
        }
        match self.bindings.iter_mut().find(|b| b.button == button) {
            Some(existing) => existing.keys = deduped,
            None => self.bindings.push(KeyBinding {
                button,
                keys: deduped,
            }),
        }
    }

    /// Bind one more host key to `button`.
    pub fn bind(&mut self, button: GuestButton, key: &str) {
        let mut keys = self.keys_for(button).to_vec();
        keys.push(key.to_string());
        self.set_keys(button, keys);
    }

    /// Drop one host key from `button`.
    pub fn unbind(&mut self, button: GuestButton, key: &str) {
        let keys: Vec<String> = self
            .keys_for(button)
            .iter()
            .filter(|k| !keys_match(k, key))
            .cloned()
            .collect();
        self.set_keys(button, keys);
    }

    /// The button a host key produces, if any. This is the lookup the
    /// desktop launcher's keyboard handler does on every key event.
    pub fn button_for_key(&self, key: &str) -> Option<GuestButton> {
        self.bindings
            .iter()
            .find(|b| b.keys.iter().any(|k| keys_match(k, key)))
            .map(|b| b.button)
    }

    /// The virtual key a host key sends into the guest, if any.
    pub fn vk_for_key(&self, key: &str) -> Option<u16> {
        self.button_for_key(key).map(GuestButton::vk)
    }

    /// Add any button missing from a config written by an older build,
    /// so a new button does not silently arrive unbound.
    pub fn fill_missing_buttons(&mut self) {
        for button in GuestButton::ALL {
            if !self.bindings.iter().any(|b| b.button == button) {
                self.bindings.push(KeyBinding {
                    button,
                    keys: button
                        .default_keys()
                        .iter()
                        .map(|k| (*k).to_string())
                        .collect(),
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_cover_every_button() {
        let bindings = KeyBindings::default();
        for button in GuestButton::ALL {
            assert!(
                !bindings.keys_for(button).is_empty(),
                "{button:?} has no default key"
            );
        }
    }

    #[test]
    fn a_key_only_ever_drives_one_button() {
        let mut bindings = KeyBindings::default();
        // "Left" starts on DpadLeft; moving it to DpadRight has to
        // take it away from DpadLeft, or one press would send both.
        bindings.bind(GuestButton::DpadRight, "Left");
        assert_eq!(
            bindings.button_for_key("Left"),
            Some(GuestButton::DpadRight)
        );
        assert!(bindings.keys_for(GuestButton::DpadLeft).is_empty());
    }

    #[test]
    fn lookup_is_case_insensitive_and_maps_to_vk() {
        let bindings = KeyBindings::default();
        assert_eq!(bindings.vk_for_key("up"), Some(0x26));
        // egui parses both spellings of the arrows, so both must resolve.
        assert_eq!(bindings.vk_for_key("ArrowUp"), Some(0x26));
        assert_eq!(bindings.vk_for_key("Enter"), Some(0x0D));
        assert_eq!(bindings.vk_for_key("F12"), None);
    }

    #[test]
    fn unbinding_leaves_the_button_present_but_empty() {
        let mut bindings = KeyBindings::default();
        for key in bindings.keys_for(GuestButton::Turbo).to_vec() {
            bindings.unbind(GuestButton::Turbo, &key);
        }
        assert!(bindings.keys_for(GuestButton::Turbo).is_empty());
        assert!(bindings
            .bindings
            .iter()
            .any(|b| b.button == GuestButton::Turbo));
    }

    #[test]
    fn a_config_from_an_older_build_gains_the_new_buttons() {
        let mut bindings = KeyBindings {
            bindings: vec![KeyBinding {
                button: GuestButton::DpadUp,
                keys: vec!["K".to_string()],
            }],
        };
        bindings.fill_missing_buttons();
        assert_eq!(bindings.bindings.len(), GuestButton::ALL.len());
        assert_eq!(bindings.keys_for(GuestButton::DpadUp), &["K".to_string()]);
    }

    #[test]
    fn round_trips_through_json_as_a_list() {
        let bindings = KeyBindings::default();
        let json = serde_json::to_string(&bindings).unwrap();
        assert!(json.starts_with('['), "expected a JSON array, got {json}");
        let back: KeyBindings = serde_json::from_str(&json).unwrap();
        assert_eq!(back, bindings);
    }
}
