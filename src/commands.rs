//! The command palette: everything the app can do, found by typing.
//!
//! The app's keys are all under Super+Alt, which is deliberate and has a cost.
//! It means no binding collides with the shell, claude or readline inside a
//! pane - and it means there are two dozen of them, all reachable only if you
//! remember which letter. The cheatsheet answers "what was the key for this?"
//! but you have to know to open it, and it can't do anything but tell you.
//!
//! This answers the other question, which is the one people actually have:
//! "can this app do the thing I want?" You type a word from it and press Enter.
//!
//! Everything here comes from `keybindings::COMMANDS`, so a command cannot
//! exist and be missing from the palette - that is the whole reason the command
//! table was unified first. The only entries built here rather than read are the
//! ones that can't be constants: one per open project.
//!
//! Reached by Super+Alt+P, and not the Ctrl+Shift+P this idiom is usually bound
//! to. Ctrl+Shift+P is a key combination the thing inside a pane may well want,
//! and the rule that keeps every other binding out of the terminal's way isn't
//! worth breaking for familiarity.

use adw::prelude::*;
use gtk4::glib;

use crate::app::App;
use crate::keybindings::{Action, COMMANDS};

/// How many rows the list shows before it scrolls. Enough that a short query
/// resolves to something you can see the whole of, few enough that the dialog
/// stays a lens over your work rather than a window in front of it.
const VISIBLE_ROWS: i32 = 9;

/// One thing the palette can do.
struct Entry {
    title: String,
    /// The group it came from - a section name, or "Project" for the ones built
    /// per open project. Shown beside the title, because "Grid" and "agenttilecli"
    /// are not self-describing in a flat list of thirty things.
    context: String,
    accelerator: &'static str,
    run: Box<dyn Fn(&App)>,
}

/// Every entry, in the order an empty query shows them.
///
/// Projects first. With no query the palette is a switcher - the likeliest
/// reason to have opened it without typing is to go somewhere - and the
/// commands below are the answer to a query rather than to a blank field.
fn entries(app: &App) -> Vec<Entry> {
    let mut entries = Vec::new();

    for (id, name, is_active) in app.project_list() {
        // The open one is listed and marked rather than hidden. A switcher that
        // silently omits where you already are makes you check the sidebar to
        // find out whether it omitted anything.
        let context = if is_active {
            "Project · open"
        } else {
            "Project"
        };
        entries.push(Entry {
            title: format!("Switch to {name}"),
            context: context.to_string(),
            accelerator: "",
            run: Box::new(move |app: &App| app.switch_to_project(id)),
        });
    }

    for command in COMMANDS {
        let Some(action) = command.run else {
            continue;
        };
        entries.push(Entry {
            title: command.title.to_string(),
            context: command.section.to_string(),
            accelerator: command.accelerator,
            run: Box::new(move |app: &App| run_action(app, action)),
        });
    }

    entries
}

/// Runs a table command from the palette.
///
/// A pane command with no project open does nothing at all, which is the same
/// answer the keyboard gives - except that the keyboard's version passes the
/// keystroke on to whatever is focused, and there is nothing to pass on here.
fn run_action(app: &App, action: Action) {
    match action {
        Action::App(run) => run(app),
        Action::Tiler(run) => {
            if let Some(tiler) = app.active_tiler() {
                run(&tiler);
            }
        }
    }
}

/// How well `query` matches `text`, or `None` if it doesn't.
///
/// Subsequence matching rather than substring: "nxp" finds "Switch to the
/// **n**e**x**t **p**roject", which is the behaviour that makes a palette worth
/// typing into rather than scrolling. Case-insensitive over ASCII, which is what
/// every string in the table is.
///
/// The score is "lower is better" and rewards two things: matching early in the
/// string, and matching in a run rather than scattered across it. Without the
/// second, "grid" would rank a string containing g...r...i...d over the one that
/// says "grid", which is the failure that makes people stop trusting the ranking
/// and read the whole list anyway.
fn score(query: &str, text: &str) -> Option<u32> {
    if query.is_empty() {
        return Some(0);
    }

    let text: Vec<char> = text.chars().flat_map(char::to_lowercase).collect();
    let mut score = 0;
    let mut at = 0;
    let mut previous: Option<usize> = None;

    for needle in query.chars().flat_map(char::to_lowercase) {
        let found = text[at..].iter().position(|c| *c == needle)? + at;
        score += match previous {
            // A gap since the last match costs what it spans; adjacency is free.
            Some(previous) => (found - previous - 1) as u32,
            // The first match costs where it is, so an early hit wins.
            None => found as u32,
        };
        previous = Some(found);
        at = found + 1;
    }

    // Among equal matches, prefer the shorter string: it is the one with less
    // unmatched text around the hit.
    Some(score * 4 + text.len() as u32)
}

