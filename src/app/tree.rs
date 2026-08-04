//! The folder tree a strip unfolds into.
//!
//! A strip names a folder and, until now, said nothing about what was in it -
//! the one fact about a project the rack could not answer without leaving the
//! app. The chevron on each strip unfolds this: the project's own directory,
//! one level at a time, read from disk at the moment it is asked for.
//!
//! Read on every unfold rather than cached, and that is a decision about what
//! this app is: its panes hold agents whose entire job is changing these files,
//! so a tree read once at startup is a tree that is wrong within a minute.
//! Reading lazily also means a directory nobody opens is a directory never
//! touched - `target/` with its hundred thousand build artefacts costs nothing
//! until someone actually unfolds it, and the cap below is what it meets if
//! they do.
//!
//! The listing itself - what is shown, in what order, and where it stops - is
//! plain Rust with no GTK in it, for the same reason `model` is: it is the part
//! with rules worth testing, and this way the tests run on a headless machine
//! where every GTK test skips.

use std::cmp::Ordering;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use gtk4::prelude::*;

/// What clicking a file does - handed in by the strip rather than decided
/// here, because the answer ("show it in a pane of this project") needs the
/// `App` and the project id, and this module deliberately knows neither. An
/// `Rc` because every level of the tree carries it down to the next: a file
/// three folders deep answers the same way a file at the root does.
pub(super) type OpenFile = dyn Fn(&Path);

/// Where a directory's listing stops. Past this the tree is no longer an index
/// of a folder, it is the folder, poured into a 170px column - and the rack has
/// panes beside it with a better claim on the space. The remainder is said in a
/// count instead (see `rebuild`), so a capped listing never silently reads as a
/// complete one.
const MAX_ENTRIES: usize = 100;

/// How far each level steps in from the one above it. Applied to the child
/// *container* rather than recomputed per row from a depth, which is what makes
/// nesting automatic: every level is one box inside another, each indented once.
const INDENT_PX: i32 = 14;

/// The width of the chevron slot on a folder row, so a file's icon sits on the
/// same vertical line as the folder icons around it - a file has no chevron,
/// and without the spacer the two kinds of row read as two ragged columns.
const CHEVRON_SLOT_PX: i32 = 22;

/// One thing a directory holds, as much of it as the tree draws.
struct Entry {
    name: String,
    is_dir: bool,
}

/// A directory's contents, ordered and capped, plus how many entries the cap
/// hid - carried alongside rather than dropped, so the tree can say so.
struct Listing {
    entries: Vec<Entry>,
    more: usize,
}

/// Folders first, then names ordered without regard to case, with the exact
/// name as the tie-break so two spellings of one word still land somewhere
/// stable. Case-insensitive because `README.md` sorting above every lowercase
/// file is ASCII's opinion of importance, not the user's.
fn ordered(a: &Entry, b: &Entry) -> Ordering {
    b.is_dir
        .cmp(&a.is_dir)
        .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        .then_with(|| a.name.cmp(&b.name))
}

/// Sorts and caps a raw listing. Split from `read_listing` so the rules have a
/// seam the tests can reach without a filesystem.
fn capped(mut entries: Vec<Entry>) -> Listing {
    entries.sort_by(ordered);
    let more = entries.len().saturating_sub(MAX_ENTRIES);
    entries.truncate(MAX_ENTRIES);
    Listing { entries, more }
}

/// What `dir` holds, ready to draw - or `None` when it can't be read, which the
/// tree reports rather than papers over: a project whose folder has been
/// deleted out from under it is a fact, not an empty list.
///
/// Dotfiles are skipped. The tree is an index of the project, and `.git` with
/// its thousands of objects is plumbing every project carries rather than
/// something anyone navigates - the terminal beside the rack is right there for
/// the times someone genuinely needs it.
///
/// A symlink is asked what it points at (`Path::is_dir` follows it), so a
/// linked directory unfolds like any other. A loop of links can't run away with
/// that: each level is only ever read when someone clicks it open.
fn read_listing(dir: &Path) -> Option<Listing> {
    let reader = std::fs::read_dir(dir).ok()?;
    let mut entries = Vec::new();
    for entry in reader.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        // An entry whose type can't be read is dropped rather than guessed at;
        // it is one unreadable name, not a reason to abandon the other ninety.
        let Ok(ftype) = entry.file_type() else {
            continue;
        };
        let is_dir = if ftype.is_symlink() {
            entry.path().is_dir()
        } else {
            ftype.is_dir()
        };
        entries.push(Entry { name, is_dir });
    }
    Some(capped(entries))
}

