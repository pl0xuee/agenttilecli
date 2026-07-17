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
//! # Why the keys are split rather than clever
//!
//! Text and images each get their own key, and neither ever second-guesses the
//! other. That looks like one key too many until you try to write the clever
//! version, which this module did first and which had to be taken back out.
//!
//! The clever version was a single paste key that sniffed the clipboard and
//! guessed: image if the clipboard held an image and no text, text otherwise.
//! The guess reads sensibly and is wrong in practice, because *whether a copied
//! image comes with text attached is decided by the app it was copied from*,
//! not by the person pressing the key. A screenshot tool offers `image/png`
//! alone, so it pasted as an image. Firefox's "Copy Image" helpfully attaches
//! `text/plain` as well, so the same gesture pasted as text instead. Plasma's
//! clipboard manager re-offers a history entry with formats of its own choosing,
//! so an image could change its mind about what it was between one paste and the
//! next. One intent, one keystroke, and the answer depended on which program the
//! user had been standing in - which is indistinguishable, from the outside,
//! from the feature being broken at random.
//!
//! There is no tie-break that fixes this, only a choice of which case to get
//! wrong: "image wins" turns copying a paragraph that happens to contain an
//! inline image into a PNG path where the words should be. So the tie isn't
//! broken here, it's refused. `Ctrl+V` is text, always. `Ctrl+Shift+V` is the
//! image, always. The key says which, the clipboard's advertised formats don't
//! get a vote, and the same keystroke does the same thing every time no matter
//! where the copy came from.

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
/// - `Ctrl+V` and `Shift+Insert` - paste the clipboard's text, and only ever its
///   text. `Shift+Insert` is the traditional terminal one; `Ctrl+V` isn't, but
///   this app's panes are claude sessions rather than shells, and the 0x16 it
///   would otherwise send has no use in a claude prompt, so there's nothing lost
///   in honoring the key everyone presses first anyway.
/// - `Ctrl+Shift+V` - paste a copied image, as a PNG path claude can read. Falls
///   back to text when there's no image to paste, so it's never a dead key.
/// - `Ctrl+Shift+C` - copy the selection, and *only* when there is one, so the
///   binding can't shadow anything when there isn't.
///
/// Neither paste key consults the other's format: see the module docs for why
/// guessing between them is the one thing that can't work.
///
/// Plain `Ctrl+C` is deliberately absent: it must stay SIGINT, which is how you
/// interrupt a running agent.
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
            Action::PasteText => paste_text(&target),
            Action::PasteImage => paste_image(&target),
            Action::Copy => target.copy_clipboard_format(Format::Text),
        }

        glib::Propagation::Stop
    });

    terminal.add_controller(controller);
}

/// What a keystroke means, once the modifiers have been read off it.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum Action {
    PasteText,
    PasteImage,
    Copy,
}

/// Maps a keystroke to the clipboard action it triggers, or `None` for "not
/// ours, hand it to the pty untouched".
///
/// Split out from `install` so the mapping can be tested without a window, a
/// focused terminal, or a synthetic key event: this is a pure decision, and it's
/// the part with the sharp edge in it (see the shift-ordering note below).
fn action_for(keyval: gdk::Key, ctrl: bool, shift: bool, has_selection: bool) -> Option<Action> {
    // Letter keys arrive uppercase when Shift is held, so normalize and let
    // `shift` alone say whether Shift was down.
    match keyval.to_lower() {
        // Shift first: `ctrl && shift` is a strictly narrower match than `ctrl`,
        // so it has to be asked first or the text arm would answer for both and
        // Ctrl+Shift+V would never paste an image.
        gdk::Key::v if ctrl && shift => Some(Action::PasteImage),
        gdk::Key::v if ctrl => Some(Action::PasteText),
        gdk::Key::Insert if shift && !ctrl => Some(Action::PasteText),
        // Only with a selection to copy, so the binding can't shadow anything
        // when there isn't one.
        gdk::Key::c if ctrl && shift && has_selection => Some(Action::Copy),
        _ => None,
    }
}

