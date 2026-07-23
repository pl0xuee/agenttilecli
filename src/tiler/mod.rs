use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gtk4::gio::prelude::*;
use gtk4::prelude::*;
use gtk4::subclass::prelude::*;
use gtk4::{glib, Widget};
use vte4::prelude::*;

mod manager;
mod panes;
mod resize;

pub(crate) use manager::{GridDragState, Handle, TilerLayout};

use crate::layout::Mode;
use crate::pane::Pane;


// ---------------------------------------------------------------------
// Tiler: the container widget. Owns pane order/focus; the LayoutManager
// above only needs mode/ratio/master_count/focus to compute geometry from
// the widget tree's actual child order, which Tiler keeps in sync with
// its own `panes` Vec.
// ---------------------------------------------------------------------

/// Everything about how a group is arranged that isn't the mode - reported out
/// to whoever is keeping the model in step, via `Tiler::set_layout_callback`.
///
/// These used to be readable only from inside the layout manager, which meant
/// the app could tile by them but never report or save them. They are still
/// *owned* here rather than read back out of the model on every allocation:
/// `TilerLayout::allocate` runs inside GTK's layout pass, and reaching into a
/// `RefCell` the same pass may already have borrowed is a re-entrancy bug
/// waiting to happen. So the tiler stays the source and pushes changes out,
/// exactly as `mode` already does.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct LayoutState {
    pub master_ratio: f64,
    pub master_count: usize,
    /// Index of the focused pane within this group's pane order.
    pub focus: usize,
}

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct Tiler {
        pub panes: RefCell<Vec<Rc<Pane>>>,
        pub focus: Cell<usize>,
        pub cwd: RefCell<String>,
        pub title_cb: RefCell<Option<Box<dyn Fn(&str)>>>,
        /// Invoked when a pane in this group wants the user - see
        /// `Tiler::set_attention_callback`.
        pub attention_cb: RefCell<Option<Box<dyn Fn()>>>,
        /// Invoked whenever the layout mode changes, however it changed - see
        /// `Tiler::set_mode_callback`.
        pub mode_cb: RefCell<Option<Box<dyn Fn(Mode)>>>,
        /// Invoked whenever the master ratio, master count or focus index
        /// changes - see `Tiler::set_layout_callback`.
        pub layout_cb: RefCell<Option<Box<dyn Fn(LayoutState)>>>,
        /// Invoked with the new pane count whenever a pane is attached or
        /// removed - see `Tiler::set_pane_count_callback`.
        pub count_cb: RefCell<Option<Box<dyn Fn(usize)>>>,
        pub resizing: Cell<bool>,
        pub drag_start_ratio: Cell<f64>,
        pub drag_start_width: Cell<i32>,
        pub(crate) grid_drag: RefCell<Option<GridDragState>>,
        /// VTE `font-scale` applied to every pane, including ones spawned
        /// after a resize. Set to 1.0 in `Tiler::new`, since `Cell<f64>`'s
        /// `Default` is 0.0 (invisible text), not the unscaled size.
        pub font_scale: Cell<f64>,
        /// Whether typing into the focused pane is echoed to every other pane in
        /// this group. Off by default and never persisted: it is a mode you turn
        /// on for one deliberate thing - the same command to four agents - and a
        /// window that remembered it across a restart would send your next
        /// keystroke to four terminals you had forgotten were listening.
        pub broadcast: Cell<bool>,
        /// True while a broadcast is fanning out, so the `commit` it causes on
        /// the receiving panes can't fan out again. The focus gate already
        /// stops that, since only the focused pane broadcasts - this is the belt
        /// to that braces.
        pub broadcasting: Cell<bool>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for Tiler {
        const NAME: &'static str = "AgentTileCliTiler";
        type Type = super::Tiler;
        type ParentType = Widget;
    }

    impl ObjectImpl for Tiler {
        fn constructed(&self) {
            self.parent_constructed();
            self.obj().set_layout_manager(Some(super::TilerLayout::new()));
        }

        fn dispose(&self) {
            for pane in self.panes.borrow().iter() {
                pane.frame.unparent();
            }
        }
    }

    impl WidgetImpl for Tiler {}
}

glib::wrapper! {
    pub struct Tiler(ObjectSubclass<imp::Tiler>)
        @extends Widget,
        @implements gtk4::Accessible, gtk4::Buildable, gtk4::ConstraintTarget;
}

impl Tiler {
    pub fn new(cwd: String) -> Self {
        let this: Self = glib::Object::new();
        *this.imp().cwd.borrow_mut() = cwd;
        this.imp().font_scale.set(1.0);
        this.setup_resize();
        this
    }

    fn layout_mgr(&self) -> TilerLayout {
        self.layout_manager()
            .expect("Tiler always has a layout manager")
            .downcast::<TilerLayout>()
            .expect("Tiler's layout manager is always a TilerLayout")
    }





