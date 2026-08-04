//! The editor body a pane can carry instead of a terminal.
//!
//! An editor pane is a tile like any other - it takes a cell in the grid, wears
//! the head strip and the focus lamp, and leaves through the close button -
//! because that is what this window *is*: a rack of tiles you arrange. The
//! first version floated in a dialog over the workspace, and floating was the
//! mistake; a dialog covers the very agents whose work you opened the file to
//! check on, and it answers to none of the layout keys your hands already know.
//!
//! What lives here is everything about *editing*: the buffer, the view, the
//! save/undo/redo controls, and the one question a dirty buffer gets asked on
//! its way out. What lives in `pane` is the tile itself - the head strip, the
//! dot, the frame - and `tiler` decides when the pane leaves. The seam between
//! them is the `Editor` struct: a body, a strip of controls, and a handful of
//! honest answers ("is there unsaved work?").
//!
//! The widget is GtkSourceView, and the dependency is argued for in
//! `Cargo.toml` where it is declared. What it refuses to open, it refuses out
//! loud and cheaply: a file that isn't UTF-8 text is not something this editor
//! can round-trip without corrupting it, and a file past a couple of megabytes
//! is not something anyone hand-edits from a sidebar's file tree. Both come
//! back as an `Err` sentence for the caller to toast, because a click that
//! silently does nothing reads as a broken app.

use std::path::{Path, PathBuf};

use adw::prelude::*;
use gtk4::glib;
use sourceview5::prelude::*;

/// Where the editor stops pretending to be an editor. Past this a file is a
/// log, a lockfile, or an artefact - things you inspect with a pager in a
/// terminal pane, which this app is full of - and loading it into an undo-
/// tracking, syntax-highlighting buffer buys seconds of stall for a file
/// nobody hand-edits.
const MAX_BYTES: usize = 2 * 1024 * 1024;

/// The file's name, for sentences about it. Lossy on purpose: this is display,
/// and a name that isn't clean UTF-8 still deserves a legible refusal.
fn name_of(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string_lossy().into_owned())
}

/// The file as editable text, or the sentence saying why it can't be.
///
/// UTF-8 strictly, not lossily: a lossy read replaces bytes, and an editor
/// that loads a file it would corrupt on save is a data-loss bug wearing a
/// convenience feature's clothes.
fn load(path: &Path) -> Result<String, String> {
    let name = name_of(path);
    let bytes = std::fs::read(path).map_err(|e| format!("Couldn't read {name}: {e}"))?;
    if bytes.len() > MAX_BYTES {
        return Err(format!(
            "{name} is {} KB - past the editor's {} KB limit",
            bytes.len() / 1024,
            MAX_BYTES / 1024,
        ));
    }
    String::from_utf8(bytes).map_err(|_| format!("{name} isn't UTF-8 text, so editing would corrupt it"))
}

/// One editor: the widget that shows a file, the buffer that holds it, and
/// the controls the pane's head strip packs. Everything GTK in here is a
/// reference-counted handle, which is what lets the tiler's close flow hold a
/// cheap clone while the pane owns the original.
///
/// "One editor", not "one open file": which file it holds changes over its
/// life (see `open`), which is why the path and name sit behind
/// `Rc<RefCell>`. A `Clone` of this struct is a second handle to the *same*
/// editor, the way its widget fields already are, and the clones the close
/// flow and the save shortcut hold have to see a switch. A derived clone of
/// a bare `RefCell` would copy the path instead of sharing it, and Ctrl+S
/// after switching files would write the new text over the old file.
#[derive(Clone)]
pub struct Editor {
    /// The body the pane frames: the error line, then the scrolled view.
    pub root: gtk4::Box,
    pub view: sourceview5::View,
    pub buffer: sourceview5::Buffer,
    /// Undo, redo, Save - built here so their sensitivity can be wired to the
    /// buffer they act on, packed by the pane whose strip they sit in.
    pub controls: gtk4::Box,
    path: std::rc::Rc<std::cell::RefCell<PathBuf>>,
    name: std::rc::Rc<std::cell::RefCell<String>>,
    /// Where a failed save says why, since an editor pane has no terminal to
    /// say it in and a toast belongs to the window, not the tile. Hidden until
    /// there is something to admit.
    error: gtk4::Label,
}

