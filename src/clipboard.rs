//! Copy and paste for a pane's terminal.
//!
//! VTE ships *no* clipboard keybindings of its own - not one. It exposes
//! `paste_clipboard`/`copy_clipboard_format` and leaves the key handling
//! entirely to the embedder, which is why every real terminal emulator
//! (gnome-terminal, kitty, foot) has a chunk of code that looks like this one.
//! Until this module existed, this app didn't, and so a pane could not be
//! pasted into at all: Ctrl+V reached the pty as a bare 0x16 (readline's
//! "quote the next character"), which does nothing visible, and every user who
//! tried it concluded - correctly - that paste was broken.
//!
//! Images are the other half, and they need more than a keybinding. Claude
//! Code's own Ctrl+V image paste reads the system clipboard by shelling out to
//! `wl-paste` or `xclip`, neither of which is installed on a stock desktop, and
//! neither of which this app can promise. So rather than route an image through
//! a tool that may not be there, this module takes the image straight from GDK
//! (the toolkit this window is already drawn with, so it is always there),
//! writes it to a PNG under the cache dir, and types that path into the pane.
//! Claude reads an image path in a prompt perfectly happily, so the paste lands
//! whether or not the user has ever heard of wl-clipboard.
//!
//! # One paste key, and which case it gets wrong
//!
//! `Ctrl+V` pastes an image when the clipboard holds one and text otherwise -
//! "image wins". This module has been round this loop twice, so it's worth
//! writing down what that costs, because it is not free.
//!
//! The first version sniffed the clipboard a different way: image only if the
//! clipboard held an image *and no text*, text otherwise. That one is strictly
//! broken, and it's the version to never go back to. Whether a copied image
//! comes with text attached is decided by the app it was copied from, not by
//! the person pressing the key: a screenshot tool offers `image/png` alone, so
//! it pasted as an image; Firefox's "Copy Image" attaches `text/plain` too, so
//! the same gesture pasted as text. One intent, one keystroke, and the answer
//! depended on which program the user had been standing in.
//!
//! Splitting the keys apart fixed that, and cost a key everyone then had to
//! remember. "Image wins" is the third position and the one the app now takes:
//! the clipboard's *text* no longer gets a vote, so an image is an image no
//! matter which app it came from - Firefox and the screenshot tool paste alike,
//! which was the actual bug. What's given up is the paragraph-with-an-inline-
//! image case: copy prose out of a rich-text editor that tucks a picture in it,
//! and `Ctrl+V` types a PNG path where the words should be. That case is rarer
//! than pasting a screenshot into an agent prompt, which is what this app is
//! for, so it's the one deliberately gotten wrong. `Ctrl+Shift+V` is kept as a
//! plain alias of `Ctrl+V` for fingers trained on the split version - with
//! image-wins it would do the same thing either way.
//!
//! `Shift+Insert` stays text-only, and is the escape hatch when image-wins
//! guesses wrong: it's the traditional terminal paste, it never looks at the
//! image, and it's what to reach for to get the words out of a clipboard that
//! also happens to hold a picture.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use gtk4::prelude::*;
use gtk4::{gdk, gio, glib, EventControllerKey, PropagationPhase};
use vte4::{prelude::*, Format, Terminal};

/// How long a pasted image is kept before the next paste sweeps it up. These
/// are scratch files in a cache dir, written so claude can read them a moment
/// later; a day is far longer than that hand-off needs and short enough that a
/// year of screenshots doesn't quietly accumulate in the user's home.
const PASTED_IMAGE_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// Wires copy/paste onto `terminal`. Installed on every pane (see `Pane::bare`).
///
/// - `Ctrl+V` (and `Ctrl+Shift+V`) - paste. An image if the clipboard holds one,
///   as a PNG path claude can read; the text otherwise. `Ctrl+V` isn't the
///   traditional terminal paste key, but this app's panes are claude sessions
///   rather than shells, and the 0x16 it would otherwise send has no use in a
///   claude prompt, so there's nothing lost in honoring the key everyone presses
///   first anyway.
/// - `Shift+Insert` - paste the text, and only ever the text, whatever else the
///   clipboard is also carrying. The way out when image-wins guesses wrong.
/// - `Ctrl+C` - copy the selection, and *only* when there is one. With nothing
///   selected the key falls straight through to the pty as SIGINT, which is how
///   you interrupt a running agent - so the one thing to know about this binding
///   is that a *stale* selection left sitting in a pane will eat a Ctrl+C meant
///   as an interrupt. Click once to clear it and the key is SIGINT again. This
///   is the same bargain gnome-terminal and kitty strike.
/// - `Ctrl+Shift+C` - copy, same terms, for fingers trained on the old binding.
pub fn install(terminal: &Terminal) {
    let controller = EventControllerKey::new();
    // Capture, so these land before the terminal turns the keypress into bytes
    // for the pty. (The window's Super+Alt bindings capture ahead of this, but
    // they let everything that isn't Super+Alt straight through - see
    // `keybindings::install`.)
    controller.set_propagation_phase(PropagationPhase::Capture);

    let target = terminal.clone();
    controller.connect_key_pressed(move |_, keyval, _keycode, state| {
        let ctrl = state.contains(gdk::ModifierType::CONTROL_MASK);
        let shift = state.contains(gdk::ModifierType::SHIFT_MASK);

        let Some(action) = action_for(keyval, ctrl, shift, target.has_selection()) else {
            return glib::Propagation::Proceed;
        };

        match action {
            Action::Paste => paste(&target),
            Action::PasteText => paste_text(&target),
            Action::Copy => target.copy_clipboard_format(Format::Text),
        }

        glib::Propagation::Stop
    });

    terminal.add_controller(controller);
}

