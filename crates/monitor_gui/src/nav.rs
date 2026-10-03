//! Explicit directional navigation for a remote control.
//!
//! egui walks the focusable widgets with the arrow keys itself, and it picks
//! the next widget by *geometry*: the candidates are scored by the distance
//! between their axis ranges and the focused widget's, the ones outside a 45
//! degree cone are dropped, and a tie is broken by `id.hash()`. On a desktop
//! with a Tab key that is fine. On a television remote - four arrows, OK and
//! Back, no Tab - it is not: a row of tabs over a column of full width fields
//! puts the field the viewer wants outside the cone (its offset is mostly
//! sideways), two fields under one wide box score identically and the hash
//! order picks between them, and a press can walk the focus out of a window
//! that has no way back in.
//!
//! The compensation for that used to live in the call sites, one special case
//! per window: a tab strip locked by hand, an Up/Down seam answered after the
//! walk had already run, a focus put back on the next frame because the walk
//! had overruled it. Every one of them was a reaction to a decision egui had
//! already made, so none of them could be trusted, and adding a control meant
//! working out the new geometry all over again.
//!
//! This module takes the decision away from egui instead. A widget registered
//! here is navigated by *declaration*: it belongs to a scope, the scope has an
//! axis, and the axis gives the order. There is no geometry inside a scope and
//! therefore nothing to get wrong - Up and Down walk the members of a column,
//! Left and Right the members of a row, in the order they were drawn. Only the
//! crossing between scopes is answered by a flag on the scope itself (a handful
//! of edges for the whole application), and the wall keeps the geometric walk
//! it already had, because a uniform grid of tiles is exactly the shape the
//! walk is good at.
//!
//! egui is kept off the arrows at the end of each frame, with the same
//! [`egui::Memory::set_focus_lock_filter`] the tab strip used: the walk reads
//! it at the start of the *next* frame, when it decides whether a press is
//! offered to it at all. Between that and the arrows being taken out of the
//! input queue in `handle_keys`, the walk never moves the focus of a widget
//! registered here.
//!
//! Only the widgets of a window that opts in are registered, so the layer can
//! be turned on one window at a time.

use std::collections::HashMap;

use egui::{Context, EventFilter, Id, Response};

/// How the members of a scope are laid out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Axis {
    /// Members are side by side: Left / Right walk them, Up / Down leave.
    Row,
    /// Members are stacked: Up / Down walk them, Left / Right leave.
    Column,
}

/// One of the four arrows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dir {
    Up,
    Down,
    Left,
    Right,
}

impl Dir {
    fn index(self) -> usize {
        match self {
            Dir::Up => 0,
            Dir::Down => 1,
            Dir::Left => 2,
            Dir::Right => 3,
        }
    }
}

/// Where a direction press goes once it walks off the end of a scope.
#[derive(Debug, Clone, Copy)]
pub enum Exit {
    /// Into another scope: its entry control, or its first member.
    Scope(&'static str),
}

/// A scope: its axis, and the four ways out of it.
#[derive(Debug, Clone, Copy)]
pub struct ScopeDef {
    axis: Axis,
    exits: [Option<Exit>; 4],
}

impl ScopeDef {
    pub fn new(axis: Axis) -> Self {
        Self { axis, exits: [None; 4] }
    }

