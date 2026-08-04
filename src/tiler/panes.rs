//! Spawning, attaching and closing the panes a group holds.
//!
//! A pane is a live PTY with an agent in it, which is why `Tiler` owns them
//! directly rather than the model doing it: they are processes, not values, and
//! a second list of them anywhere else would be exactly the duplicated-order
//! problem `model` exists to remove.

use std::rc::Rc;

// Just the one trait, rather than the whole libadwaita prelude: `play` lives on
// `AnimationExt`, and glob-importing adw's prelude alongside gtk4's puts two
// `play` methods (the other is `MediaStream`'s) in scope on types that have
// both, which resolves to the wrong one rather than to an error.
use adw::prelude::AnimationExt;
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
    /// Echoes `text`, just committed by `source`, to every other pane - if this
    /// group is broadcasting and `source` is the pane the keyboard is in.
    ///
    /// The focus check is what makes this safe against itself. `feed_child` on
    /// the receiving panes causes their own `commit` to fire, which lands back
    /// here - but they are not the focused pane, so they broadcast nothing, and
    /// the fan-out stops one level deep. `broadcasting` guards the same thing a
    /// second way, in case a future VTE changes when `commit` fires.
    fn broadcast_from(&self, source: &Rc<Pane>, text: &str) {
        if !self.imp().broadcast.get() || self.imp().broadcasting.get() {
            return;
        }
        let panes = self.imp().panes.borrow();
        let focused = self.imp().focus.get();
        let is_focused = panes.get(focused).is_some_and(|p| Rc::ptr_eq(p, source));
        if !is_focused {
            return;
        }
        let bytes = text.as_bytes().to_vec();
        let others: Vec<_> = panes
            .iter()
            .filter(|p| !Rc::ptr_eq(p, source))
            .cloned()
            .collect();
        drop(panes);

        self.imp().broadcasting.set(true);
        for pane in others {
            // Editor panes are passed by: broadcast means "type into every
            // agent at once", and a file buffer is not an agent - keystrokes
            // meant for four claudes silently landing in an open file would be
            // corruption delivered by a feature.
            if let Some(terminal) = pane.terminal() {
                terminal.feed_child(&bytes);
            }
        }
        self.imp().broadcasting.set(false);
    }

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
    /// What each of this group's agents is doing, in pane order.
    ///
    /// The ordered form of `agent_tally`, for the rack: a tally can say "one of
    /// them wants you" and a list can say which, and where in the group it is.
    pub fn agent_states(&self) -> Vec<PaneState> {
        self.imp()
            .panes
            .borrow()
            .iter()
            // `agent_state`, not `state`: an editor pane has no agent for the
            // rack to draw a dot for, and a tally that counted it would say
            // "3 agents" of a project running two.
            .filter_map(|pane| pane.agent_state())
            .collect()
    }

    pub fn agent_tally(&self) -> Tally {
        let mut tally = Tally::default();
        for pane in self.imp().panes.borrow().iter() {
            match pane.agent_state() {
                Some(PaneState::Working { .. }) => tally.working += 1,
                Some(PaneState::Waiting) => tally.waiting += 1,
                Some(_) => tally.other += 1,
                None => {}
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
        if let Some(terminal) = pane.terminal() {
            terminal.connect_child_exited(move |_, status| on_finished(status == 0));
        }
    }

    /// Opens `path` in this group's editor pane - the same tile an agent
    /// gets, holding a file instead. `Err` is the editor's refusal, worded
    /// for a toast.
    ///
    /// One editor pane per group, reused: the first file opens a pane, every
    /// later click switches what that pane holds. Clicking through a tree is
    /// *browsing*, and a browse that spawned a tile per click would bury the
    /// agents under a grid of editors - the thing you actually want several
    /// of at once is agents, and the editor is where you look at one file at
    /// a time. Switching away from unsaved changes asks the same Save /
    /// Discard / Keep-editing question closing does, because it is the same
    /// event from the buffer's point of view: this text is about to stop
    /// being on screen.
    ///
    /// Unlike spawning an agent, both paths *take* the focus. Spawning is a
    /// background act (see `attach_pane`); opening a file is the opposite -
    /// the click meant "put this in front of me", and an editor that arrives
    /// without the keyboard is a click that half-worked.
    pub fn open_editor_pane(&self, path: &std::path::Path) -> Result<(), String> {
        let existing = self
            .imp()
            .panes
            .borrow()
            .iter()
            .find(|p| p.editor().is_some())
            .cloned();
        let Some(pane) = existing else {
            let pane = Rc::new(Pane::open_file(path)?);
            self.attach_pane_front(pane);
            self.set_focus(0);
            return Ok(());
        };
        let Some(editor) = pane.editor().cloned() else {
            // Unreachable: `existing` was found by having an editor.
            return Ok(());
        };

        // The refusal happens now, before any dialog. The confirm below is
        // asynchronous, so the file is read again inside it - if it becomes
        // unreadable in that gap, the editor's own error line reports it.
        crate::editor::Editor::readable(path)?;

        let this_weak = self.downgrade();
        let pane_weak = Rc::downgrade(&pane);
        let path = path.to_path_buf();
        editor.clone().confirm_close(self, move || {
            let (Some(this), Some(pane)) = (this_weak.upgrade(), pane_weak.upgrade()) else {
                return;
            };
            if editor.open(&path).is_ok() {
                pane.refresh_file_name();
            }
            // Recomputed rather than remembered: the confirm may have sat
            // open while panes came and went around it.
            let idx = this
                .imp()
                .panes
                .borrow()
                .iter()
                .position(|p| Rc::ptr_eq(p, &pane));
            if let Some(idx) = idx {
                this.set_focus(idx);
            }
            // The header's subtitle says which file this pane is "editing",
            // and that just changed.
            this.notify_title();
        });
        Ok(())
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
        // Every pane this function is handed was built by `Pane::new` or
        // `Pane::command`, so the terminal is always there - but a bug that
        // routed an editor pane here should skip the process wiring, not
        // panic a UI path.
        let Some(terminal) = pane.terminal().cloned() else {
            self.attach_pane(pane.clone());
            return pane;
        };

        let this_weak = self.downgrade();
        let pane_weak = Rc::downgrade(&pane);
        terminal.connect_child_exited(move |_, _status| {
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
        terminal.connect_bell(move |_| {
            if let Some(this) = this_weak.upgrade() {
                this.notify_attention();
            }
        });

        let this_weak = self.downgrade();
        let pane_weak = Rc::downgrade(&pane);
        terminal.connect_window_title_notify(move |_| {
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

    /// `attach_pane`, except the pane lands *first* - in the widget tree and
    /// in the pane list, which have to agree (the allocator zips one against
    /// rects computed from the other's order). First is where the editor
    /// lives: it docks at the workspace's left, and the cycling order should
    /// walk the tiles the way the eye does, editor then agents.
    fn attach_pane_front(&self, pane: Rc<Pane>) {
        // The pane everyone was focused on just moved one slot right; the
        // index follows it so the focus stays on the same pane rather than
        // the same number.
        self.imp().focus.set(self.imp().focus.get() + 1);
        self.attach_pane_at(pane, Some(0));
    }

    fn attach_pane(&self, pane: Rc<Pane>) {
        self.attach_pane_at(pane, None);
    }

    /// `position` is `Some(0)` for the front, `None` for the end - the only
    /// two places a pane ever arrives.
    fn attach_pane_at(&self, pane: Rc<Pane>, position: Option<usize>) {
        match position {
            Some(0) => pane.frame.insert_after(self, None::<&gtk4::Widget>),
            _ => pane.frame.set_parent(self),
        }
        pane.set_font_scale(self.imp().font_scale.get());
        fade_in(&pane.frame);

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

        // Broadcast: when this group is in broadcast mode and this is the
        // focused pane, whatever the terminal is about to send its own child
        // gets sent to every other pane's child too. Hooking `commit` rather
        // than the keyboard means VTE has already done the key-to-bytes work -
        // arrows, control codes, pasted text and all - and hands over exactly
        // the bytes it is sending, so the copies are byte-for-byte identical to
        // the original.
        if let Some(terminal) = pane.terminal() {
            let this_weak = self.downgrade();
            let pane_weak = Rc::downgrade(&pane);
            terminal.connect_commit(move |_, text, _size| {
                let (Some(this), Some(pane)) = (this_weak.upgrade(), pane_weak.upgrade()) else {
                    return;
                };
                this.broadcast_from(&pane, text);
            });
        }

        match position {
            Some(i) => self.imp().panes.borrow_mut().insert(i, pane),
            None => self.imp().panes.borrow_mut().push(pane),
        }
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
        settle_input_method(&pane.frame);
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

    /// Repaints every pane from the current appearance, and re-lays them.
    ///
    /// The re-lay is for the gap: it is read by the layout manager rather than
    /// stored anywhere, so a changed gap has no effect at all until something
    /// asks for a fresh allocation.
    pub fn refresh_appearance(&self) {
        for pane in self.imp().panes.borrow().iter() {
            pane.refresh_appearance();
        }
        self.queue_allocate();
    }

    pub fn close_focused(&self) {
        let focus = self.imp().focus.get();
        if let Some(pane) = self.imp().panes.borrow().get(focus).cloned() {
            self.close_pane(&pane);
        }
    }

    /// Close a specific pane regardless of focus (e.g. from its own X button).
    /// For a process pane, removal happens asynchronously via the
    /// `child-exited` signal; an editor pane has no process to exit, so its
    /// removal is driven from here - after the one question a dirty buffer
    /// gets, and after the same fade every other pane leaves by.
    fn close_pane(&self, pane: &Rc<Pane>) {
        if let Some(editor) = pane.editor() {
            let this_weak = self.downgrade();
            let pane_weak = Rc::downgrade(pane);
            editor.confirm_close(self, move || {
                let (Some(this), Some(pane)) = (this_weak.upgrade(), pane_weak.upgrade()) else {
                    return;
                };
                fade_out(&pane.frame);
                // The fade is a promise the removal keeps: a process pane
                // dims while its agent is hung up and leaves on
                // `child-exited`; this pane has no such signal, so the wait
                // is explicit. Weak references again - a project closed
                // during the fade takes the pane with it, and this timeout
                // must not be what keeps either alive.
                let this_weak = this.downgrade();
                let pane_weak = Rc::downgrade(&pane);
                gtk4::glib::timeout_add_local_once(
                    std::time::Duration::from_millis(u64::from(FADE_OUT_MS)),
                    move || {
                        if let (Some(this), Some(pane)) = (this_weak.upgrade(), pane_weak.upgrade())
                        {
                            this.remove_pane(&pane);
                        }
                    },
                );
            });
            return;
        }
        fade_out(&pane.frame);
        pane.hangup();
    }
}

/// Lets GTK's Wayland input-method plumbing finish coming up, before a pane's
/// terminal - which may be the widget it believes the keyboard is in - is
/// destroyed out from under it.
///
/// This works around a use-after-free in GTK itself
/// (`gtk/gtkimcontextwayland.c`, read at 4.22.4), which a tiler trips far more
/// easily than an ordinary app does. GTK keeps one `current` pointer per
/// display, naming the input-method context that holds the text-input focus.
/// `focus_in` writes it unconditionally; `focus_out` clears it only once the
/// `zwp_text_input_v3` object exists - and that object is bound lazily, one
/// Wayland round trip after the very first `focus_in` asks for the registry. A
/// terminal focused inside that round trip and destroyed before it lands
/// therefore leaves `current` pointing at freed memory, and the first
/// text-input event the compositor sends afterwards dereferences it: three GTK
/// criticals (`GTK_IS_WIDGET`, then `G_IS_OBJECT`, then `GDK_IS_WAYLAND_DISPLAY`
/// failing in turn) and then a segfault in `wl_proxy_get_version`, reached from
/// `wl_display_dispatch_queue_pending` with nothing of ours on the stack.
///
/// Two agents that exit the instant they are started is exactly that shape:
/// taking the first pane down hands the keyboard to the second (the `set_focus`
/// at the end of `remove_pane`), which is the first `focus_in` this process ever
/// performs, and the second pane is gone again before the round trip completes.
///
/// A `sync` is that round trip, forced early. Unparenting is GTK's one chance to
/// let go of the pane's context - afterwards the context has no widget and
/// `focus_out` gives up for a second, unrelated reason - and it takes that
/// chance only if the text-input object is already bound. Doing it before every
/// removal rather than once keeps the guarantee whichever order a focus and an
/// exit happen to arrive in, and it costs a sub-millisecond round trip on an
/// operation that already re-tiles a whole group. A compositor with no
/// text-input manager binds nothing and sends no such events either, so it was
/// never at risk; the sync is simply wasted there, and on X11.
///
/// `pub(super)` because `remove_pane` is not the only way a pane leaves the
/// widget tree: `imp::Tiler::dispose` unparents all of them at once when a
/// project is closed or the window goes down, and that path needs the same round
/// trip. One helper called from both, rather than the sync written out twice and
/// only one copy maintained.
pub(super) fn settle_input_method(frame: &gtk4::Frame) {
    if let Some(display) = display_to_settle(frame) {
        display.sync();
    }
}

/// The display `settle_input_method` should round-trip, or `None` when there
/// isn't one to round-trip to.
///
/// Deliberately not `frame.display()`, which is the natural spelling and the one
/// that cannot be used here. `gtk_widget_get_display` answers with the root's
/// display, or with the default display when the widget has no root, or with
/// NULL when there is no default display either - and the Rust binding feeds
/// that straight into `from_glib_none`, whose null check is a `debug_assert`. So
/// the display-less case panics in a debug build and wraps a `Display` around a
/// null pointer in a release one. Trading a segfault for a segfault is not a fix,
/// and this function exists only to prevent one.
///
/// `Tiler::dispose` is what makes that reachable. `remove_pane` only ever runs
/// with a live window on screen, so it could ask the frame directly and get away
/// with it; dispose is also reached from widget teardown at process exit, where
/// the display can already be closed or gone. So this walks the same path GTK
/// does - root first, default second - and returns the NULL as a `None` instead
/// of asserting on it.
///
/// A closed display is refused for a different reason: `sync` on one is a
/// round trip on a `wl_display` GDK is in the middle of tearing down, and there
/// is nothing left to protect anyway. The crash this guards against is a
/// text-input event *arriving later*, and a closed display delivers no more
/// events.
fn display_to_settle(frame: &gtk4::Frame) -> Option<gdk::Display> {
    // Spelled out because `gtk4::Root` extends `Widget`, so both preludes in
    // scope here offer a `display` on it and neither wins.
    let display = frame
        .root()
        .map(|root| gtk4::prelude::RootExt::display(&root))
        .or_else(gdk::Display::default)?;
    (!display.is_closed()).then_some(display)
}

/// How long a pane takes to arrive, and to leave. Short enough not to be a
/// wait, since closing a pane is a thing you do repeatedly, and long enough
/// that the eye registers the tiles re-laying as a consequence of it rather
/// than as an unrelated jump.
const FADE_IN_MS: u32 = 180;
const FADE_OUT_MS: u32 = 140;

/// A new pane arrives at zero and comes up.
///
/// Only the opacity moves. Animating the *geometry* is the obvious next thought
/// and is a genuinely bad idea here: a pane's size is its terminal's character
/// grid, so interpolating it would resize a pty on every frame of the animation
/// and have every agent's output reflow continuously for the length of it.
fn fade_in(frame: &gtk4::Frame) {
    frame.set_opacity(0.0);
    animate_opacity(frame, 0.0, 1.0, FADE_IN_MS);
}

/// A closing pane dims while its agent is being hung up.
///
/// It does not wait for the fade before hanging up, and the fade does not
/// remove anything: the pane goes when its `child-exited` arrives, which is the
/// same as it ever was. This is only the acknowledgement that the click landed,
/// which the instant version left to the tiles jumping some milliseconds later.
fn fade_out(frame: &gtk4::Frame) {
    animate_opacity(frame, frame.opacity(), 0.12, FADE_OUT_MS);
}

/// Runs `widget.opacity` from `from` to `to`.
///
/// Never fades fully to zero, and that is a safety floor rather than a taste
/// call: the fade is started by "close this pane" but the *removal* is driven by
/// the agent's process actually exiting, and an agent that ignores its hangup
/// leaves the pane on screen. At zero that pane would be invisible, still
/// holding its share of the tiling, and impossible to click on. At 0.12 it is
/// plainly on its way out and still there to be dealt with.
///
/// The animation needs no owner. `AdwAnimation` holds a reference to itself for
/// as long as it is playing, and when the desktop has animations turned off it
/// jumps straight to the end value and finishes - so the widget lands on `to`
/// either way, which is the property that matters when `to` is what makes a new
/// pane visible at all.
fn animate_opacity(frame: &gtk4::Frame, from: f64, to: f64, ms: u32) {
    let target = adw::PropertyAnimationTarget::new(frame, "opacity");
    adw::TimedAnimation::builder()
        .widget(frame)
        .value_from(from)
        .value_to(to)
        .duration(ms)
        .easing(adw::Easing::EaseOutCubic)
        .target(&target)
        .build()
        .play();
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::gtk_test;

    /// `remove_pane` forces a display round trip on its way to unparenting a
    /// pane - see `settle_input_method` for what that is buying - and it asks
    /// the departing frame itself for the display to sync. So the round trip has
    /// to be reachable from a frame in exactly that position, and this pins the
    /// two properties the workaround rests on: a widget answers with a display
    /// whether or not it is rooted (GTK falls back to the default one), and
    /// syncing is safe to repeat.
    ///
    /// Both matter because the obvious spelling is `frame.root().unwrap()`, and
    /// a frame on its way out of the tree is precisely the case with no root, so
    /// that version would panic on every pane the app ever closes and on every
    /// pane that dies before its group was ever shown. Which, since panes are
    /// removed by their agent exiting, is the same startup race this whole
    /// workaround exists for.
    ///
    /// It cannot go the whole way and reproduce the crash: that needs a
    /// compositor holding the keyboard focus and a real `zwp_text_input_v3`
    /// round trip, neither of which a unit test has.
    #[test]
    fn a_pane_on_its_way_out_can_still_reach_a_display_to_sync() {
        gtk_test(|| {
            let tiler = Tiler::new("/tmp".to_string());
            let frame = gtk4::Frame::new(None);

            // Never parented - a half-built pane, or one already let go of.
            assert_eq!(
                Some(frame.display()),
                gdk::Display::default(),
                "an unrooted frame still names the display to round-trip",
            );
            settle_input_method(&frame);

            // Parented to its group, which is where `remove_pane` calls it from,
            // one line before `unparent`.
            frame.set_parent(&tiler);
            settle_input_method(&frame);

            // And after, which is the state `Tiler::dispose` leaves panes in.
            frame.unparent();
            settle_input_method(&frame);
        });
    }

    /// `display_to_settle` replaced `frame.display()` so that a dispose running
    /// with no display left does nothing instead of tripping a `debug_assert`
    /// inside the binding. That swap is only correct if it still finds the same
    /// display in every case where there *is* one - a guard that quietly answered
    /// `None` for a live window would turn the segfault workaround off and leave
    /// no trace of having done it.
    ///
    /// The no-display case itself is deliberately not tested: there is no way to
    /// take the default display away from one test without taking it away from
    /// the whole binary, and GTK's own null return is what the guard is written
    /// against, not something a test can manufacture.
    #[test]
    fn the_display_guard_agrees_with_gtk_wherever_there_is_a_display() {
        gtk_test(|| {
            let tiler = Tiler::new("/tmp".to_string());
            let frame = gtk4::Frame::new(None);

            // The same three positions the test above walks, since those are the
            // ones `remove_pane` and `dispose` between them actually call from.
            let agrees = |position: &str| {
                assert_eq!(
                    display_to_settle(&frame),
                    Some(frame.display()),
                    "the guard dropped a display GTK was willing to name \
                     ({position}); the round trip would silently stop happening",
                );
            };

            agrees("never parented");
            frame.set_parent(&tiler);
            agrees("parented to its group");
            frame.unparent();
            agrees("unparented again");

            // And the `is_closed` filter is not what produced those: an open
            // display has to survive it, or the guard would be refusing every
            // display there is and the assertions above would be vacuous.
            assert!(
                !frame.display().is_closed(),
                "the display a test runs against should be open",
            );
        });
    }
}
