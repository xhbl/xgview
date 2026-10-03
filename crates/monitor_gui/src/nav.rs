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

/// A scope: its axis, the four ways out of it, and whether its members are
/// drawn inside a scroll area.
#[derive(Debug, Clone, Copy)]
pub struct ScopeDef {
    axis: Axis,
    exits: [Option<Exit>; 4],
    scrolling: bool,
}

impl ScopeDef {
    pub fn new(axis: Axis) -> Self {
        Self { axis, exits: [None; 4], scrolling: false }
    }

    pub fn exit(mut self, dir: Dir, exit: Exit) -> Self {
        self.exits[dir.index()] = Some(exit);
        self
    }

    /// Marks this scope's members as living inside a scroll area.
    ///
    /// egui's scroll areas do not follow the focus by themselves, so a body
    /// taller than its window would be walked blind; a member of a scrolling
    /// scope is scrolled into view when the arrows move the focus onto it. See
    /// [`Nav::reveal`] for why this is declared rather than assumed.
    pub fn scrolling(mut self) -> Self {
        self.scrolling = true;
        self
    }
}

/// What kind of control a registered widget is, which decides the arrows it
/// keeps for itself rather than giving to the focus.
///
/// A slider is moved with Left and Right and a [`egui::DragValue`] with Up and
/// Down; the other axis is the one that moves the focus. A plain control keeps
/// neither, and all four arrows walk the scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Kind {
    #[default]
    Plain,
    Slider,
    DragValue,
}

