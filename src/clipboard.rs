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
/// The bindings are the ones a terminal user already has in their fingers:
///
/// - `Ctrl+Shift+V` and `Shift+Insert` - paste, the two standard terminal ones.
/// - `Ctrl+V` - also paste. Not traditional, but this app's panes are claude
///   sessions rather than shells, and claude asks people to press Ctrl+V; the
///   0x16 it would otherwise send has no use in a claude prompt, so there is
///   nothing to lose by honoring the keystroke everyone presses first anyway.
/// - `Ctrl+Shift+C` - copy the selection, and *only* when there is one, so the
///   binding can't shadow anything when there isn't.
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

        // Letter keys arrive uppercase when Shift is held, so normalize and let
        // `shift` alone say whether Shift was down.
        match keyval.to_lower() {
            gdk::Key::v if ctrl => paste(&target),
            gdk::Key::Insert if shift && !ctrl => paste(&target),
            gdk::Key::c if ctrl && shift && target.has_selection() => {
                target.copy_clipboard_format(Format::Text);
            }
            _ => return glib::Propagation::Proceed,
        }

        glib::Propagation::Stop
    });

    terminal.add_controller(controller);
}

/// Paste whatever the clipboard is holding into `terminal`.
///
/// Text first: a clipboard that offers text is a text paste, even if it *also*
/// offers an image (Firefox's "copy image" hands over both an image and the
/// HTML around it, and a file manager's copied file offers its path as text) -
/// pasting the text is what the user meant in every one of those cases. Only a
/// clipboard with an image and no text at all - a screenshot, in other words -
/// takes the image path.
fn paste(terminal: &Terminal) {
    let clipboard = terminal.clipboard();

    if wants_image_paste(&clipboard.formats()) {
        paste_image(terminal, &clipboard);
        return;
    }

    // VTE's own paste: it pulls the text, filters out the control characters a
    // pasted string has no business carrying, and honors bracketed-paste mode
    // if the program in the pane has asked for it. All things this module very
    // much does not want to reimplement.
    terminal.paste_clipboard();
}

/// Whether `formats` describes a clipboard that should paste as an image: one
/// carrying an image and *no* text. See `paste` for why text wins the tie.
fn wants_image_paste(formats: &gdk::ContentFormats) -> bool {
    let has_text = formats.contain_mime_type("text/plain")
        || formats.contain_mime_type("text/plain;charset=utf-8");
    let has_image = formats.contains_type(gdk::Texture::static_type());

    has_image && !has_text
}

/// Read the clipboard's image, write it out as a PNG, and type its path into
/// the pane - the paste claude can actually act on, without wl-clipboard.
///
/// Asynchronous because the clipboard's owner is another process: GDK has to go
/// and ask it for the bytes, and the answer arrives on the main loop later. If
/// anything in that chain fails, the paste is dropped rather than guessed at -
/// there's nothing sensible to type into a prompt on behalf of an image that
/// didn't arrive.
fn paste_image(terminal: &Terminal, clipboard: &gdk::Clipboard) {
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

    /// The screenshot case, and the whole reason `paste_image` exists: an image
    /// on the clipboard and nothing else has to route to the image path, or the
    /// user gets VTE's text paste of an image, which is nothing at all.
    ///
    /// Asserted against a *real* `gdk::Clipboard` rather than a hand-built
    /// `ContentFormats`, because the question this has to answer is what GDK
    /// actually advertises for a copied image - which a fabricated format list
    /// would be me answering it myself.
    #[test]
    fn an_image_alone_pastes_as_an_image() {
        gtk_test(|| {
            let clipboard = gdk::Display::default().expect("a display").clipboard();
            clipboard.set_texture(&a_texture());

            assert!(
                wants_image_paste(&clipboard.formats()),
                "a clipboard holding only an image should paste as an image, \
                 but GDK advertised it as: {:?}",
                clipboard.formats().mime_types(),
            );
        });
    }

    /// Copied text is a text paste - the ordinary case, and the one that has to
    /// keep working now that Ctrl+V is intercepted rather than passed through.
    #[test]
    fn copied_text_pastes_as_text() {
        gtk_test(|| {
            let clipboard = gdk::Display::default().expect("a display").clipboard();
            clipboard.set_text("git status");

            assert!(
                !wants_image_paste(&clipboard.formats()),
                "copied text should paste as text, but was taken for an image; \
                 GDK advertised: {:?}",
                clipboard.formats().mime_types(),
            );
        });
    }

    /// The tie, which text wins: a browser's "copy image" puts the image *and*
    /// the markup around it on the clipboard, and a file manager's copied file
    /// offers its own path as text. Writing a PNG to disk in either case would
    /// be a strange answer to a paste the user meant as text.
    #[test]
    fn text_wins_when_the_clipboard_holds_both() {
        gtk_test(|| {
            let both = gdk::ContentProvider::new_union(&[
                gdk::ContentProvider::for_value(&a_texture().to_value()),
                gdk::ContentProvider::for_value(&"an image, and words".to_value()),
            ]);
            let clipboard = gdk::Display::default().expect("a display").clipboard();
            clipboard.set_content(Some(&both)).expect("clipboard set");

            assert!(
                !wants_image_paste(&clipboard.formats()),
                "a clipboard holding text *and* an image should paste the text; \
                 GDK advertised: {:?}",
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