/// What a keystroke means, once the modifiers have been read off it.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum Action {
    /// Image if the clipboard holds one, text otherwise.
    Paste,
    /// The text, whatever else the clipboard is also carrying.
    PasteText,
    Copy,
}

/// Maps a keystroke to the clipboard action it triggers, or `None` for "not
/// ours, hand it to the pty untouched".
///
/// Split out from `install` so the mapping can be tested without a window, a
/// focused terminal, or a synthetic key event: this is a pure decision, and it's
/// the part with the sharp edge in it (see the `Ctrl+C` note below).
fn action_for(keyval: gdk::Key, ctrl: bool, shift: bool, has_selection: bool) -> Option<Action> {
    // Letter keys arrive uppercase when Shift is held, so normalize and let
    // `shift` alone say whether Shift was down.
    match keyval.to_lower() {
        gdk::Key::v if ctrl => Some(Action::Paste),
        gdk::Key::Insert if shift && !ctrl => Some(Action::PasteText),
        // The selection gate is what lets Ctrl+C be a copy key at all: with
        // nothing selected this returns `None`, the key is never swallowed, and
        // it reaches the pty as the SIGINT that stops a running agent. Copy only
        // borrows the key for the moments there's something to copy.
        gdk::Key::c if ctrl && has_selection => Some(Action::Copy),
        _ => None,
    }
}

/// Paste the clipboard's text into `terminal`, whatever else it may also be
/// holding. Bound to `Shift+Insert`.
///
/// VTE's own paste: it pulls the text, filters out the control characters a
/// pasted string has no business carrying, and honors bracketed-paste mode if
/// the program in the pane has asked for it. All things this module very much
/// does not want to reimplement.
fn paste_text(terminal: &Terminal) {
    terminal.paste_clipboard();
}

/// Paste, image-wins: a PNG path if the clipboard holds an image, its text if
/// not. Bound to `Ctrl+V` and `Ctrl+Shift+V`.
fn paste(terminal: &Terminal) {
    let clipboard = terminal.clipboard();

    if !has_image(&clipboard.formats()) {
        paste_text(terminal);
        return;
    }
    read_and_type_image(terminal, &clipboard);
}

/// Whether `formats` describes a clipboard carrying an image.
///
/// Note this asks about the *image* alone and pointedly says nothing about text:
/// text's presence used to be half this answer, and that coin flip made an image
/// copied from Firefox behave differently from the same image copied from a
/// screenshot tool. See the module docs.
fn has_image(formats: &gdk::ContentFormats) -> bool {
    formats.contains_type(gdk::Texture::static_type())
}

/// Read the clipboard's image, write it out as a PNG, and type its path into
/// the pane - the paste claude can actually act on, without wl-clipboard.
///
/// Asynchronous because the clipboard's owner is another process: GDK has to go
/// and ask it for the bytes, and the answer arrives on the main loop later. If
/// anything in that chain fails, the paste is dropped rather than guessed at -
/// there's nothing sensible to type into a prompt on behalf of an image that
/// didn't arrive.
fn read_and_type_image(terminal: &Terminal, clipboard: &gdk::Clipboard) {
    let terminal = terminal.clone();
    clipboard.read_texture_async(None::<&gio::Cancellable>, move |result| {
        let Ok(Some(texture)) = result else {
            return;
        };
        let Some(path) = write_png(&texture) else {
            return;
        };

        // Typed into the pane exactly as if the user had typed it: the path,
        // then a space, so whatever they type next doesn't run into it. No
        // newline - the paste goes into the prompt, and pressing Return stays
        // the user's decision, the same as it is for a pasted line of text.
        //
        // Abbreviated rather than absolute, because this lands in a prompt the
        // user is still writing in: a full `/home/.../.cache/...` path is most
        // of a line of noise wrapped around the sentence they're composing.
        terminal.feed_child(format!("{} ", abbreviate(&path)).as_bytes());
    });
}

