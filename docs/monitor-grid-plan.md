# Monitor Wall Layouts — Implementation Plan

*Adding 2, 3, 5, 6, 7, 8, 10, 12 and 13-cell layouts to a wall that today only
knows four square grids.*

---

## 1. Summary

The wall currently offers four layouts, all square uniform grids: `1x1`,
`2x2`, `3x3`, `4x4` (`crates/monitor_core/src/layout.rs`). The target is a
family of 1–13 cell layouts in which most members are **not** square uniform
grids: some are rectangular (2, 12), most are mixed grids of large and small
cells (3, 5, 6, 7, 8, 10, 13). This plan upgrades the layout model from "N×N
square grid" to "grid units + a table of cell rectangles", and adds the nine
new layouts.

The nine new layouts are **extended layouts**, chosen from a pop-up picker; the
four existing ones stay as direct buttons on the toolbar. Behavioural change:
the digit keys are re-mapped to cell *count* (see §7).

---

## 2. Current architecture (verified)

| Concern | Location |
| --- | --- |
| Layout enum, `ALL`, `cols/rows/capacity/label/from_label/next/prev/index` | `crates/monitor_core/src/layout.rs:5-89` |
| DPAD navigation math (`navigate`) | `crates/monitor_core/src/layout.rs:199-276` |
| Cell rectangles (`tile_rect`), page cells | `crates/monitor_gui/src/grid.rs:58-81` |
| Tile painting, aspect fitting (`fitted`) | `crates/monitor_gui/src/grid.rs:450-496` |
| Toolbar layout buttons | `crates/monitor_gui/src/app.rs:2855-2863` |
| Settings "Grid" row | `crates/monitor_gui/src/app.rs:3184-3193` |
| Digit keys consumed / applied | `crates/monitor_gui/src/app.rs:2531-2536`, `:2576-2582` |
| `Keys.layout` field | `crates/monitor_gui/src/app.rs:571` |
| Status-line key mapping (`layout_hint`) | `crates/monitor_gui/src/app.rs:548-555` |
| `set_layout` | `crates/monitor_gui/src/app.rs:1191-1200` |
| Persisted `Group.layout` | `crates/monitor_core/src/config.rs:489` |
| Config clamp (`normalize`) | `crates/monitor_core/src/config.rs:929-967` |
| Scheduler layout / pages | `crates/monitor_core/src/scheduler.rs:239,299-308,430-453` |
| Nav scopes | `crates/monitor_gui/src/app.rs:967-975`, `crates/monitor_gui/src/nav.rs:84-133` |
| i18n keys | `crates/monitor_i18n/langs/en.ftl:237-240`, `langs/zh-CN.ftl:235-238` |

Key constraints found:

* `GridLayout::rows()` is defined as `self.cols()` (`layout.rs:40-42`) — the
  square assumption is hard-coded.
* `tile_rect` positions a cell with `cell % cols`, `cell / cols`
  (`grid.rs:70-81`) — it assumes every cell is one unit, in row-major order.
* `navigate` derives `col = cell % cols`, `row = cell / cols` and steps by ±1 /
  ±cols (`layout.rs:221-275`).
* Reusable pieces that need **no** change: `page_cells` (`grid.rs:58-67`,
  already capacity-driven), `page_count` / `page_info` (`layout.rs:152-175`),
  `Scheduler::schedule` cells (`scheduler.rs:445-451`, capacity-driven),
  `TileAspect` / `fitted` (`grid.rs:472-496`), zoom (`G1x1` `PageView`).
* Dead code encountered: `GridLayout::from_label` (`layout.rs:58-63`) and
  `Scheduler::cycle_layout` (`scheduler.rs:305-308`) have **no callers**;
  `Schedule::{rows,cols}` (`scheduler.rs:78-84`) also has no callers.

---

## 3. Target layout set

Two groups. Serde string is the persisted identifier.

### Standard (direct toolbar buttons, unchanged identifiers)

| Enum | serde | cells | grid units | `label()` |
| --- | --- | --- | --- | --- |
| `G1x1` | `"1x1"` | 1 | 1×1 | `1x1` |
| `G2x2` | `"2x2"` | 4 | 2×2 | `2x2` |
| `G3x3` | `"3x3"` | 9 | 3×3 | `3x3` |
| `G4x4` | `"4x4"` | 16 | 4×4 | `4x4` |