/// Empties `container` and fills it with `dir`'s listing, one row per entry.
/// This is the whole interface: the strip calls it on unfold, and a folder row
/// calls it on itself when clicked open - which is all "lazily, one level at a
/// time" amounts to. `on_open` is what a clicked file does.
pub(super) fn rebuild(container: &gtk4::Box, dir: &Path, on_open: &Rc<OpenFile>) {
    while let Some(child) = container.first_child() {
        container.remove(&child);
    }
    let Some(listing) = read_listing(dir) else {
        container.append(&note("unreadable"));
        return;
    };
    if listing.entries.is_empty() {
        container.append(&note("empty"));
        return;
    }
    for entry in &listing.entries {
        if entry.is_dir {
            container.append(&folder_branch(dir.join(&entry.name), &entry.name, on_open));
        } else {
            container.append(&file_row(dir.join(&entry.name), &entry.name, on_open));
        }
    }
    if listing.more > 0 {
        container.append(&note(&format!("+{} more", listing.more)));
    }
}

/// A folder row and, beneath it, the box its own listing unfolds into.
///
/// The child box starts hidden and *empty*: clicking the row open is what reads
/// the directory, so the cost of a level is paid at the moment someone asks for
/// it and re-paid - fresh - each time they ask again. Closing only hides it;
/// the stale contents are invisible, and the next open overwrites them.
fn folder_branch(path: PathBuf, name: &str, on_open: &Rc<OpenFile>) -> gtk4::Box {
    let chevron = gtk4::Image::from_icon_name("pan-end-symbolic");
    let content = gtk4::Box::builder()
        .orientation(gtk4::Orientation::Horizontal)
        .spacing(6)
        .build();
    content.append(&chevron);
    content.append(&gtk4::Image::from_icon_name("folder-symbolic"));
    content.append(&name_label(name));

    let head = gtk4::Button::builder()
        .child(&content)
        .css_classes(["sidebar-tree-dir"])
        .can_focus(false)
        .build();

    let kids = gtk4::Box::builder()
        .orientation(gtk4::Orientation::Vertical)
        .margin_start(INDENT_PX)
        .visible(false)
        .build();

    let branch = gtk4::Box::builder()
        .orientation(gtk4::Orientation::Vertical)
        .build();
    branch.append(&head);
    branch.append(&kids);

    // Strong references, and deliberately so: the button holds its own chevron
    // and its sibling box, neither of which refers back to it, so nothing here
    // is a cycle - when the branch goes, all three go.
    let kids_of_click = kids.clone();
    let on_open = on_open.clone();
    head.connect_clicked(move |_| {
        if kids_of_click.is_visible() {
            kids_of_click.set_visible(false);
            chevron.set_icon_name(Some("pan-end-symbolic"));
        } else {
            rebuild(&kids_of_click, &path, &on_open);
            kids_of_click.set_visible(true);
            chevron.set_icon_name(Some("pan-down-symbolic"));
        }
    });
    branch
}

/// A file row: click it and `on_open` shows the file. The action itself lives
/// with the caller (the editor dialog - see `editor::present`), and it is the
/// one thing a click may do; a click must never write, run, or delete anything
/// on the strength of a name in a list.
fn file_row(path: PathBuf, name: &str, on_open: &Rc<OpenFile>) -> gtk4::Button {
    let content = gtk4::Box::builder()
        .orientation(gtk4::Orientation::Horizontal)
        .spacing(6)
        .build();
    content.append(&gtk4::Image::from_icon_name("text-x-generic-symbolic"));
    content.append(&name_label(name));

    let row = gtk4::Button::builder()
        .child(&content)
        .margin_start(CHEVRON_SLOT_PX)
        .css_classes(["sidebar-tree-file"])
        .can_focus(false)
        .tooltip_text("Open this file in the editor")
        .build();
    let on_open = on_open.clone();
    row.connect_clicked(move |_| on_open(&path));
    row
}

/// The one shape every name in the tree takes: start-aligned, ellipsized, and
/// greedy for the width - the rack is narrow and a deep path has to lose its
/// tail rather than widen the sidebar.
fn name_label(name: &str) -> gtk4::Label {
    gtk4::Label::builder()
        .label(name)
        .halign(gtk4::Align::Start)
        .hexpand(true)
        .ellipsize(gtk4::pango::EllipsizeMode::End)
        .css_classes(["sidebar-tree-label"])
        .build()
}

