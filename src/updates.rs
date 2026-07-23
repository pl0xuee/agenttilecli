//! The update control - the check it runs off the main loop, and how the answer
//! gets reported.
//!
//! Lifted out of what used to be `groups.rs`, where it sat among sidebar rows,
//! drag-and-drop and font scaling for no better reason than that its button
//! happened to live in the sidebar. None of it has anything to do with tiling.
//!
//! It owns no widget at all now. There was a button here that both started a
//! check and reported the answer, in the sidebar footer - which put a second
//! "Check for updates" on screen next to the app menu's, and put it somewhere
//! you had to open the sidebar to see. What's left publishes a `State` and lets
//! the menu draw it, which is also what stops the two disagreeing.
//!
//! What the answer looks like is now graded by how much there is to say.
//! "You're up to date" is one line and no decision, so it's a toast. An offer to
//! install is a decision with consequences (every agent in every group is about
//! to be hung up), so it's a dialog. A check that *failed* is also a dialog,
//! despite carrying no decision: the reasons run to several paragraphs and end
//! in something the user has to go and do by hand, which is more than a toast
//! that disappears on its own can carry.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adw::prelude::*;
use gtk4::{gio, glib};

use crate::update;

/// What a finished check says about whether an update exists - or `None` when
/// it says nothing at all, because the check couldn't be *made*.
///
/// That third case is the whole reason this isn't a bool: a check that failed
/// (no network, GitHub down) hasn't discovered that a previously-found update
/// went away. Clearing the button on it would throw away a true answer and
/// replace it with no answer, so `Failed` leaves the last one standing.
fn update_available(status: &update::Status) -> Option<bool> {
    match status {
        update::Status::UpToDate => Some(false),
        update::Status::Available(_) => Some(true),
        update::Status::Failed(_) => None,
    }
}

/// What the update control has to say for itself, handed to whoever is drawing
/// it. Both flags come from the same place and are reported together, so a
/// caption and a highlight painted from this cannot drift apart - which is the
/// property the sidebar button used to hold on its own.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct State {
    /// The last *conclusive* check found a newer version.
    pub available: bool,
    /// A check is running right now.
    pub checking: bool,
}

struct Inner {
    /// Told the state whenever it changes - see `Updates::set_state_callback`.
    on_state: RefCell<Option<Box<dyn Fn(State)>>>,
    /// What the last *conclusive* check found - the single source of truth
    /// behind `notify`, so every rendering of this fact is painted from the
    /// same one and they can't drift apart.
    available: Cell<bool>,
    /// Whether a check is already running, so a second one can't be started on
    /// top of it. The keybinding doesn't go through the button, so the button's
    /// own sensitivity can't be what enforces this.
    in_flight: Cell<bool>,
    toasts: adw::ToastOverlay,
    /// Handed the shell command that performs the update. The caller runs it in
    /// a pane and restarts the app if it succeeds - neither of which is this
    /// module's business, and both of which need the project stack.
    on_install: RefCell<Option<Box<dyn Fn(String)>>>,
}

/// The update button and everything behind it.
#[derive(Clone)]
pub struct Updates(Rc<Inner>);

impl Updates {
    pub fn new(toasts: &adw::ToastOverlay) -> Self {
        Updates(Rc::new(Inner {
            on_state: RefCell::new(None),
            available: Cell::new(false),
            in_flight: Cell::new(false),
            toasts: toasts.clone(),
            on_install: RefCell::new(None),
        }))
    }

    /// Registers what draws this control's state, and paints it once
    /// immediately so the caller starts in step rather than a check behind.
    ///
    /// This used to be a button in the sidebar footer that both started a check
    /// and reported the answer. It reports through here instead, because the
    /// place the answer lands moved into the app menu, and because a control
    /// that renders itself can only ever be in one place at a time.
    pub fn set_state_callback(&self, f: impl Fn(State) + 'static) {
        *self.0.on_state.borrow_mut() = Some(Box::new(f));
        self.notify();
    }

    /// This control's current state.
    pub fn state(&self) -> State {
        State {
            available: self.0.available.get(),
            checking: self.0.in_flight.get(),
        }
    }

    fn notify(&self) {
        if let Some(cb) = self.0.on_state.borrow().as_ref() {
            cb(self.state());
        }
    }