impl Editor {
    /// Opens `path` for editing, or says in a sentence why it won't.
    pub fn load(path: &Path) -> Result<Editor, String> {
        let content = load(path)?;

        let buffer = sourceview5::Buffer::new(None);
        if let Some(scheme) = sourceview5::StyleSchemeManager::default().scheme("Adwaita-dark") {
            buffer.set_style_scheme(Some(&scheme));
        }
        buffer.set_enable_undo(true);

        let view = sourceview5::View::with_buffer(&buffer);
        view.set_show_line_numbers(true);
        view.set_monospace(true);
        view.set_tab_width(4);
        view.set_highlight_current_line(true);
        view.set_top_margin(6);
        view.set_bottom_margin(6);
        view.set_left_margin(4);
        // Long lines wrap, because this editor lives in a *tile*. Every code
        // editor defaults to clipping and a horizontal scrollbar, and in a
        // full-width window that is the right call - but these panes get
        // narrow the moment another agent arrives, and a pane whose text
        // vanishes off its right edge reads as cut off, not as scrollable.
        // The terminals beside it reflow on every re-tile (their programs
        // redraw for the new width); a buffer has nobody to redraw it except
        // the view, so the view wraps. `WordChar` over `Word` so a minified
        // line with no spaces in it still folds instead of clipping.
        view.set_wrap_mode(gtk4::WrapMode::WordChar);

        let scrolled = gtk4::ScrolledWindow::builder()
            .child(&view)
            // Wrapping is what makes this honest: with lines folding to the
            // allocation there is never anything to the right to scroll to.
            .hscrollbar_policy(gtk4::PolicyType::Never)
            .vexpand(true)
            .hexpand(true)
            .build();

        let error = gtk4::Label::builder()
            .halign(gtk4::Align::Start)
            .ellipsize(gtk4::pango::EllipsizeMode::End)
            .css_classes(["editor-error"])
            .visible(false)
            .build();

        let root = gtk4::Box::builder()
            .orientation(gtk4::Orientation::Vertical)
            .build();
        root.append(&error);
        root.append(&scrolled);

        let editor = Editor {
            root,
            view,
            buffer,
            controls: gtk4::Box::builder()
                .orientation(gtk4::Orientation::Horizontal)
                .spacing(2)
                .build(),
            path: std::rc::Rc::new(std::cell::RefCell::new(PathBuf::new())),
            name: std::rc::Rc::new(std::cell::RefCell::new(String::new())),
            error,
        };
        editor.apply(path, &content);
        editor.build_controls();
        editor.install_save_key();
        Ok(editor)
    }

    /// What the sidebar strip calls this editor: the open file's name.
    pub fn name(&self) -> String {
        self.name.borrow().clone()
    }

    /// Whether `path` could be opened right now, with the same refusals `load`
    /// gives. For the switch path, which has to refuse *before* asking about
    /// unsaved changes - "Save changes?" answered carefully and then "that
    /// file is a PNG" is the dialog having wasted the answer.
    pub fn readable(path: &Path) -> Result<(), String> {
        load(path).map(|_| ())
    }

    /// Puts a different file in this editor - same tile, same buffer, new
    /// contents - or says why it won't and leaves the current file alone.
    /// The caller decides what unsaved changes mean first (see
    /// `Tiler::open_editor_pane`); by the time this runs the old file's fate
    /// is settled.
    pub fn open(&self, path: &Path) -> Result<(), String> {
        let content = load(path).inspect_err(|why| {
            // The switch was already agreed to, so a refusal here has to say
            // itself in the pane - there is no `Result` left to carry it.
            self.error.set_label(why);
            self.error.set_visible(true);
        })?;
        self.apply(path, &content);
        Ok(())
    }

    /// The one place a file's text, language and identity land in the widgets
    /// - `load` and `open` are both this, before and after construction.
    fn apply(&self, path: &Path, content: &str) {
        // The colour: the language guessed from the file's name, drawn in the
        // scheme set at construction. The lookup failing is fine - an unknown
        // extension simply arrives unhighlighted.
        self.buffer.set_language(
            sourceview5::LanguageManager::default()
                .guess_language(Some(path), None)
                .as_ref(),
        );

        // The load must not be a thing Ctrl+Z can take back: undo past it is
        // an empty buffer - or, worse, the *previous* file - one reflexive
        // keypress from being saved over this one.
        self.buffer.begin_irreversible_action();
        self.buffer.set_text(content);
        self.buffer.end_irreversible_action();
        self.buffer.set_modified(false);

        self.error.set_visible(false);
        *self.path.borrow_mut() = path.to_path_buf();
        *self.name.borrow_mut() = name_of(path);
    }