    pub fn exit(mut self, dir: Dir, exit: Exit) -> Self {
        self.exits[dir.index()] = Some(exit);
        self
    }
}

/// Where a registered control lives.
#[derive(Debug, Clone, Copy)]
struct Slot {
    scope: &'static str,
    pos: usize,
}

/// The navigation layer.
///
/// One frame is: [`begin`](Self::begin) at the top, [`step`](Self::step) when
/// the arrows are handed out, [`open`](Self::open) / [`item`](Self::item) while
/// the widgets are drawn, and [`finish`](Self::finish) at the end. Direction
/// presses are answered from the previous frame's layout, which is all egui
/// itself has to work with.
#[derive(Debug)]
pub struct Nav {
    defs: HashMap<&'static str, ScopeDef>,
    entries: HashMap<&'static str, Id>,
    prev: HashMap<Id, Slot>,
    prev_scopes: HashMap<&'static str, Vec<Id>>,
    cur: HashMap<Id, Slot>,
    cur_scopes: HashMap<&'static str, Vec<Id>>,
    open: Vec<&'static str>,
}

impl Nav {
    pub fn new(defs: &[(&'static str, ScopeDef)]) -> Self {
        Self {
            defs: defs.iter().copied().collect(),
            entries: HashMap::new(),
            prev: HashMap::new(),
            prev_scopes: HashMap::new(),
            cur: HashMap::new(),
            cur_scopes: HashMap::new(),
            open: Vec::new(),
        }
    }

    /// Starts a frame: the layout collected now becomes the previous one.
    pub fn begin(&mut self) {
        self.cur.clear();
        self.cur_scopes.clear();
        self.open.clear();
    }

    /// Opens a scope for the widgets drawn until the matching [`close`](Self::close).
    pub fn open(&mut self, scope: &'static str) {
        self.open.push(scope);
    }

    /// Closes the innermost scope.
    pub fn close(&mut self) {
        self.open.pop();
    }

    /// Sets the control a scope is entered at, overriding its first member.
    ///
    /// The tab strip uses this for the tab that is on, so that an Up out of the
    /// body comes back to the tab in use rather than to the leftmost one.
    pub fn set_entry(&mut self, scope: &'static str, id: Id) {
        self.entries.insert(scope, id);
    }

    /// Registers a control with the innermost open scope, in draw order.
    pub fn item(&mut self, response: &Response) {
        self.push(response.id);
    }

    fn push(&mut self, id: Id) {
        let Some(&scope) = self.open.last() else {
            return;
        };
        let members = self.cur_scopes.entry(scope).or_default();
        let pos = members.len();
        members.push(id);
        self.cur.insert(id, Slot { scope, pos });
    }

    /// [`item`](Self::item), handed the response and giving it back, so a call
    /// site reads `nav.tracked(ui.button("..."))` instead of a binding.
    pub fn tracked(&mut self, response: Response) -> Response {
        self.item(&response);
        response
    }

    /// Whether this layer owns `id` - that is, whether it was drawn under a
    /// scope last frame. Anything else is left to egui.
    pub fn owns(&self, id: Id) -> bool {
        self.prev.contains_key(&id)
    }

    /// The control a direction press from `focused` moves to.
    ///
    /// `None` means the press is spent: the end of a scope with no exit, or a
    /// focus this layer does not know.
    pub fn step(&self, focused: Id, dir: Dir) -> Option<Id> {
        let slot = self.prev.get(&focused)?;
        let def = self.defs.get(slot.scope)?;
        let members = self.prev_scopes.get(slot.scope)?;

        let delta = match (def.axis, dir) {
            (Axis::Row, Dir::Left) => Some(-1isize),
            (Axis::Row, Dir::Right) => Some(1),
            (Axis::Column, Dir::Up) => Some(-1),
            (Axis::Column, Dir::Down) => Some(1),
            _ => None,
        };
        if let Some(delta) = delta {
            let next = slot.pos as isize + delta;
            if next >= 0 && (next as usize) < members.len() {
                return Some(members[next as usize]);
            }
        }

        match def.exits[dir.index()]? {
            Exit::Scope(key) => self
                .entries
                .get(key)
                .copied()
                .or_else(|| self.prev_scopes.get(key).and_then(|members| members.first().copied())),
        }
    }

    /// Ends a frame: keeps egui's own walk off the arrows of whatever this
    /// layer has focused, and makes this frame's layout the previous one.
    pub fn finish(&mut self, ctx: &Context) {
        if let Some(focused) = ctx.memory(|memory| memory.focused()) {
            if self.cur.contains_key(&focused) || self.prev.contains_key(&focused) {
                ctx.memory_mut(|memory| {
                    memory.set_focus_lock_filter(
                        focused,
                        EventFilter { horizontal_arrows: true, vertical_arrows: true, ..Default::default() },
                    );
                });
            }
        }
        self.prev = std::mem::take(&mut self.cur);
        self.prev_scopes = std::mem::take(&mut self.cur_scopes);
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    /// The shape of the "Add devices" window: a row of tabs over a column.
    fn nav() -> Nav {
        Nav::new(&[
            ("tabs", ScopeDef::new(Axis::Row).exit(Dir::Down, Exit::Scope("body"))),
            ("body", ScopeDef::new(Axis::Column).exit(Dir::Up, Exit::Scope("tabs"))),
        ])
    }

    /// Draws one frame: `tabs` in the row, then `body` in the column.
    fn frame(nav: &mut Nav, tabs: &[Id], body: &[Id]) {
        let ctx = Context::default();
        nav.begin();
        nav.open("tabs");
        for id in tabs {
            nav.push(*id);
        }
        nav.close();
        nav.open("body");
        for id in body {
            nav.push(*id);
        }
        nav.close();
        nav.finish(&ctx);
    }

    #[test]
    fn a_row_walks_its_members_and_stops_at_the_ends() {
        let (a, b, c) = (Id::new("a"), Id::new("b"), Id::new("c"));
        let mut nav = nav();
        frame(&mut nav, &[a, b, c], &[]);

        assert_eq!(nav.step(a, Dir::Right), Some(b));
        assert_eq!(nav.step(b, Dir::Right), Some(c));
        assert_eq!(nav.step(b, Dir::Left), Some(a));
        assert_eq!(nav.step(a, Dir::Left), None, "nothing beside the strip");
        assert_eq!(nav.step(c, Dir::Right), None);
    }

    #[test]
    fn down_leaves_the_strip_for_the_first_control_of_the_body() {
        let (a, b, c) = (Id::new("a"), Id::new("b"), Id::new("c"));
        let (x, y) = (Id::new("x"), Id::new("y"));
        let mut nav = nav();
        frame(&mut nav, &[a, b, c], &[x, y]);

        assert_eq!(nav.step(b, Dir::Down), Some(x));
    }

    #[test]
    fn a_column_walks_its_members_and_up_returns_to_the_tab_in_use() {
        let (a, b, c) = (Id::new("a"), Id::new("b"), Id::new("c"));
        let (x, y) = (Id::new("x"), Id::new("y"));
        let mut nav = nav();
        frame(&mut nav, &[a, b, c], &[x, y]);

        assert_eq!(nav.step(x, Dir::Down), Some(y));
        assert_eq!(nav.step(y, Dir::Up), Some(x));
        assert_eq!(nav.step(y, Dir::Down), None, "end of the column");
        assert_eq!(nav.step(x, Dir::Left), None, "the column has no sideways exit");
        // With no entry set, an Up out of the body falls back to the first tab.
        assert_eq!(nav.step(x, Dir::Up), Some(a));
    }

    #[test]
    fn the_entry_sends_the_return_to_the_tab_in_use() {
        let (a, b, c) = (Id::new("a"), Id::new("b"), Id::new("c"));
        let x = Id::new("x");
        let mut nav = nav();
        frame(&mut nav, &[a, b, c], &[x]);
        nav.set_entry("tabs", c);

        assert_eq!(nav.step(x, Dir::Up), Some(c));
    }

    #[test]
    fn owns_only_the_controls_drawn_under_a_scope() {
        let a = Id::new("a");
        let mut nav = nav();
        frame(&mut nav, &[a], &[]);

        assert!(nav.owns(a));
        assert!(!nav.owns(Id::new("elsewhere")));
    }
}