### Extended (chosen from the picker)

| Enum | serde | cells | grid units | cell list `(col,row,col_span,row_span)`, in assignment order |
| --- | --- | --- | --- | --- |
| `N2` | `"2"` | 2 | 2×1 | `(0,0,1,1) (1,0,1,1)` |
| `N3` | `"3"` | 3 | 3×2 | `(0,0,2,2) (2,0,1,1) (2,1,1,1)` |
| `N5` | `"5"` | 5 | 6×5 | `(0,0,3,3) (3,0,3,3) (0,3,2,2) (2,3,2,2) (4,3,2,2)` |
| `N6` | `"6"` | 6 | 3×3 | `(0,0,2,2) (2,0,1,1) (2,1,1,1) (0,2,1,1) (1,2,1,1) (2,2,1,1)` |
| `N7` | `"7"` | 7 | 4×4 | `(0,0,2,2) (2,0,2,2) (0,2,2,2) (2,2,1,1) (3,2,1,1) (2,3,1,1) (3,3,1,1)` |
| `N8` | `"8"` | 8 | 4×4 | `(0,0,3,3) (3,0,1,1) (3,1,1,1) (3,2,1,1) (3,3,1,1) (0,3,1,1) (1,3,1,1) (2,3,1,1)` |
| `N10` | `"10"` | 10 | 4×4 | `(0,0,2,2) (2,0,2,2) (0,2,1,1) (1,2,1,1) (2,2,1,1) (3,2,1,1) (0,3,1,1) (1,3,1,1) (2,3,1,1) (3,3,1,1)` |
| `N12` | `"12"` | 12 | 4×3 | full 4×3 grid |
| `N13` | `"13"` | 13 | 4×4 | `(0,0,2,2) (2,0,1,1) (3,0,1,1) (2,1,1,1) (3,1,1,1) (0,2,1,1) (1,2,1,1) (2,2,1,1) (3,2,1,1) (0,3,1,1) (1,3,1,1) (2,3,1,1) (3,3,1,1)` |

All 13 variants become members of the enum; `ALL` grows to 13 (order: standard
first, then extended by cell count — see §14). Cell lists are independently
verified against the overview table (each list tiles the grid exactly once, no
gaps, no overlap).

Assignment order = the order channels fill the page. Large ("primary") cells are
listed first. Since the user will arrange cameras by hand, this order is only
the initial pairing and may be re-tuned later.

---

## 4. Data model (`crates/monitor_core/src/layout.rs`)

Add a cell descriptor and a per-layout cell table; keep the enum `Copy`/`Hash`
and its serde strings.

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cell {
    pub col: u8,
    pub row: u8,
    pub col_span: u8,
    pub row_span: u8,
}
```

Changes to `impl GridLayout`:

* `cells(self) -> &'static [Cell]` — the table from §3.
* `grid_cols(self) -> usize`, `grid_rows(self) -> usize` — grid units
  (rename of today's `cols`/`rows`; the old names are removed so every caller
  must be revisited).
* `capacity(self) -> usize` → `self.cells().len()`.
* `label(self)` — `1x1`…`4x4` for standard, `"2"`…`"13"` for extended.
* `from_label` — updated to the new `label()` set (or removed if still unused).
* `next` / `prev` — cycle the new `ALL` order.
* `standard(self) -> bool` — `true` for the four standard variants.
* `numeric_label(self) -> Option<&'static str>` — `Some("6")` for extended,
  `None` for standard (drives the toolbar button text, §8).
* `for_digit(digit: u8) -> Option<GridLayout>` — digit→layout map for §7.

Enum variants and serde:

```rust
pub enum GridLayout {
    #[serde(rename = "1x1")] G1x1,
    #[serde(rename = "2x2")] G2x2,
    #[serde(rename = "3x3")] G3x3,
    #[serde(rename = "4x4")] G4x4,
    #[serde(rename = "2")]  N2,
    #[serde(rename = "3")]  N3,
    #[serde(rename = "5")]  N5,
    #[serde(rename = "6")]  N6,
    #[serde(rename = "7")]  N7,
    #[serde(rename = "8")]  N8,
    #[serde(rename = "10")] N10,
    #[serde(rename = "12")] N12,
    #[serde(rename = "13")] N13,
}
```