/// Paste the clipboard's text into `terminal`, whatever else it may also be
/// holding. Bound to `Ctrl+V` and `Shift+Insert`.
///
/// VTE's own paste: it pulls the text, filters out the control characters a
/// pasted string has no business carrying, and honors bracketed-paste mode if
/// the program in the pane has asked for it. All things this module very much
/// does not want to reimplement.
fn paste_text(terminal: &Terminal) {
    terminal.paste_clipboard();
}

/// Paste the clipboard's image into `terminal` as a PNG path. Bound to
/// `Ctrl+Shift+V`.
///
/// Falls back to a text paste when the clipboard has no image in it. That's for
/// the fingers rather than the logic: `Ctrl+Shift+V` is the paste key in every
/// other terminal on the machine, so someone will press it meaning "paste", with
/// nothing but text copied, and having it do nothing at all would be its own
/// small bug. There's no guessing in the fallback - it happens only when the
/// image key has no image to paste, never when there's a choice to be made.
fn paste_image(terminal: &Terminal) {
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
/// text's presence used to be half this answer, and that's precisely the coin
/// flip the module docs describe removing.
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
        terminal.feed_child(format!("{} ", path.display()).as_bytes());
    });
}

/// `$XDG_CACHE_HOME/agenttilecli/pasted-images`, created if it isn't there.
fn pasted_images_dir() -> Option<PathBuf> {
    let dir = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))?
        .join("agenttilecli")
        .join("pasted-images");
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir)
}

/// Writes `texture` into the cache dir as a PNG and returns its path, first
/// sweeping out any image old enough to be nobody's business (see
/// `PASTED_IMAGE_TTL`).
fn write_png(texture: &gdk::Texture) -> Option<PathBuf> {
    let dir = pasted_images_dir()?;
    sweep_old_images(&dir);
    save_texture(texture, &dir)
}

/// Writes `texture` into `dir` as a PNG under a fresh name, returning its path.
fn save_texture(texture: &gdk::Texture, dir: &Path) -> Option<PathBuf> {
    // Named by the clock, so two pastes never collide and the newest one is
    // obvious in a directory listing. The clock can't run backwards past the
    // epoch, so the fallback is unreachable in practice - it exists so a
    // pathological clock costs a filename, not a panic.
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let path = dir.join(format!("paste-{stamp}.png"));

    texture.save_to_png(&path).ok()?;
    Some(path)
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

    /// The whole point of the split: each paste key means one thing, and the
    /// two don't overlap. Ctrl+Shift+V in particular has to reach the *image*
    /// arm - it's a strictly narrower match than the Ctrl+V text arm, so an arm
    /// written in the other order would quietly swallow it and paste text.
    #[test]
    fn each_paste_key_means_exactly_one_thing() {
        let no_selection = false;
        for (key, ctrl, shift, expected, why) in [
            (
                gdk::Key::v,
                true,
                false,
                Some(Action::PasteText),
                "Ctrl+V is text",
            ),
            (
                gdk::Key::V,
                true,
                true,
                Some(Action::PasteImage),
                "Ctrl+Shift+V is the image key",
            ),
            (
                gdk::Key::Insert,
                false,
                true,
                Some(Action::PasteText),
                "Shift+Insert is text",
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

    /// Plain Ctrl+C must stay SIGINT - it's how you interrupt a running agent,
    /// and swallowing it would strand the user in front of a pane that won't
    /// stop. Ctrl+Shift+C only copies when there's a selection to copy.
    #[test]
    fn ctrl_c_is_left_alone_so_agents_stay_interruptible() {
        assert_eq!(
            action_for(gdk::Key::c, true, false, true),
            None,
            "plain Ctrl+C was intercepted; it has to reach the pty as SIGINT \
             even when there's a selection sitting there",
        );
        assert_eq!(
            action_for(gdk::Key::C, true, true, true),
            Some(Action::Copy),
            "Ctrl+Shift+C with a selection should copy",
        );
        assert_eq!(
            action_for(gdk::Key::C, true, true, false),
            None,
            "Ctrl+Shift+C with nothing selected should fall through, not \
             swallow the key",
        );
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