    /// Change focus, refresh the focus-border CSS, re-tile (needed in
    /// Monocle mode, harmless elsewhere), and grab keyboard focus onto the
    /// newly-focused pane's terminal.
    fn set_focus(&self, idx: usize) {
        self.imp().focus.set(idx);
        self.update_focus_style();
        self.relayout();
        self.grab_focus_on_current();
        self.notify_layout();
    }

    /// Push this widget's focus index into the layout manager and request a
    /// re-tile. Geometry only actually depends on focus in Monocle mode, but
    /// this is cheap enough to call unconditionally after any pane op.
    fn relayout(&self) {
        let focus = self.imp().focus.get();
        self.layout_mgr().imp().focus.set(focus);
        self.queue_allocate();
    }

    fn update_focus_style(&self) {
        let focus = self.imp().focus.get();
        for (i, pane) in self.imp().panes.borrow().iter().enumerate() {
            let is_focused = i == focus;
            if is_focused {
                pane.frame.add_css_class("focused");
            } else {
                pane.frame.remove_css_class("focused");
            }
            // The frame's half of it is the border and the glow; this is the
            // fill, which only reaches the screen if VTE paints it (see
            // `Pane::set_focused`).
            pane.set_focused(is_focused);
        }
    }

    /// Reparent every pane in current Vec order so the widget child order
    /// (which the layout manager reads directly) always matches it. Cheap
    /// for the small pane counts this app deals with.
    fn reflow_children(&self) {
        let panes = self.imp().panes.borrow();
        for pane in panes.iter() {
            pane.frame.unparent();
        }
        for pane in panes.iter() {
            pane.frame.set_parent(self);
        }
    }













    pub fn focus_next(&self) {
        let len = self.imp().panes.borrow().len();
        if len == 0 {
            return;
        }
        self.set_focus((self.imp().focus.get() + 1) % len);
    }

    pub fn focus_prev(&self) {
        let len = self.imp().panes.borrow().len();
        if len == 0 {
            return;
        }
        self.set_focus((self.imp().focus.get() + len - 1) % len);
    }

    /// dwm-style "zoom": swap the focused pane into the master slot (index 0).
    pub fn promote_focused_to_master(&self) {
        let focus = self.imp().focus.get();
        if focus == 0 {
            return;
        }
        self.imp().panes.borrow_mut().swap(0, focus);
        self.reflow_children();
        self.set_focus(0);
    }

    pub fn inc_master_ratio(&self) {
        let lm = self.layout_mgr();
        let r = (lm.imp().master_ratio.get() + 0.05).min(0.9);
        lm.imp().master_ratio.set(r);
        self.queue_allocate();
        self.notify_layout();
    }

    pub fn dec_master_ratio(&self) {
        let lm = self.layout_mgr();
        let r = (lm.imp().master_ratio.get() - 0.05).max(0.1);
        lm.imp().master_ratio.set(r);
        self.queue_allocate();
        self.notify_layout();
    }

    pub fn inc_master_count(&self) {
        let len = self.imp().panes.borrow().len().max(1);
        let lm = self.layout_mgr();
        let c = (lm.imp().master_count.get() + 1).min(len);
        lm.imp().master_count.set(c);
        self.queue_allocate();
        self.notify_layout();
    }

    pub fn dec_master_count(&self) {
        let lm = self.layout_mgr();
        let c = (lm.imp().master_count.get().max(2)) - 1;
        lm.imp().master_count.set(c);
        self.queue_allocate();
        self.notify_layout();
    }

    /// Apply `scale` to every current pane's terminal (new panes pick up
    /// whatever `font_scale` holds at attach time, in `attach_pane`). Text
    /// size is a global setting (see `App::inc_font_scale`), which calls
    /// this on every group's `Tiler` in lockstep - not just the active
    /// one - so switching groups never shows a different zoom level.
    pub(crate) fn set_font_scale(&self, scale: f64) {
        self.imp().font_scale.set(scale);
        for pane in self.imp().panes.borrow().iter() {
            pane.terminal.set_font_scale(scale);
        }
    }

    /// The terminal of the pane the keyboard is in.
    pub fn focused_terminal(&self) -> Option<vte4::Terminal> {
        let focus = self.imp().focus.get();
        self.imp()
            .panes
            .borrow()
            .get(focus)
            .map(|pane| pane.terminal.clone())
    }

    /// Whether keystrokes are being echoed to every pane in this group.
    pub fn broadcast(&self) -> bool {
        self.imp().broadcast.get()
    }

    /// Turns input broadcast on or off for this group.
    pub fn set_broadcast(&self, on: bool) {
        self.imp().broadcast.set(on);
    }