/// Opens the palette over `app`'s window.
pub fn present(app: &App) {
    let entries = std::rc::Rc::new(entries(app));

    let search = gtk4::SearchEntry::builder()
        .placeholder_text("Search for a command or a project")
        .hexpand(true)
        .build();

    let list = gtk4::ListBox::builder()
        .selection_mode(gtk4::SelectionMode::Browse)
        .css_classes(["palette-list"])
        .build();

    let scrolled = gtk4::ScrolledWindow::builder()
        .hscrollbar_policy(gtk4::PolicyType::Never)
        .propagate_natural_height(true)
        .max_content_height(VISIBLE_ROWS * 44)
        .vexpand(true)
        .child(&list)
        .build();

    let empty = gtk4::Label::builder()
        .label("Nothing matches")
        .css_classes(["palette-empty"])
        .visible(false)
        .build();

    let content = gtk4::Box::builder()
        .orientation(gtk4::Orientation::Vertical)
        .css_classes(["palette"])
        .build();
    content.append(&search);
    content.append(&scrolled);
    content.append(&empty);

    // A fixed width, and explicitly *not* `follows-content-size`. Every row's
    // title ellipsizes, so its natural width is a few characters - which means
    // a dialog that sizes to its content sizes to nothing, and every command in
    // it comes out as "Open a n...". The height still follows the list, via the
    // scroller's `propagate-natural-height`.
    let dialog = adw::Dialog::builder()
        .title("Commands")
        .presentation_mode(adw::DialogPresentationMode::Floating)
        .content_width(640)
        .child(&content)
        .build();

    // Rebuilding the rows per keystroke rather than filtering in place. The
    // list is a few dozen entries and the ranking reorders them, which a
    // `set_filter_func` cannot do - and a palette that filters without
    // reordering puts the best match wherever it happened to be declared.
    let rebuild = {
        let list = list.clone();
        let entries = entries.clone();
        let empty = empty.clone();
        let scrolled = scrolled.clone();
        move |query: &str| {
            while let Some(row) = list.first_child() {
                list.remove(&row);
            }

            let mut ranked: Vec<(u32, usize)> = entries
                .iter()
                .enumerate()
                .filter_map(|(i, entry)| {
                    // Matched against the context too, so "layout" finds every
                    // layout command whether or not the word is in its title.
                    let haystack = format!("{} {}", entry.title, entry.context);
                    score(query, &haystack).map(|s| (s, i))
                })
                .collect();
            // `sort_by_key` is stable, so equal scores keep declaration order -
            // which is what puts projects above commands on an empty query.
            ranked.sort_by_key(|(score, _)| *score);

            for (_, index) in &ranked {
                list.append(&build_row(&entries[*index], *index));
            }

            let any = !ranked.is_empty();
            empty.set_visible(!any);
            scrolled.set_visible(any);
            if let Some(first) = list.row_at_index(0) {
                list.select_row(Some(&first));
            }
        }
    };
    rebuild("");

    let rebuild = std::rc::Rc::new(rebuild);
    {
        let rebuild = rebuild.clone();
        search.connect_search_changed(move |entry| rebuild(&entry.text()));
    }

    // Activating a row: close first, then run. Several commands open a dialog
    // of their own, and presenting one from inside a dialog that is still up
    // stacks them.
    let activate = {
        let entries = entries.clone();
        let app = app.clone();
        let dialog = dialog.clone();
        move |row: &gtk4::ListBoxRow| {
            let Ok(index) = row.widget_name().parse::<usize>() else {
                return;
            };
            dialog.close();
            if let Some(entry) = entries.get(index) {
                (entry.run)(&app);
            }
        }
    };

    {
        let activate = activate.clone();
        list.connect_row_activated(move |_, row| activate(row));
    }

    // Enter runs whatever is selected. The entry keeps focus the whole time -
    // you are typing - so the list never sees the keypress itself.
    {
        let list = list.clone();
        let activate = activate.clone();
        search.connect_activate(move |_| {
            if let Some(row) = list.selected_row() {
                activate(&row);
            }
        });
    }

    // Up and down move the selection without leaving the entry, which is the
    // whole interaction: type, arrow, Enter, never touching the mouse or losing
    // the cursor out of the search field.
    let keys = gtk4::EventControllerKey::new();
    keys.set_propagation_phase(gtk4::PropagationPhase::Capture);
    {
        let list = list.clone();
        keys.connect_key_pressed(move |_, key, _, _| {
            let delta = match key {
                gtk4::gdk::Key::Down => 1,
                gtk4::gdk::Key::Up => -1,
                _ => return glib::Propagation::Proceed,
            };
            let current = list.selected_row().map_or(0, |row| row.index());
            let next = current + delta;
            if let Some(row) = list.row_at_index(next.max(0)) {
                list.select_row(Some(&row));
                row.grab_focus();
                // Focus moves to the row to scroll it into view; give the
                // keyboard straight back, or the next character typed goes
                // into the list instead of the query.
                return glib::Propagation::Stop;
            }
            glib::Propagation::Stop
        });
    }
    search.add_controller(keys);

    dialog.present(Some(app.window()));
    search.grab_focus();
}

