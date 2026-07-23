//! What the app knows, kept apart from what it draws.
//!
//! Before this module there was no such separation: a project was a
//! `GroupEntry` holding a sidebar row, a stack page name and a `Tiler`, and the
//! three were kept in step by hand - a `ListBox` sort function that read the
//! entry vector, a stack whose page names were the row widget names, and a
//! focus index that lived inside the layout manager where nothing else could
//! reach it. Every one of those is a place two orders can disagree.
//!
//! So the order, the active project, and the layout state each project carries
//! live here instead, in plain Rust with no GTK in it, and the widgets read
//! from this rather than from each other. That has three payoffs: the ordering
//! logic can be tested without a display (see this module's tests, which run on
//! a headless machine where every GTK test skips), the header bar can *show*
//! the layout mode because the mode is finally somewhere it can be asked for,
//! and persisting a session becomes a matter of serialising this one struct.
//!
//! Panes are the deliberate exception. They stay owned by `Tiler`, because a
//! pane is a live PTY and a VTE widget rather than a value - and a second list
//! of them here would be exactly the duplicated-order problem this module
//! exists to remove. `PaneState` is defined here because it is a fact about an
//! agent rather than about a widget, but it hangs off the pane itself until
//! there is an IPC channel to drive it.

use crate::layout::Mode;

/// A project's handle, unique for the life of the process.
///
/// A newtype rather than the bare `String` the stack and the sidebar rows used
/// to pass between them: those were the same value doing three jobs at once (a
/// `Stack` page name, a `ListBoxRow` widget name, and the key `entries` was
/// searched by), and nothing stopped an unrelated string being handed to a
/// function expecting one - which is exactly how a text selection dragged in
/// from a terminal pane could reach the reorder logic.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct ProjectId(u32);

impl ProjectId {
    /// The `Stack` page name and `ListBoxRow` widget name for this project.
    /// GTK insists on strings for both; this is the one place that conversion
    /// happens, and `from_widget_name` is its only inverse.
    pub fn as_name(self) -> String {
        self.0.to_string()
    }

    /// Recovers an id from a GTK widget name, or `None` if the string names no
    /// project of ours. The `None` case is load-bearing: a drop payload can be
    /// arbitrary text dragged in from outside the app.
    pub fn from_widget_name(name: &str) -> Option<Self> {
        name.parse().ok().map(ProjectId)
    }
}

/// What an agent is doing, as far as the app can tell.
///
/// Only `Starting`, `Idle` and `Exited` are reachable today - the bell an agent
/// rings says "something happened" and no more, so there is nothing yet that
/// can tell working from waiting. The full set is spelled out now because it is
/// what the pane chrome and the sidebar counts will both be written against,
/// and because a two-state enum would have to be replaced rather than extended.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
// Nothing constructs this yet - it is phase 2's vocabulary, written down in
// phase 1 for the reason the doc comment gives. The alternative to this `allow`
// is a warning that stays in every build until then, which is how a build stops
// being read at all.
#[allow(dead_code)]
pub enum PaneState {
    /// Spawned, nothing heard from it yet.
    #[default]
    Starting,
    /// Working. `tool` names what it's running, when that's known.
    Working { tool: Option<String> },
    /// Finished its turn and waiting on you to say something.
    Idle,
    /// Stopped to ask permission. Distinct from `Idle` because this one is
    /// blocking on an answer rather than merely resting.
    Waiting,
    /// The process is gone.
    Exited,
}

/// How many `hue-N` classes `style.css` defines for sidebar rows.
const HUE_COUNT: u64 = 5;

/// The identity colour class for a project called `name` - `hue-1` through
/// `hue-{HUE_COUNT}`, matching the rules in `style.css`.
///
/// Hashed from the name rather than handed out by row position, so a project's
/// colour is a property *of that project*: it survives reordering the sidebar,
/// closing the group above it, and quitting the app, all of which would shuffle
/// an index-assigned palette and retrain the eye for nothing. The point of the
/// colour is that you learn it once.
///
/// FNV-1a, spelled out here rather than reached for from `std`: `DefaultHasher`
/// is explicitly not promised to be stable across Rust releases, and a toolchain
/// upgrade silently repainting every project in the sidebar is the exact failure
/// this function exists to avoid.
pub fn hue_class(name: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in name.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("hue-{}", hash % HUE_COUNT + 1)
}