/// The tree's asides - "empty", "unreadable", "+30 more" - set apart from the
/// names so a fact about the listing is never mistaken for a file called that.
fn note(text: &str) -> gtk4::Label {
    gtk4::Label::builder()
        .label(text)
        .halign(gtk4::Align::Start)
        .css_classes(["sidebar-tree-note"])
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::gtk_test;

    fn entry(name: &str, is_dir: bool) -> Entry {
        Entry {
            name: name.to_string(),
            is_dir,
        }
    }

    /// A scratch directory of this test's own, cleaned up on drop so a failed
    /// assertion doesn't leave litter for the next run to trip over.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("atc-tree-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("a scratch directory");
            Scratch(dir)
        }

        fn touch(&self, name: &str) {
            std::fs::write(self.0.join(name), b"").expect("a scratch file");
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// The order is the tree's whole opinion: folders above files, names
    /// compared without regard to case, so `README.md` files don't float to the
    /// top of every project on the strength of an ASCII table.
    #[test]
    fn folders_sort_first_and_case_carries_no_weight() {
        let listing = capped(vec![
            entry("zeta.rs", false),
            entry("README.md", false),
            entry("assets", true),
            entry("alpha.rs", false),
            entry("Src", true),
        ]);
        let names: Vec<&str> = listing.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["assets", "Src", "alpha.rs", "README.md", "zeta.rs"]);
        assert_eq!(listing.more, 0);
    }

    /// The cap trims the listing and *says so* - `more` is what keeps a capped
    /// directory from quietly reading as a complete one.
    #[test]
    fn a_listing_past_the_cap_is_trimmed_and_counted() {
        let listing = capped((0..MAX_ENTRIES + 30).map(|i| entry(&format!("file-{i:04}"), false)).collect());
        assert_eq!(listing.entries.len(), MAX_ENTRIES);
        assert_eq!(listing.more, 30);
    }

    /// Dotfiles stay out, folders are told from files, and a folder that isn't
    /// there is `None` rather than an empty listing - deleted and empty are
    /// different facts and the tree reports which.
    #[test]
    fn the_listing_reads_what_is_there_and_admits_what_is_not() {
        let scratch = Scratch::new("read");
        scratch.touch("main.rs");
        scratch.touch(".hidden");
        std::fs::create_dir(scratch.0.join("src")).expect("a scratch subfolder");

        let listing = read_listing(&scratch.0).expect("a readable folder lists");
        let shape: Vec<(&str, bool)> = listing
            .entries
            .iter()
            .map(|e| (e.name.as_str(), e.is_dir))
            .collect();
        assert_eq!(shape, [("src", true), ("main.rs", false)]);

        assert!(
            read_listing(&scratch.0.join("no-such-folder")).is_none(),
            "a missing folder must be unreadable, not empty",
        );
    }

    /// The widget end of the bargain: pointing `rebuild` at a folder draws one
    /// row per entry, and pointing it somewhere unreadable draws the note that
    /// says so rather than nothing.
    #[test]
    fn rebuilding_draws_the_listing_it_read() {
        gtk_test(|| {
            let scratch = Scratch::new("widget");
            scratch.touch("a.rs");
            scratch.touch("b.rs");
            std::fs::create_dir(scratch.0.join("src")).expect("a scratch subfolder");
            let open: Rc<OpenFile> = Rc::new(|_| {});

            let container = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
            rebuild(&container, &scratch.0, &open);

            let mut rows = 0;
            let mut child = container.first_child();
            while let Some(widget) = child {
                rows += 1;
                child = widget.next_sibling();
            }
            assert_eq!(rows, 3, "one row per entry, nothing extra");

            // Rebuilding replaces rather than appends - the strip calls this on
            // every unfold, and a tree that grew by one copy per unfold would
            // be the first thing anyone noticed.
            rebuild(&container, &scratch.0, &open);
            let mut rows = 0;
            let mut child = container.first_child();
            while let Some(widget) = child {
                rows += 1;
                child = widget.next_sibling();
            }
            assert_eq!(rows, 3, "a rebuild replaces the rows it drew last time");
        });
    }

    /// Clicking a file row hands the file's whole path to the opener - the one
    /// thing the tree promises the strip that wired it. The folder sorts first,
    /// so the first `Button` child is the first *file*; a folder row is a `Box`
    /// holding its own button, which is what keeps this lookup honest.
    #[test]
    fn clicking_a_file_hands_its_path_to_the_opener() {
        gtk_test(|| {
            use std::cell::RefCell;

            let scratch = Scratch::new("click");
            scratch.touch("main.rs");
            std::fs::create_dir(scratch.0.join("src")).expect("a scratch subfolder");

            let opened: Rc<RefCell<Option<PathBuf>>> = Rc::new(RefCell::new(None));
            let sink = opened.clone();
            let open: Rc<OpenFile> = Rc::new(move |path| {
                *sink.borrow_mut() = Some(path.to_path_buf());
            });

            let container = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
            rebuild(&container, &scratch.0, &open);

            let mut child = container.first_child();
            let mut file_button = None;
            while let Some(widget) = child {
                child = widget.next_sibling();
                if let Ok(button) = widget.downcast::<gtk4::Button>() {
                    file_button = Some(button);
                    break;
                }
            }
            file_button
                .expect("a file row is a button")
                .emit_clicked();
            assert_eq!(
                opened.borrow().as_deref(),
                Some(scratch.0.join("main.rs").as_path()),
                "the opener got a different file than the one clicked",
            );
        });
    }
}