/// One row: what it does, where it came from, and its key if it has one.
fn build_row(entry: &Entry, index: usize) -> gtk4::ListBoxRow {
    let title = gtk4::Label::builder()
        .label(&entry.title)
        .xalign(0.0)
        .hexpand(true)
        .ellipsize(gtk4::pango::EllipsizeMode::End)
        .css_classes(["palette-title"])
        .build();

    // Right-aligned against the key, so the three columns read as title, then
    // where-it-came-from, then how-to-reach-it, rather than as a title with a
    // word stuck to the end of it.
    let context = gtk4::Label::builder()
        .label(&entry.context)
        .xalign(1.0)
        .css_classes(["palette-context"])
        .build();

    let row_box = gtk4::Box::builder()
        .orientation(gtk4::Orientation::Horizontal)
        .spacing(10)
        .css_classes(["palette-row"])
        .build();
    row_box.append(&title);
    row_box.append(&context);

    if !entry.accelerator.is_empty() {
        row_box.append(
            &gtk4::ShortcutLabel::builder()
                .accelerator(entry.accelerator)
                .valign(gtk4::Align::Center)
                .build(),
        );
    }

    // The index into `entries`, carried in the widget name - the same channel
    // the sidebar uses to put a `ProjectId` on a row. It has to be carried
    // *somewhere* because the rows are rebuilt and reordered on every keystroke,
    // so a row's position in the list says nothing about which entry it is.
    gtk4::ListBoxRow::builder()
        .child(&row_box)
        .name(index.to_string())
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_query_matches_everything() {
        assert_eq!(score("", "anything"), Some(0));
    }

    #[test]
    fn a_subsequence_matches_and_a_missing_letter_does_not() {
        assert!(score("nxp", "Switch to the next project").is_some());
        assert!(score("zzz", "Switch to the next project").is_none());
    }

    #[test]
    fn matching_is_case_insensitive() {
        assert!(score("GRID", "Use the grid layout").is_some());
        assert!(score("grid", "Use the GRID layout").is_some());
    }

    /// The ranking's whole job. A palette that returns the right set in the
    /// wrong order is one people scroll instead of trust.
    #[test]
    fn a_run_of_letters_beats_the_same_letters_scattered() {
        let tight = score("grid", "Use the grid layout").expect("matches");
        let scattered = score("grid", "Growing a rapid indigo dream").expect("matches");
        assert!(
            tight < scattered,
            "scattered {scattered} should rank worse than tight {tight}",
        );
    }

    #[test]
    fn an_earlier_match_beats_a_later_one() {
        let early = score("pane", "Pane counting").expect("matches");
        let late = score("pane", "Counting every pane").expect("matches");
        assert!(
            early < late,
            "early {early} should rank better than late {late}"
        );
    }

    /// Ties are broken toward the shorter string, so "Preferences" wins over a
    /// long sentence that merely contains the same letters early on.
    #[test]
    fn a_shorter_match_wins_a_tie() {
        let short = score("pre", "Preferences").expect("matches");
        let long = score("pre", "Preferences, and a great deal more besides").expect("matches");
        assert!(
            short < long,
            "short {short} should rank better than long {long}"
        );
    }
}