/// One project: where it is, what it's called, and how its panes are arranged.
///
/// The layout fields used to live in `TilerLayout`'s private `Cell`s, which made
/// them unreachable from anywhere but the layout manager itself - so the app
/// could tile by them but never report them. They are here so the header bar can
/// show which mode you're in, and so a session can be written to disk.
#[derive(Clone, Debug)]
pub struct Project {
    pub id: ProjectId,
    /// The directory panes in this project are spawned in.
    ///
    /// Written but not yet read: the live copy panes actually spawn against is
    /// `Tiler`'s own `cwd`. This one is the record of it, and it is what a
    /// restored session reopens a project *from* - the one field here that phase
    /// 3 cannot be written without.
    #[allow(dead_code)]
    pub path: String,
    /// What the sidebar calls it - the folder name, except for the startup
    /// project, which is named for what it holds rather than where it is.
    pub name: String,
    /// The `hue-N` class for this project's tally rail, hashed from `name`.
    pub hue: String,
    /// The sidebar icon name.
    pub icon: String,
    pub mode: Mode,
    pub master_ratio: f64,
    pub master_count: usize,
    /// Index of the focused pane within the project's `Tiler`.
    pub focus: usize,
}

impl Project {
    fn new(id: ProjectId, path: &str, name: String, icon: &str) -> Self {
        Project {
            id,
            path: path.to_string(),
            hue: hue_class(&name),
            name,
            icon: icon.to_string(),
            mode: Mode::default(),
            master_ratio: 0.55,
            master_count: 1,
            focus: 0,
        }
    }
}

/// Whether a drop at `y` within a row `height` tall means "below this row"
/// rather than "above" it. Splitting the row at its midpoint is what lets a
/// list of n rows offer all n+1 insertion points: without it the bottom slot
/// would be unreachable, since there's no row *after* the last one to aim at.
pub fn drops_below(y: f64, height: f64) -> bool {
    y > height / 2.0
}

/// Where the project currently at index `from` ends up when it's dropped on the
/// row at index `target` - `below` picking which side of that row it lands on.
///
/// The subtraction is the part worth stating: the dragged project is lifted out
/// of the list before it's put back, so every insertion point after the hole it
/// leaves behind has already shifted down by one by the time it's reinserted.
/// Without this, dragging a row downward always lands it one row short of where
/// it was dropped.
pub fn drop_index(from: usize, target: usize, below: bool) -> usize {
    let insert_at = if below { target + 1 } else { target };
    if from < insert_at {
        insert_at - 1
    } else {
        insert_at
    }
}

/// Every open project, in the order the sidebar shows them, plus which one is
/// visible.
///
/// This vector's order *is* the project order. The sidebar sorts its rows by
/// position in it and the keyboard's next/previous walk it directly, so a
/// reorder is a reorder of this vector and everything else follows - rather than
/// the old arrangement, where the list widget and the cycle logic each had an
/// opinion and a bug was the two of them differing.
#[derive(Default)]
pub struct ProjectStore {
    projects: Vec<Project>,
    active: Option<ProjectId>,
    next_id: u32,
}