    /// How this group's panes are currently arranged.
    ///
    /// Public because the mode is no longer only a tiling input: the header bar
    /// reports it, so it has to be readable from outside the layout manager it
    /// used to be sealed inside. Pressing the cycle key and learning nothing was
    /// the whole problem with keeping it private.
    pub fn mode(&self) -> Mode {
        self.layout_mgr().imp().mode.get()
    }

    /// The one place the mode is written, so `mode_cb` cannot be bypassed - a
    /// header bar showing a mode the tiler isn't in is worse than one showing
    /// nothing.
    pub fn set_mode(&self, mode: Mode) {
        let lm = self.layout_mgr();
        if lm.imp().mode.get() == mode {
            return;
        }
        lm.imp().mode.set(mode);
        self.queue_allocate();
        if let Some(cb) = self.imp().mode_cb.borrow().as_ref() {
            cb(mode);
        }
    }

    /// Registers a callback invoked whenever this group's layout mode changes,
    /// by any route - the keybinding, the header bar, or the monocle toggle.
    pub fn set_mode_callback(&self, f: impl Fn(Mode) + 'static) {
        *self.imp().mode_cb.borrow_mut() = Some(Box::new(f));
    }

    /// How this group is arranged, beyond the mode.
    pub fn layout_state(&self) -> LayoutState {
        let lm = self.layout_mgr();
        LayoutState {
            master_ratio: lm.imp().master_ratio.get(),
            master_count: lm.imp().master_count.get(),
            focus: self.imp().focus.get(),
        }
    }

    /// Puts a group back the way a previous run left it.
    ///
    /// Writes the cells directly rather than going through `set_mode` and the
    /// increment methods, because each of those reports outward - and a restore
    /// is not news. Replaying a saved layout as a series of user actions would
    /// mark the session dirty and schedule a save of the thing just loaded.
    pub fn restore_layout(&self, mode: Mode, state: LayoutState) {
        let lm = self.layout_mgr();
        lm.imp().mode.set(mode);
        lm.imp().master_ratio.set(state.master_ratio.clamp(0.1, 0.9));
        lm.imp().master_count.set(state.master_count.max(1));
        self.queue_allocate();
    }

    /// Registers a callback invoked whenever the master ratio, master count or
    /// focus changes - by keybinding or by dragging the master seam.
    pub fn set_layout_callback(&self, f: impl Fn(LayoutState) + 'static) {
        *self.imp().layout_cb.borrow_mut() = Some(Box::new(f));
    }

    /// Reports the current arrangement outward. Called from every site that
    /// writes one of those three, so the model cannot silently fall behind.
    fn notify_layout(&self) {
        if let Some(cb) = self.imp().layout_cb.borrow().as_ref() {
            cb(self.layout_state());
        }
    }

    pub fn cycle_mode(&self) {
        self.set_mode(self.mode().next());
    }

    /// Jump straight to Monocle (focused pane fullscreen), or back to
    /// MasterStack if already in Monocle.
    pub fn toggle_monocle(&self) {
        self.set_mode(if self.mode() == Mode::Monocle {
            Mode::MasterStack
        } else {
            Mode::Monocle
        });
    }

    fn grab_focus_on_current(&self) {
        let focus = self.imp().focus.get();
        if let Some(pane) = self.imp().panes.borrow().get(focus) {
            pane.terminal.grab_focus();
        }
        self.notify_title();
    }

    /// Called when this group becomes the visible one in the sidebar's
    /// stack: re-grabs terminal focus on its current pane and re-syncs the
    /// window title, since neither happens on its own while a `Tiler` sits
    /// hidden in a background group.
    pub fn on_shown(&self) {
        self.grab_focus_on_current();
    }

    /// Register a callback invoked with the focused pane's foreground-process
    /// title (e.g. so `main.rs` can mirror it onto the window titlebar).
    pub fn set_title_callback(&self, f: impl Fn(&str) + 'static) {
        *self.imp().title_cb.borrow_mut() = Some(Box::new(f));
    }

    /// Register a callback invoked whenever any pane in this group wants the
    /// user's attention - the agent rang the bell (it finished, or it's asking
    /// something) or its process exited. `Groups` uses this to flash the
    /// group's sidebar row; it fires regardless of which pane, or which group,
    /// the user is currently looking at, and it's the listener's job to decide
    /// whether that's worth saying anything about.
    pub fn set_attention_callback(&self, f: impl Fn() + 'static) {
        *self.imp().attention_cb.borrow_mut() = Some(Box::new(f));
    }

    fn notify_attention(&self) {
        if let Some(cb) = self.imp().attention_cb.borrow().as_ref() {
            cb();
        }
    }