`Default` stays `G2x2`.

---

## 5. Geometry (`crates/monitor_gui/src/grid.rs::tile_rect`)

Replace the uniform division with a grid-unit division that honours spans and
the inter-cell gap:

```rust
pub fn tile_rect(area: Rect, layout: GridLayout, cell: usize, gap: f32) -> Rect {
    let c = &layout.cells()[cell];
    let cols = layout.grid_cols() as f32;
    let rows = layout.grid_rows() as f32;
    let unit_w = ((area.width()  - gap * (cols - 1.0)) / cols).max(1.0);
    let unit_h = ((area.height() - gap * (rows - 1.0)) / rows).max(1.0);
    let x = area.left() + c.col as f32 * (unit_w + gap);
    let y = area.top()  + c.row as f32 * (unit_h + gap);
    let w = c.col_span as f32 * unit_w + (c.col_span as f32 - 1.0) * gap;
    let h = c.row_span as f32 * unit_h + (c.row_span as f32 - 1.0) * gap;
    Rect::from_min_size(egui::pos2(x, y), vec2(w, h))
}
```

For unit spans this reduces to the current formula, so the four standard
layouts paint identically. Callers (`paint_view` `grid.rs`/`app.rs:5174,5262`,
the page-slide paths) already pass a `cell` index that matches the `cells()`
order, because `page_cells` fills slots in that order.

No change to `page_cells`, `Tier`, painting or `fitted`. Non-16:9 cells (2, 3,
5, 12) are handled by each camera's own `TileAspect`, as decided.

---

## 6. Navigation (`crates/monitor_core/src/layout.rs::navigate`)

`navigate` must stop assuming a uniform square grid. Replace the `% cols`,
`/ cols` math with **geometric adjacency** over the cell table, which reduces to
the current behaviour for uniform grids (proved below).

Add a pure helper:

```rust
fn neighbour(layout: GridLayout, cell: usize, dir: Direction, visible: usize) -> Option<usize>
```

For a move to the right: candidates are cells `o` with
`o.col == cell.col + cell.col_span` and a non-empty row overlap
(`max(a.row, o.row) < min(a.row + a.row_span, o.row + o.row_span)`); only cells
`< visible` are candidates. Left is symmetrical. Up/Down use rows/columns
symmetrically.

Tie-break when several candidates qualify (a large cell beside two small ones,
e.g. `N3` `(0,0,2,2)` → right has `(2,0)` and `(2,1)`): pick the candidate whose
overlap interval centre is nearest the moving cell's centre; ties broken by the
smaller row (for horizontal) / column (for vertical).

Page turn when there is no neighbour:

* **Right** with `page + 1 < pages`: land on the new page's leftmost column cell
  whose row interval overlaps the current cell (`col == 0`); fall back to the
  first cell.
* **Left** with `page > 0`: land on the previous page's rightmost column cell
  (`col + col_span == grid_cols`) overlapping the current row; fall back to the
  last cell.
* **Up / Down** with no neighbour: `NavigateOutcome::Blocked` (unchanged
  contract — the key is handed to the bars above/below).

Correctness for uniform grids (2×2 shown; 3×3/4×4 identical):

* `cell 1` (row 0, col 1) Right → no neighbour → new page, first column, row
  overlap [0,1) → cell 0 → `focus = next * capacity + 0`, matching today's
  `row * cols`.
* `cell 0` Left → no neighbour → previous page, last column, row overlap
  [0,1) → cell 1 → `focus = prev * capacity + 1`, matching today's
  `row * cols + (cols - 1)`.

The existing `navigate` tests (`layout.rs:278-351`) therefore stay green, and
new tests cover the mixed layouts.

`Scheduler` (`scheduler.rs`) needs no logic change: it only uses `capacity()`
and `next_page`/`prev_page`, both capacity-driven. `Schedule::{rows,cols}`
(`scheduler.rs:78-84`, no callers) should be deleted or renamed to the grid-unit
accessors.

---

## 7. Keyboard