    /// The head strip's three verbs. Undo and redo answer to the buffer's own
    /// account of its history, bound rather than tracked: a button that
    /// decides for itself when undo is possible is a second copy of the truth,
    /// and the copies drift on the first edge case. Save lights up exactly
    /// while the buffer differs from the disk.
    fn build_controls(&self) {
        let undo = head_action("edit-undo-symbolic", "Undo (Ctrl+Z)");
        let redo = head_action("edit-redo-symbolic", "Redo (Ctrl+Shift+Z)");
        let save = head_action("document-save-symbolic", "Save (Ctrl+S)");
        save.add_css_class("pane-editor-save");
        save.set_sensitive(false);

        self.buffer
            .bind_property("can-undo", &undo, "sensitive")
            .sync_create()
            .build();
        self.buffer
            .bind_property("can-redo", &redo, "sensitive")
            .sync_create()
            .build();
        {
            let buffer = self.buffer.clone();
            undo.connect_clicked(move |_| buffer.undo());
        }
        {
            let buffer = self.buffer.clone();
            redo.connect_clicked(move |_| buffer.redo());
        }
        {
            let save_button = save.clone();
            self.buffer.connect_modified_changed(move |buffer| {
                save_button.set_sensitive(buffer.is_modified());
            });
        }
        {
            let editor = self.clone();
            save.connect_clicked(move |_| editor.save());
        }

        self.controls.append(&undo);
        self.controls.append(&redo);
        self.controls.append(&save);
    }

    /// Ctrl+S while typing, without reaching for the mouse. On the view rather
    /// than the pane's frame, because the keystroke belongs to the text you
    /// are editing - a Ctrl+S over some other pane's terminal must keep
    /// meaning whatever the program in that terminal says it means.
    fn install_save_key(&self) {
        let editor = self.clone();
        let shortcuts = gtk4::ShortcutController::new();
        shortcuts.add_shortcut(gtk4::Shortcut::new(
            gtk4::ShortcutTrigger::parse_string("<Control>s"),
            Some(gtk4::CallbackAction::new(move |_, _| {
                editor.save();
                glib::Propagation::Stop
            })),
        ));
        self.view.add_controller(shortcuts);
    }

    /// Writes the buffer back to disk. Success is silent - the Save button
    /// dimming and the dot going quiet are the receipt - and failure stays on
    /// screen in the error line until a save lands, with the buffer still
    /// modified so nothing anywhere claims a save that didn't happen.
    pub fn save(&self) {
        let text = self
            .buffer
            .text(&self.buffer.start_iter(), &self.buffer.end_iter(), true);
        // The path read at the moment of writing, not captured when the
        // button was wired: this editor switches files (see `open`), and a
        // save must land on the file whose text it is.
        let path = self.path.borrow().clone();
        match std::fs::write(&path, text.as_bytes()) {
            Ok(()) => {
                self.buffer.set_modified(false);
                self.error.set_visible(false);
            }
            Err(e) => {
                self.error.set_label(&format!("Couldn't save: {e}"));
                self.error.set_visible(true);
            }
        }
    }

    pub fn is_modified(&self) -> bool {
        self.buffer.is_modified()
    }

    /// Runs `close` now if nothing would be lost, and otherwise asks first -
    /// Save (the default: someone closing an editor with changes in it almost
    /// always meant to keep them), Discard, or stay. Answering Save only
    /// closes if the write actually landed; a full disk must not eat the one
    /// copy of the changes on its way out.
    pub fn confirm_close(&self, parent: &impl IsA<gtk4::Widget>, close: impl Fn() + 'static) {
        if !self.is_modified() {
            close();
            return;
        }
        let ask = adw::AlertDialog::new(
            Some("Save changes?"),
            Some(&format!("{} has unsaved changes.", self.name.borrow())),
        );
        ask.add_responses(&[
            ("cancel", "Keep editing"),
            ("discard", "Discard"),
            ("save", "Save"),
        ]);
        ask.set_response_appearance("discard", adw::ResponseAppearance::Destructive);
        ask.set_response_appearance("save", adw::ResponseAppearance::Suggested);
        ask.set_default_response(Some("save"));
        ask.set_close_response("cancel");
        let editor = self.clone();
        ask.connect_response(None, move |_, response| match response {
            "save" => {
                editor.save();
                if !editor.is_modified() {
                    close();
                }
            }
            "discard" => close(),
            _ => {}
        });
        ask.present(Some(parent));
    }
}