    fn notify_title(&self) {
        let focus = self.imp().focus.get();
        let title = self
            .imp()
            .panes
            .borrow()
            .get(focus)
            .and_then(|p| p.terminal.window_title())
            .map(|t| t.to_string())
            .unwrap_or_default();
        if let Some(cb) = self.imp().title_cb.borrow().as_ref() {
            cb(&title);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::gtk_test;
    use std::cell::RefCell;
    use std::rc::Rc;

    /// The layout callback is the only route by which the master ratio and the
    /// focus index reach the model - `TilerLayout`'s cells are private and its
    /// allocation pass is the wrong place to read them from. So a write that
    /// forgets to report is a group that tiles correctly and saves wrong, which
    /// is invisible until a session is restored.
    #[test]
    fn every_master_ratio_change_is_reported_outward() {
        gtk_test(|| {
            let tiler = Tiler::new("/tmp".to_string());
            let seen: Rc<RefCell<Vec<LayoutState>>> = Rc::new(RefCell::new(Vec::new()));

            let sink = seen.clone();
            tiler.set_layout_callback(move |state| sink.borrow_mut().push(state));

            tiler.inc_master_ratio();
            tiler.inc_master_ratio();
            tiler.dec_master_ratio();

            let seen = seen.borrow();
            assert_eq!(seen.len(), 3, "one report per write, no more and no fewer");
            // 0.55 is the starting ratio; the step is 0.05.
            let ratios: Vec<f64> = seen.iter().map(|s| s.master_ratio).collect();
            for (got, want) in ratios.iter().zip([0.60, 0.65, 0.60]) {
                assert!(
                    (got - want).abs() < 1e-9,
                    "reported ratios {ratios:?}, expected 0.60 / 0.65 / 0.60",
                );
            }
            assert_eq!(
                seen.last().map(|s| s.master_ratio),
                Some(tiler.layout_state().master_ratio),
                "the last thing reported is what the tiler actually holds",
            );
        });
    }

    /// A grid the user has arranged by dragging its seams keeps those
    /// proportions for as long as its shape holds - which is every resize that
    /// doesn't want a different (cols, rows), and every one that does not touch
    /// the pane count.
    #[test]
    fn a_dragged_grid_survives_a_resize_that_keeps_its_shape() {
        gtk_test(|| {
            let lm = TilerLayout::new();
            let imp = lm.imp();

            imp.ensure_grid_ratios(4, 1600, 900);
            let shape = imp.grid_shape_dims.get();
            imp.row_ratios.borrow_mut()[0] = 1.6;
            let dragged = imp.row_ratios.borrow().clone();

            // A resize the shape is happy with changes nothing underneath it.
            imp.ensure_grid_ratios(4, 1500, 880);
            assert_eq!(imp.grid_shape_dims.get(), shape);
            assert_eq!(*imp.row_ratios.borrow(), dragged, "the drag survives");

            // Opening a pane genuinely invalidates the arrangement.
            imp.ensure_grid_ratios(5, 1500, 880);
            assert!(
                imp.row_ratios.borrow().iter().all(|r| *r == 1.0),
                "a new pane count starts from equal cells again",
            );
        });
    }

    /// The shape follows the window, and it follows it on a plain resize - no
    /// pane opened or closed.
    ///
    /// This is what stops three panes in a wide window standing as three tall
    /// slivers after it has been dragged narrow. The stability bias that used to
    /// apply here was strong enough to hold a landscape shape through the whole
    /// journey to portrait.
    #[test]
    fn the_grid_reorients_on_a_plain_resize() {
        gtk_test(|| {
            let lm = TilerLayout::new();
            let imp = lm.imp();

            imp.ensure_grid_ratios(3, 1600, 500);
            let wide = imp.grid_shape_dims.get();

            imp.ensure_grid_ratios(3, 500, 1600);
            let tall = imp.grid_shape_dims.get();

            assert_ne!(wide, tall, "a window turned on its side wants a new shape");
            assert!(wide.0 > wide.1, "wide window: more columns than rows");
            assert!(tall.1 > tall.0, "tall window: more rows than columns");
        });
    }

    /// The ratio is clamped inside the tiler rather than by whoever stores it,
    /// so the clamp has to be visible in what gets reported - a model that
    /// records 1.4 restores a master column wider than the window.
    #[test]
    fn a_reported_ratio_is_the_clamped_one() {
        gtk_test(|| {
            let tiler = Tiler::new("/tmp".to_string());
            let seen = Rc::new(RefCell::new(Vec::new()));

            let sink = seen.clone();
            tiler.set_layout_callback(move |state: LayoutState| {
                sink.borrow_mut().push(state.master_ratio)
            });

            for _ in 0..20 {
                tiler.inc_master_ratio();
            }
            assert_eq!(seen.borrow().last().copied(), Some(0.9));

            for _ in 0..40 {
                tiler.dec_master_ratio();
            }
            assert_eq!(seen.borrow().last().copied(), Some(0.1));
        });
    }
}