Today: `Keys.layout: [bool; 4]` (`app.rs:571`), consumed for `Num1..Num4`
(`:2531-2536`), applied by indexing `GridLayout::ALL` (`:2576-2582`).

New mapping — **digit = cell count** (user-confirmed):

| key | layout | key | layout |
| --- | --- | --- | --- |
| `Num1` | `G1x1` (1) | `Num6` | `N6` (6) |
| `Num2` | `N2` (2) | `Num7` | `N7` (7) |
| `Num3` | `N3` (3) | `Num8` | `N8` (8) |
| `Num4` | `G2x2` (4) | `Num9` | `G3x3` (9) |
| `Num5` | `N5` (5) | `Num0` | `N10` (10) |

`Num12` / `Num13` do not exist; 12, 13 and 16-cell layouts are picker-only.

Implementation:

* `Keys.layout: [bool; 10]` — indices 0..9 map to digits 1..9 then 0.
* Consume `Num1..Num9` and `Num0` inside the existing `if !typing` guard
  (`app.rs:2528-2537`) so address fields keep their digits.
* Apply with `GridLayout::for_digit(d)` instead of `ALL.get(slot)`
  (`app.rs:2576-2582`).
* **Behavioural change to call out:** `Num4` now selects `2x2` (4 cells) rather
  than `4x4` (16 cells). `4x4`/16-cell has no digit any more and is reachable
  from its toolbar button.

The `wall_seen` "keep the ring alive" check that reads `keys.layout.iter()`
(`app.rs:2682`) keeps working with the wider array.

---

## 8. Toolbar button and picker panel

### 8.1 Toolbar

Keep the four standard buttons exactly as they are (`app.rs:2855-2863`,
iterating the four standard layouts). Insert **one more button** right after
them (before the separator at `:2877`):

* Text: `layout.numeric_label().unwrap_or("…")` — i.e. the current extended
  layout's number (`6`), otherwise `…`.
* Selected/active styling when the current layout is extended.
* Tooltip / status name: new key `toolbar-layout-more`.
* On click: toggle `self.layout_picker`.
* Register in `items` like the others so the remote can reach it
  (`app.rs:2949-2953`).

Add field `layout_picker: bool` to `XgViewApp` (init `false`; near
`show_settings`, `app.rs:630`).

### 8.2 Picker panel (as built)

An `egui::Modal` (`draw_layout_picker`, `app.rs:5419`), the way "Add devices"
is: it centres itself, its backdrop swallows the presses that would otherwise
land behind it, and its own Close - or Back - is the only way out. A press on
the backdrop does **not** close it. Contents:

* Title from a new key `layout-picker-title`, and a Close cross at the far end
  of the title row (the `dialog-close` key as its tooltip). The cross is a
  mouse / touch way out and is registered with the navigation layer, so a remote
  reaches it with the arrows as well as with Back.
* Nine thumbnail buttons, one per extended layout, in cell-count order
  (2, 3, 5, 6, 7, 8, 10, 12, 13), three to a row. Each thumbnail draws its cell
  table scaled into a small box (reusing the §5 geometry on the thumbnail rect),
  with the number drawn overlaid — "graphical layout with numbers", per the
  requirement.
* A thumbnail is a control drawn by hand, the way the toolbar's icons are: the
  rectangle is allocated with `Sense::click` and painted from the same
  `WidgetVisuals` a `selectable_label` uses, so the fill, the number and the
  focus ring all match the toolbar's own layout buttons. The layout in use
  carries the accent fill; the thumbnail the remote is on carries the focus
  ring.
* Click a thumbnail → `self.set_layout(chosen)`, then close the picker. The
  toolbar button now shows the chosen number.
* Close without choosing (Back, or the cross) → leave the layout unchanged; the
  button shows `…` if no extended layout is active.

Keyboard / remote:

* The three rows are three `Axis::Row` scopes (`layout-picker-row-0/1/2`,
  `app.rs:1007-1009`). The sideways arrows walk a row and are spent at either
  end rather than wrapping; the vertical arrows cross between rows at the same
  column through a new `Exit::ScopeAt` (`nav.rs`), so Down from the middle of a
  row lands on the middle of the next rather than back at its left edge. A
  target shorter than the seat falls back to its last member.
