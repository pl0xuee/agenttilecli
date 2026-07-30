use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gtk4::gio::prelude::*;
use gtk4::prelude::*;
use gtk4::subclass::prelude::*;
use gtk4::{gdk, glib, graphene, gsk, Widget};
use vte4::prelude::*;

mod manager;
mod panes;
mod resize;

pub(crate) use manager::{GridDragState, Handle, TilerLayout};
pub(crate) use panes::Tally;

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

    /// A slot for one of the tiler's outward-facing callbacks.
    ///
    /// Named rather than written out, because written out it is
    /// `RefCell<Option<Box<dyn Fn(T)>>>` five times over and each layer means
    /// something: `RefCell` because the app installs these after construction,
    /// `Option` because until it does there is nobody to tell, and `Box<dyn>`
    /// because whose closure it is, is the app's business and not the tiler's.
    type Callback<T> = RefCell<Option<Box<dyn Fn(T)>>>;

    /// The same, for the one that is handed a borrow rather than a value. It
    /// cannot go through `Callback<T>`: `Fn(&str)` is higher-ranked over the
    /// lifetime, and `Callback<&str>` would demand one be named here.
    type TextCallback = RefCell<Option<Box<dyn Fn(&str)>>>;

    #[derive(Default)]
    pub struct Tiler {
        pub panes: RefCell<Vec<Rc<Pane>>>,
        pub focus: Cell<usize>,
        pub cwd: RefCell<String>,
        pub title_cb: TextCallback,
        /// Invoked when a pane in this group wants the user - see
        /// `Tiler::set_attention_callback`.
        pub attention_cb: RefCell<Option<Box<dyn Fn()>>>,
        /// Invoked whenever the layout mode changes, however it changed - see
        /// `Tiler::set_mode_callback`.
        pub mode_cb: Callback<Mode>,
        /// Invoked whenever the master ratio, master count or focus index
        /// changes - see `Tiler::set_layout_callback`.
        pub layout_cb: Callback<LayoutState>,
        /// Invoked with the new pane count whenever a pane is attached or
        /// removed - see `Tiler::set_pane_count_callback`.
        pub count_cb: Callback<usize>,
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

        /// Unparents every pane, because a `GtkWidget` subclass has to take its
        /// own children down - GTK will not do it, and a child still parented to
        /// a finalized widget is a dangling parent pointer.
        ///
        /// The round trip first is the same use-after-free `remove_pane` guards
        /// against, reached by the other route panes leave the tree by. See
        /// `panes::settle_input_method` for the mechanism; the short version is
        /// that GTK's Wayland IM context keeps a per-display `current` pointer
        /// which `focus_out` only clears once `zwp_text_input_v3` has been bound,
        /// so a terminal focused and then destroyed inside that first round trip
        /// leaves it dangling and the next text-input event segfaults the process.
        /// Unparenting is the moment GTK could let go of the pane's context, and
        /// it only does if the text-input object has landed by then.
        ///
        /// This is a narrower trigger than `remove_pane`'s, not a different bug: it
        /// needs a whole project removed (`App::remove_project`) inside one round
        /// trip of the process's first `focus_in`. Narrow is not a reason to leave
        /// the same dangling pointer behind on a path that unparents *every* pane
        /// in the group.
        ///
        /// Once for the loop rather than once per pane. What the sync buys is
        /// display-wide and monotonic - the text-input object is either bound or
        /// it isn't, and once bound it stays bound - so the second round trip
        /// would guarantee nothing the first one didn't, and this is the path that
        /// can be holding six of them.
        ///
        /// Safe to force here, which is the part worth being explicit about, since
        /// `dispose` runs during teardown and can be reached with no display at
        /// all. Two things make it safe rather than lucky. The borrow is released
        /// before the round trip, so a Wayland event dispatched inside it cannot
        /// re-enter this and find `panes` already borrowed. And
        /// `settle_input_method` resolves the display through
        /// `root()`/`Display::default()` rather than asserting one exists, so a
        /// dispose running with the display gone or already closed does nothing
        /// instead of crashing - see `display_to_settle` for why the obvious
        /// spelling is the dangerous one.
        fn dispose(&self) {
            let frame = self.panes.borrow().first().map(|pane| pane.frame.clone());
            if let Some(frame) = frame {
                super::panes::settle_input_method(&frame);
            }
            for pane in self.panes.borrow().iter() {
                pane.frame.unparent();
            }
        }
    }

    /// `.pane`'s corner radius, in pixels, for the widget the tiles sit in.
    ///
    /// `style.css` states it as `0.5em` and GTK will not tell anyone what it
    /// resolved that to - border-radius is not a queryable property - so this
    /// re-derives it from the same font size the `em` was relative to. The two
    /// have to agree: this radius is used to cut the tiles out of the floor, and
    /// a radius smaller than the tile's own leaves a sliver of floor inside each
    /// corner, while a larger one shows desktop outside it.
    ///
    /// The fallback is the size Adwaita's default font resolves to, and is only
    /// reached if a context has no font description at all.
    fn tile_radius(widget: &Widget) -> f32 {
        const PANE_RADIUS_EM: f32 = 0.5;
        const FALLBACK_PX_PER_EM: f32 = 15.0;

        let px_per_em = widget
            .pango_context()
            .font_description()
            .map(|desc| {
                let size = desc.size() as f32 / gtk4::pango::SCALE as f32;
                // An absolute size is already in device pixels; a plain one is
                // in points, which is the usual case for a theme font.
                if desc.is_size_absolute() {
                    size
                } else {
                    size * 96.0 / 72.0
                }
            })
            .filter(|px| *px > 0.0)
            .unwrap_or(FALLBACK_PX_PER_EM);

        PANE_RADIUS_EM * px_per_em
    }

    impl Tiler {
        /// The workspace floor - the gutters between tiles and the margin around
        /// them - with the tiles themselves cut out of it.
        ///
        /// Painted here rather than by an ancestor, and that is the whole point.
        /// It used to be `.scaled-content`'s fill, which is the `AdwToolbarView`
        /// that *contains* every tiler, so the floor was painted underneath every
        /// pane. That is invisible while panes are opaque and ruinous the moment
        /// they aren't: alpha in GTK only ever climbs, so a pane at 0.6 sitting on
        /// a floor at 0.92 composites to 0.968 against the desktop. The pane
        /// opacity setting could not do what it said no matter what value it was
        /// given, because the floor beneath it was always most of the answer.
        ///
        /// So exactly one surface paints each pixel of the workspace: the floor in
        /// the gutters, the tile inside a tile. That is the rule `style.css`
        /// already states for the sidebar, where two fills stacked to within a
        /// percent of opaque and the answer was to let only one of them paint.
        ///
        /// The cut is a mask rather than a clip because GSK has no way to subtract
        /// one shape from another: the tiles are drawn as the mask, and
        /// `InvertedAlpha` then paints the floor everywhere they are not. The
        /// focused pane's ring and bloom are unaffected - they are drawn outside
        /// the tile's own allocation, which is exactly the region the floor keeps.
        fn snapshot_floor(&self, snapshot: &gtk4::Snapshot) {
            let obj = self.obj();
            let (width, height) = (obj.width() as f32, obj.height() as f32);
            if width <= 0.0 || height <= 0.0 {
                return;
            }

            let bounds = graphene::Rect::new(0.0, 0.0, width, height);
            let floor = crate::palette::color("field")
                .to_rgba_alpha(crate::appearance::get().window_opacity as f32);

            // The tiles as they are allocated in *this* frame, read off the widget
            // tree rather than recomputed: the layout manager has already placed
            // them, and asking it again is a second answer that can disagree.
            let radius = tile_radius(obj.upcast_ref::<Widget>());
            let mut tiles = Vec::new();
            let mut child = obj.first_child();
            while let Some(current) = child {
                child = current.next_sibling();
                if !current.is_visible() {
                    continue;
                }
                // `compute_bounds` rather than `allocation`, which is deprecated
                // as of GTK 4.12: a child placed by a transform (which is how
                // `TilerLayout` places these) has its position in that transform
                // rather than in an allocation rectangle, and this asks the
                // question in the coordinate space the answer is wanted in.
                let Some(rect) = current.compute_bounds(obj.upcast_ref::<Widget>()) else {
                    continue;
                };
                tiles.push(gsk::RoundedRect::from_rect(rect, radius));
            }

            // Nothing to cut out: an empty group is all floor. (The empty state
            // is a sibling of this widget rather than a child of it, so it paints
            // its own - see `appearance::content_css`.)
            if tiles.is_empty() {
                snapshot.append_color(&floor, &bounds);
                return;
            }

            // The mask is recorded first and the source second - see
            // `gtk_snapshot_push_mask`. The mask's colour is irrelevant; only its
            // alpha is read.
            snapshot.push_mask(gsk::MaskMode::InvertedAlpha);
            let solid = gdk::RGBA::new(1.0, 1.0, 1.0, 1.0);
            for tile in &tiles {
                snapshot.push_rounded_clip(tile);
                snapshot.append_color(&solid, tile.bounds());
                snapshot.pop();
            }
            snapshot.pop();
            snapshot.append_color(&floor, &bounds);
            snapshot.pop();
        }
    }

    impl WidgetImpl for Tiler {
        /// Paints the floor, then the children in order, except the focused one,
        /// which goes last.
        ///
        /// The focused tile is the only thing in the app that draws outside its
        /// own allocation - a two-pixel warm ring and a soft bloom, both of
        /// which land in the gutter around it. GTK paints siblings in child
        /// order, and a pane painted *after* the focused one fills its own
        /// rectangle opaquely straight over whichever part of that bloom reached
        /// into the gutter between them. The result is a glow with a clean
        /// straight bite taken out of one or two sides, depending on where the
        /// focused pane happens to sit in the order.
        ///
        /// Painting it last is the whole fix, and it costs nothing: the panes
        /// don't overlap, so the order is invisible everywhere except in the
        /// gutters where only the bloom reaches.
        ///
        /// Widget order still matches `panes` order - `reflow_children` keeps
        /// that true and the layout manager walks the same list. This changes
        /// only what is drawn on top of what.
        fn snapshot(&self, snapshot: &gtk4::Snapshot) {
            let obj = self.obj();

            // Before any child, and cut to fit them - see `snapshot_floor`.
            self.snapshot_floor(snapshot);

            let focused: Option<Widget> = self
                .panes
                .borrow()
                .get(self.focus.get())
                .map(|pane| pane.frame.clone().upcast());

            let mut child = obj.first_child();
            while let Some(current) = child {
                child = current.next_sibling();
                if focused.as_ref() != Some(&current) {
                    obj.snapshot_child(&current, snapshot);
                }
            }

            if let Some(focused) = focused {
                obj.snapshot_child(&focused, snapshot);
            }
        }
    }
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
        let r = (lm.imp().master_ratio.get() + 0.05).min(crate::layout::MASTER_RATIO_MAX);
        lm.imp().master_ratio.set(r);
        self.queue_allocate();
        self.notify_layout();
    }

    pub fn dec_master_ratio(&self) {
        let lm = self.layout_mgr();
        let r = (lm.imp().master_ratio.get() - 0.05).max(crate::layout::MASTER_RATIO_MIN);
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

    /// Copies the whole of the focused pane's output to the clipboard.
    ///
    /// select-all, copy, unselect: the binding has no `text()` to read the
    /// buffer out directly, so the clipboard path is the way to the same place.
    /// The brief selection flash is left in rather than hidden - it is honest
    /// feedback that something was grabbed, and it is gone by the next frame.
    ///
    /// Returns whether there was a pane to copy from, so the caller can say so.
    pub fn copy_focused_output(&self) -> bool {
        let Some(terminal) = self.focused_terminal() else {
            return false;
        };
        terminal.select_all();
        terminal.copy_clipboard_format(vte4::Format::Text);
        terminal.unselect_all();
        true
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
        lm.imp().master_ratio.set(
            state
                .master_ratio
                .clamp(crate::layout::MASTER_RATIO_MIN, crate::layout::MASTER_RATIO_MAX),
        );
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
