//! Spawning, attaching and closing the panes a group holds.
//!
//! A pane is a live PTY with an agent in it, which is why `Tiler` owns them
//! directly rather than the model doing it: they are processes, not values, and
//! a second list of them anywhere else would be exactly the duplicated-order
//! problem `model` exists to remove.

use std::rc::Rc;

use gtk4::prelude::*;
use gtk4::subclass::prelude::*;
use gtk4::{gdk, GestureClick, PropagationPhase};
use vte4::prelude::*;

use super::Tiler;
use crate::hooks;
use crate::ipc;
use crate::model::PaneState;
use crate::pane::Pane;

impl Tiler {
    /// Applies an agent's report to whichever of this group's panes sent it.
    ///
    /// Returns whether it landed *and* changed something - the caller uses that
    /// to decide whether the sidebar needs repainting, and a turn produces far
    /// more events than it does state changes.
    ///
    /// Every group is asked in turn until one claims the message, because a pane
    /// id is unique across the window rather than within a group, and a message
    /// naming a pane that has since been closed is simply claimed by nobody.
    pub fn apply_agent_event(&self, message: &ipc::Message) -> bool {
        let panes = self.imp().panes.borrow();
        let Some(pane) = panes.iter().find(|p| p.id == message.pane) else {
            return false;
        };
        let next = hooks::advance(&pane.state(), message.event, message.tool.as_deref());
        let changed = pane.set_state(next);
        drop(panes);
        if changed {
            // An agent that wants you is worth saying so about, exactly as the
            // bell already does - this is the same news arriving by a route that
            // knows which pane it came from.
            if message.event == crate::hooks::Event::Notification {
                self.notify_attention();
            }
        }
        changed
    }

    /// How many of this group's panes are in each state worth counting.
    pub fn agent_tally(&self) -> Tally {
        let mut tally = Tally::default();
        for pane in self.imp().panes.borrow().iter() {
            match pane.state() {
                PaneState::Working { .. } => tally.working += 1,
                PaneState::Waiting => tally.waiting += 1,
                _ => tally.other += 1,
            }
        }
        tally
    }

    /// How many panes this group is currently running.
    pub fn pane_count(&self) -> usize {
        self.imp().panes.borrow().len()
    }

    /// Spawns a pane in this group's project directory (the one it was
    /// created with) - no dialog. Opening a *different* project happens by
    /// creating a whole new project (see `crate::app::App::new_project`)
    /// rather than mixing an unrelated project's panes into this grid.
    pub fn spawn_pane_here(&self) {
        let cwd = self.imp().cwd.borrow().clone();
        self.spawn_pane_in(&cwd);
    }

    fn spawn_pane_in(&self, cwd: &str) {
        self.attach_process_pane(Pane::new(cwd));
    }

    /// Spawns a pane running `command` rather than `claude` - the update
    /// button's pull-and-rebuild script (see `crate::update::command`), which
    /// gets a pane of its own so the user can watch it work.
    ///
    /// `on_finished` is handed `true` when the command exited cleanly. The
    /// update uses that to decide whether to relaunch the app: only a script
    /// that actually got the new binary onto disk is worth restarting into.
    pub fn spawn_command_pane(
        &self,
        cwd: &str,
        command: &str,
        on_finished: impl Fn(bool) + 'static,
    ) {
        let pane = self.attach_process_pane(Pane::command(cwd, command));
        // A second handler on the same signal - `attach_process_pane` already
        // connected one to take the pane down. Both run; neither cares about
        // the other's order.
        //
        // Zero is success under either convention VTE might report the status
        // in (a raw `waitpid` status or a bare exit code), since `exit 0` is 0
        // in both, and every failure - a non-zero exit, a signal - is non-zero
        // in both.
        pane.terminal
            .connect_child_exited(move |_, status| on_finished(status == 0));
    }

    /// Wires up the signals every pane with a child process needs (close on
    /// exit, re-title on the child's title change, flag for attention when the
    /// agent rings the bell) and attaches it. The help pane skips this - it has
    /// no process behind it to exit, re-title, or ring anything.
    ///
    /// Hands the attached pane back so a caller with a further interest in it
    /// (`spawn_command_pane`, which wants to know how its child exited) can
    /// hang its own signal handlers on the same terminal.
    fn attach_process_pane(&self, pane: Pane) -> Rc<Pane> {
        let pane = Rc::new(pane);

        let this_weak = self.downgrade();
        let pane_weak = Rc::downgrade(&pane);
        pane.terminal.connect_child_exited(move |_, _status| {
            if let (Some(this), Some(pane)) = (this_weak.upgrade(), pane_weak.upgrade()) {
                this.remove_pane(&pane);
                // An agent quitting is news too, if it happened somewhere the
                // user wasn't looking.
                this.notify_attention();
            }
        });

        // The bell is what "the agent wants you" actually looks like on the
        // wire: Claude rings it when it finishes a turn and when it stops to
        // ask something. Nothing else in a stream of terminal output
        // distinguishes "done" from "still typing", so this one byte is the
        // whole signal - `Groups` turns it into a flashing sidebar row.
        let this_weak = self.downgrade();
        pane.terminal.connect_bell(move |_| {
            if let Some(this) = this_weak.upgrade() {
                this.notify_attention();
            }
        });

        let this_weak = self.downgrade();
        let pane_weak = Rc::downgrade(&pane);
        pane.terminal.connect_window_title_notify(move |_| {
            if let (Some(this), Some(pane)) = (this_weak.upgrade(), pane_weak.upgrade()) {
                let focus = this.imp().focus.get();
                let is_focused = this
                    .imp()
                    .panes
                    .borrow()
                    .get(focus)
                    .is_some_and(|p| Rc::ptr_eq(p, &pane));
                if is_focused {
                    this.notify_title();
                }
            }
        });

        self.attach_pane(pane.clone());
        pane
    }