/// Where a registered control lives.
#[derive(Debug, Clone, Copy)]
struct Slot {
    scope: &'static str,
    pos: usize,
    kind: Kind,
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
    value_step: i32,
    /// The control the arrows moved the focus to this frame, to be scrolled
    /// into view as it is drawn. Cleared at the start of every frame.
    revealed: Option<Id>,
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
            value_step: 0,
            revealed: None,
        }
    }

    /// Starts a frame: the layout collected now becomes the previous one.
    pub fn begin(&mut self) {
        self.cur.clear();
        self.cur_scopes.clear();
        self.open.clear();
        self.value_step = 0;
        self.revealed = None;
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

    /// Registers a plain control with the innermost open scope, in draw order.
    pub fn item(&mut self, response: &Response) {
        self.push(response.id, Kind::Plain);
        self.reveal(response);
    }

    /// Registers a control that keeps an axis of arrows for itself - a slider
    /// or a drag value. See [`Kind`].
    pub fn item_kind(&mut self, kind: Kind, response: &Response) {
        self.push(response.id, kind);
        self.reveal(response);
    }

    /// How many controls the innermost open scope has registered so far.
    ///
    /// This is the seat a control that is drawn *later* but belongs *here* takes
    /// with [`item_at`](Self::item_at) - a fold, whose header comes before the
    /// body it hides but whose response only arrives after it.
    pub fn seat(&self) -> usize {
        self.open
            .last()
            .and_then(|scope| self.cur_scopes.get(scope))
            .map_or(0, Vec::len)
    }

    /// Registers a control in the seat it belongs in, rather than at the end.
    ///
    /// Everything the scope registered in the meantime - the body of a fold -
    /// keeps its order and moves up one place, so the walk reaches this control
    /// first. See [`seat`](Self::seat); the response, when there is one, goes to
    /// [`reveal`](Self::reveal) separately.
    pub fn item_at(&mut self, seat: usize, id: Id) {
        self.push(id, Kind::Plain);
        let Some(&scope) = self.open.last() else {
            return;
        };
        let Some(members) = self.cur_scopes.get_mut(scope) else {
            return;
        };
        let last = members.len().saturating_sub(1);
        if seat >= last {
            return;
        }
        let id = members.remove(last);
        members.insert(seat, id);
        // The members that moved have to say so: the walk reads their positions.
        for (pos, member) in members.iter().enumerate() {
            if let Some(slot) = self.cur.get_mut(member) {
                slot.pos = pos;
            }
        }
    }

    fn push(&mut self, id: Id, kind: Kind) {
        let Some(&scope) = self.open.last() else {
            return;
        };
        let members = self.cur_scopes.entry(scope).or_default();
        let pos = members.len();
        members.push(id);
        self.cur.insert(id, Slot { scope, pos, kind });
    }

    /// Brings the control the arrows moved to into view, once.
    ///
    /// egui's scroll areas never follow the focus on their own, so a body taller
    /// than its window is walked blind without this. The request is made as the
    /// control is drawn, which is inside the scroll area that holds it - the
    /// only moment egui reads a scroll request from.
    ///
    /// It is made *only* for a member of a scope marked
    /// [`scrolling`](ScopeDef::scrolling), because egui's scroll request is a
    /// slot on the frame rather than on a scroll area: a control that is not in
    /// one and asked anyway would leave the request behind for the next scroll
    /// area in the same pass, and that one would scroll to the wrong place.
    ///
    /// [`item`](Self::item) calls this itself. It is public for a control whose
    /// response arrives after its registration - see [`item_id`](Self::item_id).
    pub fn reveal(&mut self, response: &Response) {
        if self.revealed == Some(response.id) {
            response.scroll_to_me(None);
            self.revealed = None;
        }
    }

    /// Whether `id` was registered under a scope marked [`ScopeDef::scrolling`].
    fn scrolling(&self, id: Id) -> bool {
        self.prev
            .get(&id)
            .and_then(|slot| self.defs.get(slot.scope))
            .is_some_and(|def| def.scrolling)
    }

    /// [`item`](Self::item), handed the response and giving it back, so a call
    /// site reads `nav.tracked(ui.button("..."))` instead of a binding.
    pub fn tracked(&mut self, response: Response) -> Response {
        self.item(&response);
        response
    }

    /// [`item_kind`](Self::item_kind), handed the response and giving it back.
    pub fn tracked_kind(&mut self, kind: Kind, response: Response) -> Response {
        self.item_kind(kind, &response);
        response
    }

    /// Whether this layer owns `id` - that is, whether it was drawn under a
    /// scope last frame. Anything else is left to egui.
    pub fn owns(&self, id: Id) -> bool {
        self.prev.contains_key(&id)
    }

    /// The kind `id` was registered with last frame, or [`Kind::Plain`].
    pub fn kind(&self, id: Id) -> Kind {
        self.prev.get(&id).map(|slot| slot.kind).unwrap_or_default()
    }

    /// Records a sideways press on the value control that has the focus.
    ///
    /// A value control is adjusted with Left and Right, which the arrow walk
    /// cannot do for it; the layer carries the press here instead, and the
    /// control reads it back with [`value_step`](Self::value_step) as it is
    /// drawn. Cleared at the start of every frame.
    pub fn step_value(&mut self, delta: i32) {
        self.value_step += delta;
    }

    /// How many steps the focused value control should apply this frame.
    ///
    /// Negative is one step down, positive one step up; zero when neither
    /// sideways arrow was pressed.
    pub fn value_step(&self) -> i32 {
        self.value_step
    }

    /// The control a direction press from `focused` moves to.
    ///
    /// `None` means the press is spent: the end of a scope with no exit, or a
    /// focus this layer does not know. What comes back is remembered, and
    /// scrolled into view as it is drawn - see [`reveal`](Self::reveal).
    pub fn step(&mut self, focused: Id, dir: Dir) -> Option<Id> {
        let slot = self.prev.get(&focused)?;
        let def = *self.defs.get(slot.scope)?;
        let members = self.prev_scopes.get(slot.scope)?;

        let delta = match (def.axis, dir) {
            (Axis::Row, Dir::Left) => Some(-1isize),
            (Axis::Row, Dir::Right) => Some(1),
            (Axis::Column, Dir::Up) => Some(-1),
            (Axis::Column, Dir::Down) => Some(1),
            _ => None,
        };
        let mut target = None;
        if let Some(delta) = delta {
            let next = slot.pos as isize + delta;
            if next >= 0 && (next as usize) < members.len() {
                target = Some(members[next as usize]);
            }
        }
        let target = match target {
            Some(target) => Some(target),
            None => match def.exits[dir.index()]? {
                Exit::Scope(key) => self
                    .entries
                    .get(key)
                    .copied()
                    .or_else(|| self.prev_scopes.get(key).and_then(|members| members.first().copied())),
            },
        };

        let revealed = target.filter(|target| self.scrolling(*target));
        self.revealed = revealed;
        target
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
            nav.push(*id, Kind::Plain);
        }
        nav.close();
        nav.open("body");
        for id in body {
            nav.push(*id, Kind::Plain);
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

    #[test]
    fn the_kind_is_kept_for_what_it_was_registered_as() {
        let (slider, plain) = (Id::new("slider"), Id::new("plain"));
        let mut nav = nav();
        let ctx = Context::default();
        nav.begin();
        nav.open("body");
        nav.push(slider, Kind::Slider);
        nav.push(plain, Kind::Plain);
        nav.close();
        nav.finish(&ctx);

        assert_eq!(nav.kind(slider), Kind::Slider);
        assert_eq!(nav.kind(plain), Kind::Plain);
        assert_eq!(nav.kind(Id::new("unknown")), Kind::Plain);
    }

    #[test]
    fn a_move_into_a_scrolling_scope_is_remembered_for_reveal() {
        let (a, x) = (Id::new("a"), Id::new("x"));
        let mut nav = Nav::new(&[
            ("tabs", ScopeDef::new(Axis::Row).exit(Dir::Down, Exit::Scope("body"))),
            ("body", ScopeDef::new(Axis::Column).scrolling().exit(Dir::Up, Exit::Scope("tabs"))),
        ]);
        frame(&mut nav, &[a], &[x]);

        // Down into the column: the control has to be brought into view.
        assert_eq!(nav.step(a, Dir::Down), Some(x));
        assert_eq!(nav.revealed, Some(x));

        // Back up to the row: nothing about a tab is off screen, and asking
        // would leave a scroll request for the body's scroll area to consume.
        assert_eq!(nav.step(x, Dir::Up), Some(a));
        assert_eq!(nav.revealed, None);
    }

    #[test]
    fn a_move_inside_a_scope_that_does_not_scroll_is_not_revealed() {
        let (a, b) = (Id::new("a"), Id::new("b"));
        let mut nav = nav();
        frame(&mut nav, &[a, b], &[]);

        assert_eq!(nav.step(a, Dir::Right), Some(b));
        assert_eq!(nav.revealed, None);
    }

    /// A fold is drawn header first and hands back a response only after its
    /// body has been drawn as well, so its header is registered into a seat
    /// taken before it - which is what puts it ahead of what it hides.
    #[test]
    fn a_control_drawn_last_can_take_an_earlier_seat() {
        let (above, fold, inside) = (Id::new("above"), Id::new("fold"), Id::new("inside"));
        let mut nav = Nav::new(&[("body", ScopeDef::new(Axis::Column))]);
        let ctx = Context::default();

        nav.begin();
        nav.open("body");
        nav.push(above, Kind::Plain);
        let seat = nav.seat();
        nav.push(inside, Kind::Plain); // the fold's body, drawn before its header
        nav.item_at(seat, fold);
        nav.close();
        nav.finish(&ctx);

        assert_eq!(seat, 1, "the fold's seat is between the control above and its body");
        assert_eq!(nav.step(above, Dir::Down), Some(fold), "the fold walks before what it hides");
        assert_eq!(nav.step(fold, Dir::Down), Some(inside));
        assert_eq!(nav.step(inside, Dir::Up), Some(fold));
        assert_eq!(nav.step(fold, Dir::Up), Some(above));
    }

    /// A body taller than the window it is drawn in: every control the arrows
    /// reach has to be brought into view, or the viewer walks blind.
    #[test]
    fn the_control_the_arrows_move_to_is_scrolled_into_view() {
        const CONTROLS: usize = 10;

        let ctx = Context::default();
        let mut nav = Nav::new(&[("body", ScopeDef::new(Axis::Column).scrolling())]);
        let mut ids = Vec::new();
        let mut rects = Vec::new();
        let mut view = egui::Rect::NOTHING;
        let mut offset = 0.0f32;

        // One frame to draw the body and learn its controls, then one frame per
        // press: the window shows about two of the ten.
        for frame in 0..CONTROLS + 2 {
            nav.begin();
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(200.0, 60.0))),
                ..Default::default()
            };
            let _ = ctx.run(input, |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    if frame > 0 {
                        // The arrows are handed out before the controls are
                        // drawn, the way `handle_keys` does it.
                        if frame == 1 {
                            ctx.memory_mut(|memory| memory.request_focus(ids[0]));
                        }
                        if let Some(from) = ctx.memory(|memory| memory.focused()) {
                            if let Some(to) = nav.step(from, Dir::Down) {
                                ctx.memory_mut(|memory| memory.request_focus(to));
                            }
                        }
                    }
                    let out = egui::ScrollArea::vertical().animated(false).show(ui, |ui| {
                        nav.open("body");
                        rects.clear();
                        for control in 0..CONTROLS {
                            let response = ui.add(egui::Button::new(format!("control {control}")));
                            if frame == 0 {
                                ids.push(response.id);
                            }
                            rects.push(response.rect);
                            nav.tracked(response);
                        }
                        nav.close();
                    });
                    view = out.inner_rect;
                    offset = out.state.offset.y;
                });
                // The end of the frame, as the application does it: this
                // frame's layout becomes the one the next arrows are answered
                // from.
                nav.finish(ctx);
            });
        }

        let focused = ctx.memory(|memory| memory.focused()).expect("a control has the focus");
        let index = ids.iter().position(|id| *id == focused).expect("a control of the body");
        assert!(index > 0, "the arrows walked nowhere");
        assert!(offset > 0.0, "the body never scrolled");
        assert!(
            view.contains(rects[index].center()),
            "the focused control is off screen: {index} at {:?}, view {view:?}",
            rects[index]
        );
    }
}