    /// Registers what to do when the user accepts an update: run this shell
    /// command in a pane, and restart if it exits cleanly.
    pub fn set_install_callback(&self, f: impl Fn(String) + 'static) {
        *self.0.on_install.borrow_mut() = Some(Box::new(f));
    }

    /// Checks `origin/master` for a newer release and reports back.
    ///
    /// The git work runs on Gio's blocking-IO pool rather than the main loop: it
    /// fetches over the network, and a UI frozen for however long GitHub takes
    /// to answer - or for however long it takes to *not* answer, on a flaky
    /// connection - isn't something to inflict on someone who clicked a button
    /// out of idle curiosity.
    pub fn check(&self) {
        // The button desensitizes itself below, but the keybinding path doesn't
        // go through the button at all - so the guard, not the button, is what
        // actually stops two overlapping checks.
        if self.0.in_flight.get() {
            return;
        }
        self.0.in_flight.set(true);
        self.notify();

        let this = self.clone();
        glib::spawn_future_local(async move {
            let status = gio::spawn_blocking(update::check).await;

            this.0.in_flight.set(false);

            match status {
                Ok(status) => this.show(status),
                // `check` has no panicking path, but a control stuck on
                // "Checking..." forever is the one outcome worse than a dialog
                // saying so.
                Err(_) => {
                    this.notify();
                    this.alert(
                        "Couldn't check for updates",
                        "The update check crashed unexpectedly.",
                    );
                }
            }
        });
    }

    fn show(&self, status: update::Status) {
        if let Some(available) = update_available(&status) {
            self.0.available.set(available);
        }
        // Reported after a check even when the answer didn't change, because
        // this is also what clears "Checking...". An available update is left
        // saying so after the dialog closes, so "not now" doesn't also mean
        // "and never mention it again" - it's the only trace it leaves.
        self.notify();

        match status {
            // One line, no decision, nothing to act on - so it says its piece
            // and gets out of the way rather than taking over the window.
            update::Status::UpToDate => {
                self.toast(&format!("Up to date \u{b7} AgentTileCLI {}", update::version()))
            }
            update::Status::Failed(reason) => self.alert("Couldn't check for updates", &reason),
            update::Status::Available(update) => self.offer(update),
        }
    }

    /// The "here's what's new" dialog. Installing runs the pull and rebuild in a
    /// *pane* rather than behind a spinner: it's a `cargo build` of a GTK app,
    /// it takes a while, and watching it is both more reassuring and more useful
    /// than a frozen dialog if it goes wrong.
    fn offer(&self, update: update::Update) {
        let repo = update::repo_dir();
        let plural = if update.commits == 1 {
            "commit"
        } else {
            "commits"
        };
        let mut detail = format!(
            "origin/master is {} {plural} ahead of this build ({}):\n",
            update.commits,
            update::version(),
        );
        for subject in &update.subjects {
            detail.push_str(&format!("\n  \u{2022} {subject}"));
        }
        if update.commits > update.subjects.len() {
            let rest = update.commits - update.subjects.len();
            detail.push_str(&format!("\n  \u{2022} \u{2026}and {rest} more"));
        }

        if let Some(reason) = &update.blocked {
            detail.push_str(&format!(
                "\n\nThis build's checkout can't be updated for you, because {reason}:\n\n\
                 {repo}\n\n\
                 Sort that out and update it by hand - nothing there has been touched.",
            ));
            self.alert("Update available", &detail);
            return;
        }

        // The warning about the agents is the point of this paragraph. Updating
        // ends in a restart, and a restart hangs up every agent in every group -
        // which is a fine trade if you know it's coming, and an unpleasant
        // surprise if you don't. This dialog is the last moment it can be said,
        // so it gets said plainly, next to the button that does it.
        detail.push_str(&format!(
            "\n\nUpdating fast-forwards {repo} to origin/master and re-runs ./install.sh, \
             in a new pane so you can watch it. AgentTileCLI then restarts itself into the \
             new version - which closes every agent you have running, in every group.\n\n\
             If the update fails, nothing is restarted and the pane stays open with the \
             reason.",
        ));

        let dialog = adw::AlertDialog::new(Some("Update available"), Some(&detail));
        dialog.add_responses(&[("cancel", "Not now"), ("update", "Update")]);
        dialog.set_response_appearance("update", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("update"));
        // Escape and a dismissed dialog both report this, so backing out by any
        // route leaves the checkout exactly as it is.
        dialog.set_close_response("cancel");

        let this = self.clone();
        dialog.connect_response(None, move |_, response| {
            if response != "update" {
                return;
            }
            match update::command() {
                Ok(command) => {
                    if let Some(install) = this.0.on_install.borrow().as_ref() {
                        install(command);
                    }
                }
                Err(reason) => this.alert("Couldn't start the update", &reason),
            }
        });
        dialog.present(Some(&self.0.toasts));
    }

