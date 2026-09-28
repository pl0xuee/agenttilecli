//! Which of this app's shortcuts the desktop has already taken.
//!
//! A compositor's key binding is taken before any window sees the key. So a
//! shortcut this app advertises, that the desktop also binds, is not a shortcut
//! that half-works - it is one that does something else entirely, somewhere
//! else, with nothing in this window to say why. That is not hypothetical: on a
//! stock Omarchy desktop ten of this app's keys did exactly that, and Super+Alt
//! +Return, advertised as "open a project", opened a tmux.
//!
//! The defaults have since been moved clear of a stock install (see
//! `keybindings::COMMANDS`), but a desktop is somebody's to configure, and the
//! next binding they add can land on any key. So the window asks.
//!
//! Hyprland only, because Hyprland is the compositor that can be asked:
//! `hyprctl binds -j` lists every binding with its modifiers and its key, as
//! data. Asked once, off the main thread, a moment after the window is up; if
//! anything collides, a toast says how many and the shortcuts sheet marks
//! which, with the config line that moves each one.

use std::cell::RefCell;

use adw::prelude::*;
use gtk4::{gdk, gio, glib};

use crate::app::App;

thread_local! {
    /// The command ids the desktop was found holding, for the shortcuts sheet.
    static TAKEN: RefCell<Vec<&'static str>> = const { RefCell::new(Vec::new()) };
}

/// The command ids whose keys the desktop has taken, as last found.
pub fn taken_ids() -> Vec<&'static str> {
    TAKEN.with(|taken| taken.borrow().clone())
}

/// Hyprland's modifier bits, from its `modmask`.
mod mask {
    pub const SHIFT: u32 = 1;
    pub const CAPS: u32 = 2;
    pub const CTRL: u32 = 4;
    pub const ALT: u32 = 8;
    pub const MOD2: u32 = 16;
    pub const SUPER: u32 = 64;
}

/// One binding the compositor holds, as far as a collision cares.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Held {
    /// Modifier bits, without the lock keys - Hyprland ignores Caps and Num
    /// Lock when matching, so a collision has to as well.
    pub mods: u32,
    pub key: HeldKey,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum HeldKey {
    /// A keysym name, in whatever case the config wrote it: Hyprland matches
    /// names case-insensitively, so "RETURN" and "Return" are one key.
    Name(String),
    /// A hardware keycode, for bindings written `code:34`.
    Code(u32),
}

/// The bindings in `hyprctl binds -j` output that can take a key from a window.
///
/// Leaves out the ones that can't: mouse bindings, bindings that only exist
/// inside a submap the user has to enter first, and non-consuming ones, which
/// pass the key on to the window as well.
pub fn parse_hyprland(json: &str) -> Vec<Held> {
    let Ok(serde_json::Value::Array(binds)) = serde_json::from_str::<serde_json::Value>(json)
    else {
        return Vec::new();
    };
    binds
        .iter()
        .filter(|bind| !bind["mouse"].as_bool().unwrap_or(false))
        .filter(|bind| !bind["non_consuming"].as_bool().unwrap_or(false))
        .filter(|bind| bind["submap"].as_str().unwrap_or_default().is_empty())
        .filter_map(|bind| {
            let mods = bind["modmask"].as_u64()? as u32 & !(mask::CAPS | mask::MOD2);
            let name = bind["key"].as_str().unwrap_or_default();
            let code = bind["keycode"].as_u64().unwrap_or(0) as u32;
            let key = if !name.is_empty() {
                HeldKey::Name(name.to_string())
            } else if code > 0 {
                HeldKey::Code(code)
            } else {
                return None;
            };
            Some(Held { mods, key })
        })
        .collect()
}

/// The GDK key a Hyprland key name means, trying the spellings Hyprland's own
/// case-insensitive lookup would accept.
fn key_from_name(name: &str) -> Option<gdk::Key> {
    let lower = name.to_lowercase();
    // `PAGE_UP` is `Page_Up`, `RETURN` is `Return`: capital after each `_`.
    let titled: String = lower
        .split('_')
        .map(|part| {
            let mut chars = part.chars();
            chars
                .next()
                .map(|first| first.to_uppercase().chain(chars).collect::<String>())
                .unwrap_or_default()
        })
        .collect::<Vec<_>>()
        .join("_");
    [name.to_string(), lower, titled, name.to_uppercase()]
        .iter()
        .filter_map(gdk::Key::from_name)
        .find(|key| *key != gdk::Key::VoidSymbol)
}

/// The keys a held binding stands for, as the matcher compares them.
fn held_keys(held: &Held, display: Option<&gdk::Display>) -> Vec<gdk::Key> {
    match &held.key {
        HeldKey::Name(name) => key_from_name(name).into_iter().map(|k| k.to_lower()).collect(),
        HeldKey::Code(code) => display
            .and_then(|display| display.map_keycode(*code))
            .map(|entries| {
                entries
                    .into_iter()
                    .filter(|(entry, _)| entry.group() == 0 && entry.level() == 0)
                    .map(|(_, key)| key.to_lower())
                    .collect()
            })
            .unwrap_or_default(),
    }
}

/// The ids of the app's bindings that the desktop's `held` bindings take.
pub fn collisions(
    bound: &[crate::keybindings::Bound],
    held: &[Held],
    display: Option<&gdk::Display>,
) -> Vec<&'static str> {
    let mut taken = Vec::new();
    for binding in bound {
        let mut ours = 0;
        if binding.mods.contains(gdk::ModifierType::SUPER_MASK) {
            ours |= mask::SUPER;
        }
        if binding.mods.contains(gdk::ModifierType::ALT_MASK) {
            ours |= mask::ALT;
        }
        if binding.mods.contains(gdk::ModifierType::CONTROL_MASK) {
            ours |= mask::CTRL;
        }
        let collides = held.iter().any(|held| {
            let shifted = held.mods & mask::SHIFT != 0;
            held.mods & !mask::SHIFT == ours
                && binding.advertised_with_shift() == shifted
                && held_keys(held, display).contains(&binding.key)
        });
        if collides && !taken.contains(&binding.id) {
            taken.push(binding.id);
        }
    }
    taken
}