/// The cache root images are written under: `$XDG_CACHE_HOME/atc/img`.
///
/// Short on purpose. Every character here is a character the user reads back in
/// their own prompt, so the directory is abbreviated (`atc/img`) where the rest
/// of the app spells itself out - it's the one path in the codebase whose length
/// is part of the UI.
const CACHE_SUBDIR: [&str; 2] = ["atc", "img"];

/// Where pasted images used to live, before the path became short enough to type
/// into a prompt. Swept alongside the current directory so an upgrade doesn't
/// strand a day's screenshots somewhere nothing will ever tidy them again.
const LEGACY_CACHE_SUBDIR: [&str; 2] = ["agenttilecli", "pasted-images"];

/// `$XDG_CACHE_HOME/atc/img`, created if it isn't there.
fn pasted_images_dir() -> Option<PathBuf> {
    let dir = cache_root()?.join(CACHE_SUBDIR[0]).join(CACHE_SUBDIR[1]);
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir)
}

/// `$XDG_CACHE_HOME`, or `~/.cache` when it isn't set.
fn cache_root() -> Option<PathBuf> {
    std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| home().map(|h| h.join(".cache")))
}

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// `path` with the user's home collapsed to `~`, for typing into a prompt.
///
/// Left absolute when it isn't under home (an `XDG_CACHE_HOME` pointed
/// elsewhere), because a path claude can't resolve is worse than a long one.
fn abbreviate(path: &Path) -> String {
    let shortened = home()
        .filter(|h| h.as_os_str().len() > 1) // `HOME=/` would eat the whole path
        .and_then(|h| path.strip_prefix(h).ok())
        .map(|rest| Path::new("~").join(rest));

    shortened
        .unwrap_or_else(|| path.to_path_buf())
        .display()
        .to_string()
}

/// Writes `texture` into the cache dir as a PNG and returns its path, first
/// sweeping out any image old enough to be nobody's business (see
/// `PASTED_IMAGE_TTL`).
fn write_png(texture: &gdk::Texture) -> Option<PathBuf> {
    let dir = pasted_images_dir()?;
    sweep_old_images(&dir);
    if let Some(legacy) =
        cache_root().map(|r| r.join(LEGACY_CACHE_SUBDIR[0]).join(LEGACY_CACHE_SUBDIR[1]))
    {
        sweep_old_images(&legacy);
    }
    save_texture(texture, &dir)
}

/// Writes `texture` into `dir` as a PNG under a fresh name, returning its path.
fn save_texture(texture: &gdk::Texture, dir: &Path) -> Option<PathBuf> {
    // Named by the clock in base 36, so two pastes never collide and the name
    // stays about six characters instead of the thirteen a millisecond stamp
    // spells out in decimal. Only the low end of the clock is kept: names have
    // to be unique against the images still on disk, and `PASTED_IMAGE_TTL`
    // sweeps those after a day, so a value that repeats every ~25 days can't
    // meet its own twin. The clock can't run backwards past the epoch, so the
    // `unwrap_or` is unreachable in practice - it exists so a pathological clock
    // costs a filename, not a panic.
    const NAME_PERIOD: u128 = 36u128.pow(6); // ~25 days in ms, vs. a 1-day TTL
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
        % NAME_PERIOD;
    let path = dir.join(format!("{}.png", base36(stamp)));

    texture.save_to_png(&path).ok()?;
    Some(path)
}

/// `n` in base 36 (digits then lowercase letters) - the densest filename-safe
/// encoding that survives a case-insensitive filesystem.
fn base36(mut n: u128) -> String {
    const DIGITS: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    if n == 0 {
        return "0".into();
    }
    let mut out = Vec::new();
    while n > 0 {
        out.push(DIGITS[(n % 36) as usize]);
        n /= 36;
    }
    out.reverse();
    String::from_utf8(out).expect("DIGITS is ASCII")
}