    /// A transient one-line report, for news that carries no decision.
    fn toast(&self, message: &str) {
        self.0.toasts.add_toast(adw::Toast::new(message));
    }

    /// A one-button dialog, for a report too long or too consequential to hand
    /// to a toast that vanishes on its own.
    pub fn alert(&self, heading: &str, body: &str) {
        let dialog = adw::AlertDialog::new(Some(heading), Some(body));
        dialog.add_responses(&[("ok", "OK")]);
        dialog.set_default_response(Some("ok"));
        dialog.set_close_response("ok");
        dialog.present(Some(&self.0.toasts));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::gtk_test;

    fn updates() -> Updates {
        Updates::new(&adw::ToastOverlay::new())
    }

    /// Starting a check must hand the (network-bound) work off to another thread
    /// and lock out a second one - not run it on the main loop.
    ///
    /// The check itself is deliberately never allowed to *finish* here: this
    /// test doesn't iterate the main loop, so the future stays parked at its
    /// first await and no dialog is ever shown. What's under test is the wiring
    /// up to that point; `update`'s own tests cover what the check then decides.
    #[test]
    fn a_running_check_locks_out_a_second_one() {
        gtk_test(|| {
            let updates = updates();
            assert!(!updates.state().checking);

            updates.check();
            assert!(updates.state().checking, "check runs off the main loop");

            // Nothing about the *control* enforces this - the menu item and the
            // keybinding both call straight through - so the flag has to.
            updates.check();
            assert!(updates.state().checking);
        });
    }

    /// A check that couldn't be *made* must not be read as "no update": it says
    /// nothing either way, and clearing the button on it would throw away a true
    /// answer the last check had already found.
    #[test]
    fn a_failed_check_says_nothing_about_whether_an_update_exists() {
        use update::{Status, Update};

        let available = || {
            Status::Available(Update {
                commits: 1,
                subjects: vec!["a shiny new feature".to_string()],
                blocked: None,
            })
        };

        assert_eq!(update_available(&available()), Some(true));
        assert_eq!(update_available(&Status::UpToDate), Some(false));
        assert_eq!(
            update_available(&Status::Failed("no network".to_string())),
            None,
            "a failed check leaves the last answer standing",
        );
    }

    /// Whoever draws this control is told every time the state moves, and told
    /// the whole of it at once. Both halves matter: a renderer that misses a
    /// transition paints a stale answer, and one that has to ask for `available`
    /// and `checking` separately can catch them mid-change and paint a caption
    /// that disagrees with its own highlight.
    #[test]
    fn every_state_change_is_reported_whole() {
        gtk_test(|| {
            let updates = updates();
            let seen: Rc<RefCell<Vec<State>>> = Rc::new(RefCell::new(Vec::new()));

            let sink = seen.clone();
            updates.set_state_callback(move |state| sink.borrow_mut().push(state));
            assert_eq!(
                seen.borrow().last().copied(),
                Some(State::default()),
                "registering paints once, so the renderer starts in step",
            );

            updates.check();
            assert_eq!(
                seen.borrow().last().copied(),
                Some(State {
                    available: false,
                    checking: true
                }),
            );

            // What a finished check that found something leaves behind.
            updates.0.in_flight.set(false);
            updates.0.available.set(true);
            updates.notify();
            assert_eq!(
                seen.borrow().last().copied(),
                Some(State {
                    available: true,
                    checking: false
                }),
            );

            // And what a *failed* one leaves: `available` is untouched, so the
            // found update has to survive rather than be cleared.
            updates.notify();
            assert!(seen.borrow().last().unwrap().available);
        });
    }
}