/// Asks the desktop what it holds, and tells the user about any collision.
///
/// Silent unless it finds one, and silent on anything that isn't Hyprland -
/// there is nothing useful to say about a desktop that can't be asked.
pub fn check(app: &App) {
    if std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_none() {
        return;
    }
    let app = app.clone();
    glib::spawn_future_local(async move {
        let listing = gio::spawn_blocking(|| {
            std::process::Command::new("hyprctl")
                .args(["binds", "-j"])
                .output()
                .ok()
                .filter(|output| output.status.success())
                .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
        })
        .await
        .ok()
        .flatten();
        let Some(listing) = listing else { return };

        let held = parse_hyprland(&listing);
        let (bound, _) = crate::keybindings::resolve();
        let display = gdk::Display::default();
        let taken = collisions(&bound, &held, display.as_ref());
        if taken.is_empty() {
            return;
        }
        let count = taken.len();
        TAKEN.with(|cell| *cell.borrow_mut() = taken);
        app.toast_with_action(
            &if count == 1 {
                "Hyprland has taken one of this app's shortcuts".to_string()
            } else {
                format!("Hyprland has taken {count} of this app's shortcuts")
            },
            "Show",
            "win.shortcuts",
        );
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    const LISTING: &str = r#"[
        {"modmask": 72, "submap": "", "key": "RETURN", "keycode": 0, "mouse": false, "non_consuming": false, "description": "Tmux"},
        {"modmask": 72, "submap": "", "key": "", "keycode": 34, "mouse": false, "non_consuming": false, "description": "Webcam smaller"},
        {"modmask": 73, "submap": "", "key": "A", "keycode": 0, "mouse": false, "non_consuming": false, "description": "Grok"},
        {"modmask": 72, "submap": "", "key": "mouse_down", "keycode": 0, "mouse": true, "non_consuming": false},
        {"modmask": 72, "submap": "resize", "key": "H", "keycode": 0, "mouse": false, "non_consuming": false},
        {"modmask": 72, "submap": "", "key": "O", "keycode": 0, "mouse": false, "non_consuming": true},
        {"modmask": 90, "submap": "", "key": "T", "keycode": 0, "mouse": false, "non_consuming": false}
    ]"#;

    #[test]
    fn only_bindings_that_can_take_a_key_are_kept() {
        let held = parse_hyprland(LISTING);
        assert_eq!(
            held,
            vec![
                Held { mods: 72, key: HeldKey::Name("RETURN".into()) },
                Held { mods: 72, key: HeldKey::Code(34) },
                Held { mods: 73, key: HeldKey::Name("A".into()) },
                // Num Lock (16) and Caps (2) are ignored, as Hyprland ignores them.
                Held { mods: 72, key: HeldKey::Name("T".into()) },
            ],
            "mouse, submap and pass-through bindings take nothing from a window",
        );
    }

    #[test]
    fn rubbish_from_hyprctl_is_no_bindings_rather_than_a_panic() {
        assert!(parse_hyprland("").is_empty());
        assert!(parse_hyprland("{}").is_empty());
        assert!(parse_hyprland("[{\"key\": 3}]").is_empty());
    }

    /// Hyprland's names are case-insensitive keysym names; GDK's lookup is
    /// not, so every spelling a Hyprland config uses has to land on the key.
    #[test]
    fn a_hyprland_key_name_is_read_in_any_case() {
        crate::testing::gtk_test(|| {
            assert_eq!(key_from_name("RETURN"), Some(gdk::Key::Return));
            assert_eq!(key_from_name("SLASH"), Some(gdk::Key::slash));
            assert_eq!(key_from_name("PAGE_UP"), Some(gdk::Key::Page_Up));
            assert_eq!(key_from_name("comma"), Some(gdk::Key::comma));
            assert_eq!(key_from_name("K").map(|k| k.to_lower()), Some(gdk::Key::k));
            assert_eq!(key_from_name("NotAKeyAtAll"), None);
        });
    }

    /// The whole point: the old defaults collide with a stock Omarchy listing,
    /// and the new ones do not.
    #[test]
    fn the_defaults_do_not_collide_with_the_stock_desktop() {
        crate::testing::gtk_test(|| {
            let (bound, _) = crate::keybindings::resolve();
            let held = vec![
                Held { mods: 72, key: HeldKey::Name("RETURN".into()) },
                Held { mods: 72, key: HeldKey::Name("G".into()) },
                Held { mods: 72, key: HeldKey::Name("K".into()) },
                Held { mods: 72, key: HeldKey::Name("TAB".into()) },
                Held { mods: 72, key: HeldKey::Name("F".into()) },
                Held { mods: 72, key: HeldKey::Name("SLASH".into()) },
            ];
            assert_eq!(collisions(&bound, &held, None), Vec::<&str>::new());

            // And a binding someone adds on top of a default is found.
            let added = vec![Held { mods: 72, key: HeldKey::Name("N".into()) }];
            assert_eq!(collisions(&bound, &added, None), vec!["go-to-waiting"]);
            // With Shift, only the Shift half of a pair is taken.
            let shifted = vec![Held { mods: 73, key: HeldKey::Name("J".into()) }];
            assert_eq!(collisions(&bound, &shifted, None), vec!["focus-previous"]);
        });
    }
}