/// Deletes pasted images older than `PASTED_IMAGE_TTL`. Best-effort throughout:
/// a file that won't stat or won't unlink is simply left alone, because failing
/// to tidy up is never a good enough reason to fail the paste the user asked
/// for.
fn sweep_old_images(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let stale = entry
            .metadata()
            .and_then(|m| m.modified())
            .and_then(|t| t.elapsed().map_err(std::io::Error::other))
            .is_ok_and(|age| age > PASTED_IMAGE_TTL);
        if stale {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::gtk_test;

    /// A 1x1 red pixel - enough to be a real `gdk::Texture` that a real
    /// clipboard will really advertise as an image.
    fn a_texture() -> gdk::Texture {
        let pixel = glib::Bytes::from_owned(vec![255u8, 0, 0, 255]);
        gdk::MemoryTexture::new(1, 1, gdk::MemoryFormat::R8g8b8a8, &pixel, 4).upcast()
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("agenttilecli-clipboard-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    /// Ctrl+V is the one paste key, with or without Shift, and Shift+Insert is
    /// the text-only way out when image-wins guesses wrong.
    #[test]
    fn the_paste_keys_land_where_they_should() {
        let no_selection = false;
        for (key, ctrl, shift, expected, why) in [
            (
                gdk::Key::v,
                true,
                false,
                Some(Action::Paste),
                "Ctrl+V is the paste key",
            ),
            (
                gdk::Key::V,
                true,
                true,
                Some(Action::Paste),
                "Ctrl+Shift+V is an alias of it, not a second meaning",
            ),
            (
                gdk::Key::Insert,
                false,
                true,
                Some(Action::PasteText),
                "Shift+Insert is text-only",
            ),
            (gdk::Key::v, false, false, None, "a bare v is just a letter"),
            (
                gdk::Key::Insert,
                false,
                false,
                None,
                "a bare Insert isn't ours",
            ),
        ] {
            assert_eq!(
                action_for(key, ctrl, shift, no_selection),
                expected,
                "{why}",
            );
        }
    }

    /// The bargain Ctrl+C-as-copy strikes: it copies when there's a selection,
    /// and with nothing selected it must fall through to the pty as SIGINT.
    /// Swallowing it unconditionally would strand the user in front of a running
    /// agent that won't stop, which is the one failure this binding can't have.
    #[test]
    fn ctrl_c_without_a_selection_stays_sigint() {
        assert_eq!(
            action_for(gdk::Key::c, true, false, false),
            None,
            "Ctrl+C with nothing selected was intercepted; it has to reach the \
             pty as SIGINT or a running agent can't be interrupted",
        );
        assert_eq!(
            action_for(gdk::Key::c, true, false, true),
            Some(Action::Copy),
            "Ctrl+C with a selection should copy",
        );
        assert_eq!(
            action_for(gdk::Key::C, true, true, true),
            Some(Action::Copy),
            "Ctrl+Shift+C should still copy, for fingers trained on it",
        );
    }

    /// The path is typed into a prompt the user is still writing in, so its
    /// length is a feature. A full absolute path ran past 60 characters; this
    /// keeps it under 20.
    #[test]
    fn a_pasted_image_path_is_short_enough_to_sit_in_a_prompt() {
        gtk_test(|| {
            let dir = temp_dir("short-path");
            let path = save_texture(&a_texture(), &dir).expect("the texture was written");
            let name = path.file_name().expect("a filename").to_string_lossy();

            assert!(
                name.len() <= 11,
                "the filename `{name}` is longer than a base-36 stamp plus .png",
            );
            // Measured against the real home, since that's the only prefix
            // `abbreviate` will collapse - see `abbreviate_only_collapses_the_
            // real_home`, which is what keeps that restriction honest.
            let home = std::env::var_os("HOME").map(PathBuf::from).expect("HOME");
            let typed = abbreviate(&home.join(".cache/atc/img").join(&*name));
            assert!(
                typed.len() < 30,
                "`{typed}` is too long to sit in a prompt without wrapping it",
            );
        });
    }

    /// `~` only stands in for the actual home directory - a path outside it has
    /// to stay absolute, or claude is handed something it can't resolve.
    #[test]
    fn abbreviate_only_collapses_the_real_home() {
        let home = std::env::var_os("HOME").map(PathBuf::from).expect("HOME");

        assert_eq!(
            abbreviate(&home.join(".cache/atc/img/mfd0j1.png")),
            "~/.cache/atc/img/mfd0j1.png",
            "a path under home should collapse to ~",
        );
        assert_eq!(
            abbreviate(Path::new("/var/tmp/elsewhere/mfd0j1.png")),
            "/var/tmp/elsewhere/mfd0j1.png",
            "a path outside home must stay absolute and resolvable",
        );
    }

    /// Names have to be unique against every image still on disk, since a
    /// collision would hand claude an older screenshot than the one just pasted.
    #[test]
    fn two_pastes_dont_collide() {
        gtk_test(|| {
            let dir = temp_dir("collide");
            let first = save_texture(&a_texture(), &dir).expect("written");
            std::thread::sleep(Duration::from_millis(2));
            let second = save_texture(&a_texture(), &dir).expect("written");

            assert_ne!(
                first, second,
                "two pastes a moment apart reused a filename; the second would \
                 have overwritten the first",
            );
        });
    }

    #[test]
    fn base36_encodes_the_way_the_filenames_assume() {
        assert_eq!(base36(0), "0");
        assert_eq!(base36(35), "z");
        assert_eq!(base36(36), "10");
        assert_eq!(base36(1752849301234 % 36u128.pow(6)).len(), 6);
    }

    /// The screenshot case, and the whole reason the image paste exists: an
    /// image on the clipboard has to be seen as one, or Ctrl+Shift+V falls back
    /// to pasting text and the user gets nothing.
    ///
    /// Asserted against a *real* `gdk::Clipboard` rather than a hand-built
    /// `ContentFormats`, because the question this has to answer is what GDK
    /// actually advertises for a copied image - which a fabricated format list
    /// would be me answering it myself.
    #[test]
    fn an_image_alone_is_seen_as_an_image() {
        gtk_test(|| {
            let clipboard = gdk::Display::default().expect("a display").clipboard();
            clipboard.set_texture(&a_texture());

            assert!(
                has_image(&clipboard.formats()),
                "a clipboard holding an image should be seen as holding one, \
                 but GDK advertised it as: {:?}",
                clipboard.formats().mime_types(),
            );
        });
    }

    /// Copied text isn't an image, so Ctrl+Shift+V falls back to pasting it
    /// rather than writing a PNG of nothing.
    #[test]
    fn copied_text_is_not_seen_as_an_image() {
        gtk_test(|| {
            let clipboard = gdk::Display::default().expect("a display").clipboard();
            clipboard.set_text("git status");

            assert!(
                !has_image(&clipboard.formats()),
                "copied text was taken for an image; GDK advertised: {:?}",
                clipboard.formats().mime_types(),
            );
        });
    }

    /// The regression test for the bug that split the keys apart.
    ///
    /// A browser's "Copy Image" hands over the image *and* the markup and URL
    /// around it. This module used to read that attached text as a signal that
    /// the user meant a text paste, which made an image copied from Firefox
    /// behave differently from the same image copied from a screenshot tool -
    /// the inconsistency that got reported. Attached text is the source app
    /// being helpful; it says nothing about intent, and Ctrl+Shift+V no longer
    /// listens to it.
    #[test]
    fn an_image_is_still_an_image_when_text_rides_along() {
        gtk_test(|| {
            let both = gdk::ContentProvider::new_union(&[
                gdk::ContentProvider::for_value(&a_texture().to_value()),
                gdk::ContentProvider::for_value(&"an image, and words".to_value()),
            ]);
            let clipboard = gdk::Display::default().expect("a display").clipboard();
            clipboard.set_content(Some(&both)).expect("clipboard set");

            assert!(
                has_image(&clipboard.formats()),
                "an image with text alongside it should still paste as an image \
                 on the image key; GDK advertised: {:?}",
                clipboard.formats().mime_types(),
            );
        });
    }

    /// The handoff to claude is a file on disk, so it has to actually be one -
    /// and actually be a PNG, since claude reads it by extension and content
    /// rather than by taking our word for it.
    #[test]
    fn a_pasted_image_lands_on_disk_as_a_png() {
        gtk_test(|| {
            let dir = temp_dir("writes-png");
            let path = save_texture(&a_texture(), &dir).expect("the texture was written");

            let bytes = std::fs::read(&path).expect("the file is there");
            assert_eq!(
                &bytes[..8],
                b"\x89PNG\r\n\x1a\n",
                "{} isn't a PNG - claude would refuse to read it",
                path.display(),
            );
        });
    }

    /// The sweep runs on the way *in* to a paste, immediately before the new
    /// image is written next to the old ones - so a sweep that misjudged "old"
    /// would delete the very image the user just pasted, and claude would be
    /// handed a path to nothing.
    #[test]
    fn the_sweep_spares_a_freshly_pasted_image() {
        gtk_test(|| {
            let dir = temp_dir("sweep");
            let path = save_texture(&a_texture(), &dir).expect("the texture was written");

            sweep_old_images(&dir);

            assert!(
                path.exists(),
                "the sweep deleted an image written moments ago",
            );
        });
    }
}