* While `layout_picker` is open it owns the keyboard: the wall's own keys - page
  turns and the layout digits - are spent (`app.rs:2619`), and Back closes the
  picker without unwinding the settings panel behind it (`app.rs:2710`).
* The focus moves into the picker when it opens (`layout_picker_handoff` /
  `layout_picker_anchor`) and is given back to the button it was opened from on
  close (`layout_picker_from` → `restore_focus`).
* The picker also joins `panel_anchor`, so a focus dropped while it is up comes
  back to it rather than to the settings panel behind.
* The picker holds the bars open while it is up: `chrome_visible` counts it with
  the panels (`app.rs:1776`), so the toolbar it was opened from does not fade
  away under it after the idle delay.
* Digit keys: because digits are global and mean cell count (§7), pressing
  `5` with the picker open naturally selects `N5`. No extra wiring — but ensure
  the picker does not swallow digits before `handle_keys` maps them, or map them
  inside the picker too.

### 8.3 Settings panel

Mirror the toolbar: four standard toggles plus one `…` button that opens the
same picker, in `settings_display` (`app.rs:3184-3193`). Reuse the
`layout_picker` state and panel so there is a single implementation.

---

## 9. i18n

`layout_hint` (`app.rs:548-555`) is an exhaustive `match`; extending the enum
forces all 13 arms. Add one key per layout plus the picker keys.

`crates/monitor_i18n/langs/en.ftl` (after `:240`):

```ftl
toolbar-layout-2 = Grid 2
toolbar-layout-3 = Grid 3
toolbar-layout-5 = Grid 5
toolbar-layout-6 = Grid 6
toolbar-layout-7 = Grid 7
toolbar-layout-8 = Grid 8
toolbar-layout-10 = Grid 10
toolbar-layout-12 = Grid 12
toolbar-layout-13 = Grid 13
toolbar-layout-more = More layouts
layout-picker-title = Layouts
```

`langs/zh-CN.ftl` (after `:238`) — the Chinese labels (`宫格 2` … `宫格 13`,
`更多布局`, `布局`). Per existing convention, small-cased UI text and
kebab-case keys.

`scripts/check-i18n.sh` must pass: every key the code asks for exists in both
packs, English has no key the Chinese pack lacks, no key is defined twice.

---

## 10. Config compatibility

* Serde: the four standard strings are unchanged, so existing `config.json`
  files load as before; the nine new strings are additions. No migration code
  needed.
* `adopt_flat_view` (`config.rs:626-648`, which parses a legacy flat
  `GridLayout` value) keeps working.
* `normalize` (`config.rs:929-967`) already clamps `page`/`focus` from
  `layout.capacity()`, which is now `cells().len()` — correct for every layout.
* A stale config naming a layout that no longer exists is out of scope (none is
  removed in this change).

---

## 11. Tests

`cargo test --workspace` (currently 307 passing) plus new cases.

`crates/monitor_core/src/layout.rs`:

* `capacities_match_layouts` — extend to all 13 (2,3,5,6,7,8,10,12,13).
* **Tiling invariant** — for every layout: the cells' unit areas sum to
  `grid_cols * grid_rows`, and no two cells overlap. This is the strongest
  guard that the transcribed tables are correct.
* `navigate` — keep the existing uniform-grid tests (must stay green) and add
  mixed cases: `N3` left big cell Right → `(2,0)`; `N5` top-left big cell Down →
  the small cell below-left; `N6` big cell Right → `(2,0)`; page-turn edge cases
  on a partial page.

`crates/monitor_gui/src/grid.rs`:

* `tile_rect` for every layout: all rectangles inside `area`, pairwise
  non-overlapping; and for the four standard layouts the result equals the old
  formula.

`crates/monitor_core/src/config.rs`:

* Round-trip all 13 serde strings; assert `"1x1"`…`"4x4"` still parse.

---

## 12. Files touched