impl ProjectStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a project and makes it the active one, returning its id.
    pub fn add(&mut self, path: &str, name: String, icon: &str) -> ProjectId {
        let id = ProjectId(self.next_id);
        self.next_id += 1;
        self.projects.push(Project::new(id, path, name, icon));
        self.active = Some(id);
        id
    }

    // The three below are read by this module's tests and by nothing in the
    // window yet. They are kept rather than trimmed to what phase 1 happens to
    // call, because they are how the store is *asked about* - `iter` is what
    // phase 3 serialises a session out of, and the counts are what phase 2's
    // per-project agent tallies are drawn from. `allow` here rather than at the
    // impl block, so a method that goes genuinely dead later still says so.
    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.projects.is_empty()
    }

    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.projects.len()
    }

    #[allow(dead_code)]
    pub fn iter(&self) -> impl Iterator<Item = &Project> {
        self.projects.iter()
    }

    pub fn get(&self, id: ProjectId) -> Option<&Project> {
        self.projects.iter().find(|p| p.id == id)
    }

    pub fn get_mut(&mut self, id: ProjectId) -> Option<&mut Project> {
        self.projects.iter_mut().find(|p| p.id == id)
    }

    pub fn position(&self, id: ProjectId) -> Option<usize> {
        self.projects.iter().position(|p| p.id == id)
    }

    pub fn active(&self) -> Option<ProjectId> {
        self.active
    }

    pub fn set_active(&mut self, id: ProjectId) {
        if self.position(id).is_some() {
            self.active = Some(id);
        }
    }

    #[allow(dead_code)]
    pub fn active_project(&self) -> Option<&Project> {
        self.active.and_then(|id| self.get(id))
    }

    /// Removes a project, returning the one to fall back to if the removed one
    /// was active - or `None` if it wasn't, or if the removal was refused.
    ///
    /// Refuses to remove the last remaining project: there is always at least
    /// one open, so the window is never left showing nothing.
    pub fn remove(&mut self, id: ProjectId) -> Removal {
        if self.projects.len() <= 1 {
            return Removal::Refused;
        }
        let Some(pos) = self.position(id) else {
            return Removal::Refused;
        };
        let was_active = self.active == Some(id);
        self.projects.remove(pos);

        // The neighbour that slid into the removed project's place, or the new
        // last one if it was removed off the end.
        let fallback = self.projects[pos.min(self.projects.len() - 1)].id;
        if was_active {
            self.active = Some(fallback);
            Removal::Removed {
                fallback: Some(fallback),
            }
        } else {
            Removal::Removed { fallback: None }
        }
    }

    /// Moves a project to index `to`, clamped to the list.
    pub fn move_to(&mut self, id: ProjectId, to: usize) {
        let Some(from) = self.position(id) else {
            return;
        };
        let to = to.min(self.projects.len().saturating_sub(1));
        if from == to {
            return;
        }
        let project = self.projects.remove(from);
        self.projects.insert(to, project);
    }

    /// Answers a drop of `source` onto `target`'s row. `false` means the drop
    /// wasn't ours to take.
    pub fn reorder_onto(&mut self, source: ProjectId, target: ProjectId, below: bool) -> bool {
        let (Some(from), Some(target_pos)) = (self.position(source), self.position(target)) else {
            return false;
        };
        self.move_to(source, drop_index(from, target_pos, below));
        true
    }

    /// Moves the active project one place up (`-1`) or down (`1`).
    ///
    /// Clamped rather than wrapped, unlike `cycle`: moving focus off the end of
    /// the list and round to the top costs nothing, but a project that silently
    /// teleports from the bottom to the top because a key was pressed once too
    /// often is a reorder the user now has to undo.
    pub fn move_active(&mut self, delta: i32) {
        let Some(id) = self.active else { return };
        let Some(from) = self.position(id) else { return };
        if self.projects.len() < 2 {
            return;
        }
        let to = (from as i32 + delta).clamp(0, self.projects.len() as i32 - 1) as usize;
        self.move_to(id, to);
    }

    /// Switches to the next (`1`) or previous (`-1`) project, wrapping around.
    /// Returns the newly active project, or `None` if there was nothing to do.
    pub fn cycle(&mut self, delta: i32) -> Option<ProjectId> {
        let len = self.projects.len();
        if len < 2 {
            return None;
        }
        let current = self.active.and_then(|id| self.position(id)).unwrap_or(0);
        let next = (current as i32 + delta).rem_euclid(len as i32) as usize;
        let id = self.projects[next].id;
        self.active = Some(id);
        Some(id)
    }
}