/// One small verb for the head strip, in the close button's own geometry so
/// the strip reads as one row of controls rather than two vocabularies.
fn head_action(icon: &str, tip: &str) -> gtk4::Button {
    gtk4::Button::builder()
        .icon_name(icon)
        .css_classes(["flat", "pane-editor-action"])
        .valign(gtk4::Align::Center)
        .can_focus(false)
        .tooltip_text(tip)
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::gtk_test;

    /// A scratch file with `bytes` in it, cleaned up on drop.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(tag: &str, bytes: &[u8]) -> Self {
            let path = std::env::temp_dir().join(format!("atc-editor-{tag}-{}", std::process::id()));
            std::fs::write(&path, bytes).expect("a scratch file");
            Scratch(path)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    /// The three refusals and the one acceptance, each in the words the toast
    /// would use. The UTF-8 one is the load-bearing case: a lossy read here
    /// would round-trip corruption into the file on the first save.
    #[test]
    fn only_utf8_text_of_editable_size_loads() {
        let text = Scratch::new("text", "fn main() {}\n".as_bytes());
        assert_eq!(load(&text.0).as_deref(), Ok("fn main() {}\n"));

        let binary = Scratch::new("binary", &[0x89, b'P', b'N', b'G', 0xff, 0xfe]);
        let why = load(&binary.0).expect_err("bytes that aren't text must not load");
        assert!(why.contains("UTF-8"), "the refusal should say why: {why}");

        let huge = Scratch::new("huge", &vec![b'x'; MAX_BYTES + 1]);
        let why = load(&huge.0).expect_err("a file past the cap must not load");
        assert!(why.contains("limit"), "the refusal should say why: {why}");

        let missing = std::env::temp_dir().join("atc-editor-no-such-file");
        assert!(load(&missing).is_err(), "a missing file is a refusal, not a panic");
    }

    /// The editor's whole undo contract, at the buffer level: the load itself
    /// is not undoable - Ctrl+Z past the load is an empty buffer one reflexive
    /// keypress from being saved - while the first real edit is.
    #[test]
    fn the_load_is_not_undoable_and_an_edit_is() {
        gtk_test(|| {
            let scratch = Scratch::new("undo", b"original");
            let editor = Editor::load(&scratch.0).expect("a text file loads");

            assert!(!editor.buffer.can_undo(), "nothing to undo straight after a load");
            assert!(!editor.is_modified(), "a fresh load has nothing to save");

            editor
                .buffer
                .insert(&mut editor.buffer.end_iter(), " plus an edit");
            assert!(editor.buffer.can_undo(), "a real edit must be undoable");
            assert!(editor.is_modified(), "a real edit must light the save button");

            editor.buffer.undo();
            let text =
                editor
                    .buffer
                    .text(&editor.buffer.start_iter(), &editor.buffer.end_iter(), true);
            assert_eq!(text, "original", "undo must stop at the loaded text");
            assert!(!editor.buffer.can_undo(), "and go no further back than the load");
        });
    }

    /// Save writes what the buffer holds and marks it clean; the file on disk
    /// is the proof, not the flag.
    #[test]
    fn saving_writes_the_edit_back_and_marks_the_buffer_clean() {
        gtk_test(|| {
            let scratch = Scratch::new("save", b"before");
            let editor = Editor::load(&scratch.0).expect("a text file loads");

            editor.buffer.insert(&mut editor.buffer.end_iter(), " after");
            editor.save();

            assert_eq!(
                std::fs::read_to_string(&scratch.0).expect("the file is still there"),
                "before after",
            );
            assert!(!editor.is_modified(), "a landed save leaves nothing to save");
        });
    }

    /// Switching files reuses the buffer: new text, new name, a clean flag,
    /// and a history that cannot reach back into the previous file - undo
    /// after a switch stopping at the new file's loaded text is what stands
    /// between "browse the tree" and saving one file's text over another.
    #[test]
    fn switching_files_swaps_text_name_and_history() {
        gtk_test(|| {
            let first = Scratch::new("switch-a", b"fn a() {}");
            let second = Scratch::new("switch-b", b"fn b() {}");
            let editor = Editor::load(&first.0).expect("a text file loads");
            editor
                .buffer
                .insert(&mut editor.buffer.end_iter(), " // edited");
            assert!(editor.is_modified());

            editor.open(&second.0).expect("the second file loads");
            let text =
                editor
                    .buffer
                    .text(&editor.buffer.start_iter(), &editor.buffer.end_iter(), true);
            assert_eq!(text, "fn b() {}", "the pane now holds the second file");
            assert_eq!(editor.name(), name_of(&second.0));
            assert!(!editor.is_modified(), "a freshly opened file has nothing to save");
            assert!(
                !editor.buffer.can_undo(),
                "undo must not reach back into the previous file",
            );

            // And a switch to something unreadable leaves the current file in
            // place, with the refusal reported rather than swallowed.
            let missing = std::env::temp_dir().join("atc-editor-switch-missing");
            assert!(editor.open(&missing).is_err());
            assert_eq!(editor.name(), name_of(&second.0), "a failed switch changes nothing");
        });
    }

    /// The language lookup the colour rides on: a `.rs` name resolves to Rust.
    /// This is as much a test of the packaging as of the code - it fails if the
    /// library is present but its language data didn't come with it.
    #[test]
    fn a_rust_filename_finds_its_language() {
        gtk_test(|| {
            let language = sourceview5::LanguageManager::default()
                .guess_language(Some("main.rs"), None)
                .expect("gtksourceview knows Rust");
            assert_eq!(language.id(), "rust");
        });
    }
}