| File | Change |
| --- | --- |
| `crates/monitor_core/src/layout.rs` | `Cell`, cell tables, accessors, `next/prev`, `for_digit`, `navigate`, tests |
| `crates/monitor_gui/src/grid.rs` | `tile_rect`, tests |
| `crates/monitor_gui/src/app.rs` | `layout_picker` field, toolbar button, picker panel, settings row, `layout_hint`, digit keys, `Keys.layout` |
| `crates/monitor_gui/src/nav.rs` | `Exit::ScopeAt` for the picker's same-column row crossing, and its tests |
| `crates/monitor_core/src/scheduler.rs` | drop/rename unused `Schedule::{rows,cols}` |
| `crates/monitor_i18n/langs/en.ftl` | new keys |
| `langs/zh-CN.ftl` | new keys |
| `crates/monitor_core/src/config.rs` | tests only |

`crates/monitor_core/src/lib.rs:56` re-export: add `Cell` if the GUI needs it
(the picker thumbnails may iterate `cells()`).

---

## 13. Implementation order

1. **Model + geometry for the easy rectangular layouts.** Introduce `Cell`,
   `cells()`, `grid_cols/rows`, `capacity`, and rewrite `tile_rect`. Add `N2`
   (2×1) and `N12` (4×3) first — uniform, so they validate the new plumbing
   with minimal risk. Compile and run tests.
2. **Add the mixed layouts.** `N3, N5, N6, N7, N8, N10, N13` + their tables +
   tiling-invariant tests.
3. **Rewrite `navigate`** with geometric adjacency; keep the old tests green,
   add mixed-layout tests.
4. **Keyboard** — `Keys.layout: [bool; 10]`, `for_digit`, consume/apply.
5. **Toolbar button + picker panel + settings row**, with the nav scope and
   keyboard ownership.
6. **i18n** — en + zh-CN, run `scripts/check-i18n.sh`.
7. **Full verification** — `cargo test --workspace`, `scripts/check-i18n.sh`,
   and a manual pass on the new layouts (cell count per page, page turning,
   digit keys, picker, 2/3/5/12-cell aspect handling).

Steps 1–3 are the core and touch only `monitor_core`/`grid.rs`; steps 4–7 are
the UI layer.

---

## 14. Risks and notes

* **`ALL` order.** `ALL` is used by the toolbar and settings loops today; both
  are being rewritten to "standard + picker", so the array order only matters
  for `next/prev` cycling. Order it as standard (1x1,2x2,3x3,4x4) then extended
  by cell count, or as cell count throughout — pick one and document it in the
  doc comment.
* **Nav scopes are single-axis** (`nav.rs:3-29`, "no geometry inside a scope").
  The 3×3 picker grid needs three `Axis::Row` scopes chained by `Exit::Scope`;
  a single `Axis::Column` list is the low-risk fallback. **Resolved:** the three
  rows are chained by a new `Exit::ScopeAt`, which keeps the seat across rows
  rather than landing on the first member — see §8.2.
* **Keyboard ownership / Esc layering** for the picker is the subtlest part —
  reuse the file-dialog precedent (`app.rs:600-611`) so Back does not unwind the
  settings panel behind the picker. **Resolved:** Back closes the picker alone
  (`app.rs:2710`), and the wall's own keys are spent while it is up
  (`app.rs:2619`).
* **Digit-key change** (`4` → 4 cells, not 4×4) is intentional and
  user-confirmed; it will surprise existing users, so it belongs in the release
  notes.
* **Tie-break in `navigate`** for a large cell beside two small ones (N5) is a
  judgement call; the "nearest overlap centre" rule is proposed and should be
  sanity-checked by hand on N5/N8.
* **Cell assignment order** decides which camera lands in the big cell; kept
  "primary first" for now, adjustable later since the user arranges cameras
  manually.
* **Dead code** (`from_label`, `cycle_layout`, `Schedule::{rows,cols}`) should be
  updated or removed while the model changes, so it cannot drift.

---

## 15. Confirmed decisions

1. Identifiers: keep `1x1 2x2 3x3 4x4`; extended layouts use their cell count
   (`2,3,5,6,7,8,10,12,13`).
2. Digit keys map digit → cell count; `0` → 10; 12/13/16 are picker-only.
3. No new aspect modes — each camera keeps its own `TileAspect`.
4. No "primary camera" feature — the user arranges cameras by hand.
5. Toolbar keeps the four standard buttons plus a `…` button that opens a
   graphical, numbered picker; the button shows the chosen number, or `…` when
   none is chosen.