/// What `ProjectStore::remove` did.
#[derive(PartialEq, Eq, Debug)]
pub enum Removal {
    /// Nothing was removed - the id named no project, or it was the last one.
    Refused,
    /// The project is gone. `fallback` is the project to show instead, set only
    /// when the removed one was the active one.
    Removed { fallback: Option<ProjectId> },
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every test here runs without a display, which is the point of the module:
    /// the ordering rules used to be reachable only through a `ListBox` and a
    /// `Stack`, so testing them meant a GTK thread and a machine with a screen.
    fn store_of(names: &[&str]) -> ProjectStore {
        let mut store = ProjectStore::new();
        for name in names {
            store.add(&format!("/tmp/{name}"), name.to_string(), "folder-symbolic");
        }
        store
    }

    fn names(store: &ProjectStore) -> Vec<String> {
        store.iter().map(|p| p.name.clone()).collect()
    }

    /// `hue_class` promises two things its callers can't check for themselves:
    /// that the class it names is one `style.css` actually defines, and that a
    /// given project keeps the same one forever. A hue outside the range is an
    /// uncoloured row; a hue that drifts is every project in the sidebar
    /// quietly changing colour on some unrelated release.
    #[test]
    fn a_projects_colour_is_in_range_and_never_moves() {
        for name in ["agenttilecli", "Offline_map", "castle-of-the-dreadfort", ""] {
            let class = hue_class(name);
            assert!(
                (1..=HUE_COUNT).any(|n| class == format!("hue-{n}")),
                "{name} got {class}, which style.css does not define"
            );
            assert_eq!(class, hue_class(name), "{name} did not hash the same twice");
        }

        // Pinned literals, not a recomputation: the point is to fail if the
        // hash function is ever swapped or "improved", which is precisely the
        // change that would repaint everyone's sidebar without meaning to.
        assert_eq!(hue_class("agenttilecli"), "hue-2");
        assert_eq!(hue_class("Offline_map"), "hue-4");
        assert_eq!(hue_class("castle-of-the-dreadfort"), "hue-3");
    }

    /// A row is two drop targets, not one - its top half and its bottom half -
    /// and that's what gives a list of n rows the n+1 places a project can go.
    #[test]
    fn a_row_is_split_into_an_above_half_and_a_below_half() {
        assert!(!drops_below(4.0, 30.0), "the top of a row inserts above it");
        assert!(drops_below(26.0, 30.0), "the bottom of a row inserts below");
    }

    /// The off-by-one that makes a *downward* drag land where it was dropped
    /// rather than one row short of it: the dragged project is lifted out of
    /// the list before it's put back, so every slot below the hole it left has
    /// already shifted up by one.
    #[test]
    fn a_downward_drop_accounts_for_the_hole_the_drag_left_behind() {
        // The first project, dropped below the third: it lands third (index 2),
        // not fourth - the two it passed have each moved up one.
        assert_eq!(drop_index(0, 2, true), 2);
        assert_eq!(drop_index(0, 2, false), 1);

        // Dragging *up*, nothing below the hole has moved, so the line drawn is
        // exactly the index it lands on.
        assert_eq!(drop_index(3, 1, false), 1);
        assert_eq!(drop_index(3, 1, true), 2);

        // Dropped back onto itself, either half: it stays put.
        assert_eq!(drop_index(2, 2, false), 2);
        assert_eq!(drop_index(2, 2, true), 2);
    }

    /// A reorder has to move the project in the one order everything reads from,
    /// so the sidebar the user is looking at and the order `[`/`]` walk can't
    /// disagree. That they *can't* is now a property of there being one vector,
    /// rather than something two widgets have to be kept agreeing about.
    #[test]
    fn reordering_moves_a_project_in_the_one_order_there_is() {
        let mut store = store_of(&["alpha", "beta", "gamma"]);
        let (alpha, gamma) = (store.iter().next().unwrap().id, store.active().unwrap());
        assert_eq!(names(&store), ["alpha", "beta", "gamma"]);

        // Drag the last project onto the top half of the first: to the top.
        assert!(store.reorder_onto(gamma, alpha, false));
        assert_eq!(names(&store), ["gamma", "alpha", "beta"]);

        // And the keyboard now cycles in that order: after "gamma" comes "alpha".
        store.set_active(gamma);
        assert_eq!(store.cycle(1), store.position(alpha).map(|_| alpha));
        assert_eq!(store.active_project().unwrap().name, "alpha");

        // Moving the visible project up lifts it above "gamma"...
        store.move_active(-1);
        assert_eq!(names(&store), ["alpha", "gamma", "beta"]);

        // ...and again at the top does nothing, rather than wrapping the project
        // round to the bottom of the list behind the user's back.
        store.move_active(-1);
        assert_eq!(names(&store), ["alpha", "gamma", "beta"]);
    }