    fn attach_pane(&self, pane: Rc<Pane>) {
        pane.frame.set_parent(self);
        pane.terminal.set_font_scale(self.imp().font_scale.get());

        // Click-to-focus: fires in the Capture phase so it always sees the
        // press, but never claims it, so the terminal underneath still gets
        // normal click/selection behavior afterward.
        let click = GestureClick::new();
        click.set_propagation_phase(PropagationPhase::Capture);
        click.set_button(gdk::BUTTON_PRIMARY);
        let this_weak = self.downgrade();
        let pane_weak = Rc::downgrade(&pane);
        click.connect_pressed(move |_, _n_press, _x, _y| {
            if let (Some(this), Some(pane)) = (this_weak.upgrade(), pane_weak.upgrade()) {
                let idx = this
                    .imp()
                    .panes
                    .borrow()
                    .iter()
                    .position(|p| Rc::ptr_eq(p, &pane));
                if let Some(idx) = idx {
                    this.set_focus(idx);
                }
            }
        });
        pane.frame.add_controller(click);

        let this_weak = self.downgrade();
        let pane_weak = Rc::downgrade(&pane);
        pane.close_button.connect_clicked(move |_| {
            if let (Some(this), Some(pane)) = (this_weak.upgrade(), pane_weak.upgrade()) {
                this.close_pane(&pane);
            }
        });

        self.imp().panes.borrow_mut().push(pane);
        let pane_count = self.imp().panes.borrow().len();
        if pane_count == 1 {
            // The first pane in an empty group has to take focus: nothing else
            // is holding it, and a group whose only terminal doesn't accept
            // typing is just broken.
            self.set_focus(0);
        } else {
            // After that, spawning is a background act. You start another agent
            // *while* working in one, and having the keyboard yank itself into
            // a fresh pane mid-sentence sends the rest of that sentence
            // somewhere you weren't looking. The new pane is on screen and one
            // click (or Super+Alt+j) away, which is enough of an invitation.
            //
            // Still a re-tile and a restyle, though: the grid has one more cell
            // in it, and the new pane has to be painted as the unfocused one it
            // is rather than inherit the focused frame.
            self.update_focus_style();
            self.relayout();
        }
        self.notify_pane_count();
    }

    /// Registers a callback invoked with the pane count whenever it changes.
    /// Drives the empty state: a project with nothing running shows what to do
    /// about that rather than a blank rectangle.
    pub fn set_pane_count_callback(&self, f: impl Fn(usize) + 'static) {
        *self.imp().count_cb.borrow_mut() = Some(Box::new(f));
        self.notify_pane_count();
    }

    fn notify_pane_count(&self) {
        let count = self.imp().panes.borrow().len();
        if let Some(cb) = self.imp().count_cb.borrow().as_ref() {
            cb(count);
        }
    }

    fn remove_pane(&self, pane: &Rc<Pane>) {
        let removed = {
            let mut panes = self.imp().panes.borrow_mut();
            if let Some(pos) = panes.iter().position(|p| Rc::ptr_eq(p, pane)) {
                panes.remove(pos);
                true
            } else {
                false
            }
        };
        if !removed {
            return;
        }
        pane.frame.unparent();

        let len = self.imp().panes.borrow().len();
        let focus = self.imp().focus.get();
        self.set_focus(if len == 0 { 0 } else { focus.min(len - 1) });
        self.notify_pane_count();
    }

    /// Hangs up every pane in this project, without waiting for their
    /// `child-exited` signals - used when the whole project is being torn down
    /// (see `App::remove_project`), so the caller can drop this `Tiler` right
    /// away instead of waiting on each pane individually.
    pub fn close_all_panes(&self) {
        for pane in self.imp().panes.borrow().iter() {
            pane.hangup();
        }
    }

    pub fn close_focused(&self) {
        let focus = self.imp().focus.get();
        if let Some(pane) = self.imp().panes.borrow().get(focus).cloned() {
            self.close_pane(&pane);
        }
    }

    /// Close a specific pane regardless of focus (e.g. from its own X button).
    /// Removal happens asynchronously via the `child-exited` signal.
    fn close_pane(&self, pane: &Rc<Pane>) {
        pane.hangup();
    }
}

/// What a group's agents are up to, counted.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct Tally {
    pub working: usize,
    pub waiting: usize,
    /// Starting, idle, or gone - everything that isn't a claim on your
    /// attention or a sign of progress.
    pub other: usize,
}

impl Tally {
    pub fn total(self) -> usize {
        self.working + self.waiting + self.other
    }
}