    /// A drag *into* the sidebar from outside the app (a text selection from a
    /// terminal pane, say) arrives as a string that names no project. The id
    /// newtype is what makes that a parse failure at the boundary rather than a
    /// lookup miss somewhere deeper in.
    #[test]
    fn a_payload_that_names_no_project_is_declined() {
        let store = store_of(&["alpha"]);
        let alpha = store.iter().next().unwrap().id;

        assert_eq!(ProjectId::from_widget_name(&alpha.as_name()), Some(alpha));
        for junk in ["/some/dragged/text", "", "12x", "-1"] {
            assert_eq!(
                ProjectId::from_widget_name(junk),
                None,
                "{junk:?} parsed as a project id",
            );
        }
    }

    /// Cycling wraps, and says which project it landed on so the caller doesn't
    /// have to ask a second time. A single project has nowhere to go.
    #[test]
    fn cycling_wraps_around_and_stops_at_one_project() {
        let mut store = store_of(&["alpha", "beta", "gamma"]);
        assert_eq!(store.active_project().unwrap().name, "gamma");

        store.cycle(1);
        assert_eq!(store.active_project().unwrap().name, "alpha");
        store.cycle(-1);
        assert_eq!(store.active_project().unwrap().name, "gamma");

        let mut lone = store_of(&["alpha"]);
        assert_eq!(lone.cycle(1), None, "one project has nowhere to cycle to");
    }

    /// Removing the visible project has to name a replacement, or the window is
    /// left showing a stack page that no longer exists. Removing a background
    /// one must *not* - switching away from what the user is looking at because
    /// something closed elsewhere is its own bug.
    #[test]
    fn removing_the_visible_project_falls_back_to_a_neighbour() {
        let mut store = store_of(&["alpha", "beta", "gamma"]);
        let [alpha, beta, gamma] = [0, 1, 2].map(|i| store.iter().nth(i).unwrap().id);

        store.set_active(gamma);
        match store.remove(gamma) {
            Removal::Removed { fallback } => assert_eq!(fallback, Some(beta)),
            other => panic!("expected a removal with a fallback, got {other:?}"),
        }
        assert_eq!(store.active(), Some(beta));

        // Removing a project the user isn't looking at leaves the view alone.
        store.set_active(beta);
        assert_eq!(store.remove(alpha), Removal::Removed { fallback: None });
        assert_eq!(store.active(), Some(beta));

        // And the last one standing can't be closed.
        assert_eq!(store.remove(beta), Removal::Refused);
        assert_eq!(store.len(), 1);
    }

    /// Layout state belongs to the project, not to the layout manager. This is
    /// the whole reason the header bar can report which mode you're in, and the
    /// reason two projects can be in different modes at once.
    #[test]
    fn each_project_carries_its_own_layout_state() {
        let mut store = store_of(&["alpha", "beta"]);
        let [alpha, beta] = [0, 1].map(|i| store.iter().nth(i).unwrap().id);

        assert_eq!(store.get(alpha).unwrap().mode, Mode::Grid);
        store.get_mut(alpha).unwrap().mode = Mode::Monocle;
        store.get_mut(alpha).unwrap().master_ratio = 0.7;

        assert_eq!(store.get(alpha).unwrap().mode, Mode::Monocle);
        assert_eq!(
            store.get(beta).unwrap().mode,
            Mode::Grid,
            "one project's layout mode leaked into another's",
        );
        assert_eq!(store.get(beta).unwrap().master_ratio, 0.55);
    }
}
