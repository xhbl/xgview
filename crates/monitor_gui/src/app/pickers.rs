//! The three pickers: layouts, the portrait fit, and the combined information
//! slots.

use super::*;

/// Horizontal centring without taking the row's full height.
///
/// `horizontal_centred` hands the child the whole available rect and centres
/// in both axes, which made the modal fill the screen. A plain row with a
/// measured left pad only spends width.
fn left_pad(ui: &egui::Ui, used: f32) -> f32 {
    ((ui.available_width() - used) * 0.5).max(0.0)
}

impl XgViewApp {
    /// The picker that offers the layouts with no button of their own.
    ///
    /// A modal over the wall, the way "Add devices" is: it centres itself, its
    /// backdrop swallows the presses that would otherwise land behind it, and
    /// its own Close - or Back - is the only way out. The four square layouts
    /// have buttons; this holds the rest, three to a row, each drawn as a
    /// thumbnail of its own grid with the viewport count over it.
    ///
    /// A thumbnail is a control drawn by hand, the way the toolbar's icons are:
    /// the rectangle is allocated with `Sense::click`, so it takes the focus and
    /// the focus ring the rest of the interface draws, and the picture is
    /// painted into it. The three rows are three scopes of the navigation
    /// layer, and the vertical arrows cross between them at the same column.
    pub(super) fn draw_layout_picker(&mut self, ctx: &egui::Context) {
        self.modal_frame(
            ctx,
            "xgview-layout-picker",
            380.0,
            |app| &mut app.layout_picker,
            |app, ui, ()| {
                let mut chosen: Option<GridLayout> = None;
                let mut close = false;
                let mut edit_slot: Option<(usize, Option<egui::Id>, Option<egui::Id>)> = None;

                let bar = dialogs::title_bar(
                    ui,
                    &monitor_i18n::tr("layout-picker-title"),
                    true,
                    &monitor_i18n::tr("dialog-close"),
                );
                if let Some(close_button) = bar.close {
                    if app.nav.tracked(close_button).clicked() {
                        close = true;
                    }
                }

                // First frame of this opening: land on whichever page holds
                // the current layout, so choosing a custom grid and coming
                // back finds it again.
                let tab = app.layout_picker_tab.unwrap_or_else(|| {
                    if app.scheduler.layout().is_custom() { PickerTab::Custom } else { PickerTab::Presets }
                });
                app.layout_picker_tab = Some(tab);

                ui.add_space(theme::space::XS);
                // The tabs are the ring's entry, the same as in Settings:
                // opening the picker leaves it on the active tab, a press
                // selects and keeps it there, and Left/Right select as they
                // land so the body follows the ring.
                app.nav.open("layout-picker-tabs");
                ui.horizontal(|ui| {
                    let labels = [
                        monitor_i18n::tr("layout-picker-tab-presets"),
                        monitor_i18n::tr("layout-picker-tab-custom"),
                    ];
                    let used = labels
                        .iter()
                        .map(|text| super::layout_editor::button_width(ui, text))
                        .sum::<f32>()
                        + ui.spacing().item_spacing.x;
                    ui.add_space(left_pad(ui, used));
                    let mut ids = [Id::NULL; 2];
                    let mut focused = None;
                    for (slot, (page, text)) in [
                        (PickerTab::Presets, labels[0].clone()),
                        (PickerTab::Custom, labels[1].clone()),
                    ]
                    .into_iter()
                    .enumerate()
                    {
                        let response =
                            app.nav.tracked(ui.selectable_label(tab == page, text));
                        if response.clicked() {
                            app.layout_picker_tab = Some(page);
                            response.request_focus();
                        }
                        if response.has_focus() {
                            focused = Some(page);
                        }
                        ids[slot] = response.id;
                    }
                    if let Some(page) = focused {
                        app.layout_picker_tab = Some(page);
                    }
                    let active = app.layout_picker_tab.unwrap_or(tab);
                    let slot = if active == PickerTab::Custom { 1 } else { 0 };
                    // The window opens here, and Up out of the body comes
                    // back to the tab in use.
                    app.layout_picker.anchor = Some(ids[slot]);
                    app.nav.set_entry("layout-picker-tabs", ids[slot]);
                });
                app.nav.close();
                ui.add_space(theme::space::XS);

                match app.layout_picker_tab.unwrap_or(tab) {
                    PickerTab::Presets => {
                        for (row, chunk) in GridLayout::EXTENDED.chunks(3).enumerate() {
                            let scope = LAYOUT_PICKER_ROWS[row];
                            ui.horizontal(|ui| {
                                ui.add_space(left_pad(ui, 108.0 * 3.0 + 2.0 * ui.spacing().item_spacing.x));
                                app.nav.open(scope);
                                for layout in chunk {
                                    let selected = app.scheduler.layout() == *layout;
                                    let (rect, response) =
                                        ui.allocate_exact_size(vec2(108.0, 84.0), Sense::click());
                                    if ui.is_rect_visible(rect) {
                                        let painter = ui.painter();
                                        let visuals = ui.style().interact_selectable(&response, selected);
                                        let corner = CornerRadius::same(theme::radius::S);
                                        if selected || response.hovered() {
                                            painter.rect_filled(rect, corner, visuals.weak_bg_fill);
                                        }
                                        if visuals.bg_stroke.width > 0.0 {
                                            painter.rect_stroke(rect, corner, visuals.bg_stroke, StrokeKind::Inside);
                                        }
                                        let thumb = Rect::from_min_size(
                                            rect.left_top() + vec2(6.0, 6.0),
                                            vec2(rect.width() - 12.0, rect.height() - 30.0),
                                        );
                                        grid::layout_thumbnail(painter, thumb, *layout);
                                        painter.text(
                                            egui::pos2(rect.center().x, rect.bottom() - 12.0),
                                            Align2::CENTER_CENTER,
                                            layout.label(),
                                            FontId::proportional(15.0),
                                            visuals.text_color(),
                                        );
                                    }
                                    let response = app.nav.tracked(response);
                                    app.name(&response, layout_hint(*layout));
                                    if response.clicked() {
                                        chosen = Some(*layout);
                                    }
                                }
                                app.nav.close();
                            });
                        }
                    }
                    PickerTab::Custom => {
                        let slots = app.config.group().custom_layouts;
                        for (row, pair) in [0usize, 2].into_iter().enumerate() {
                            let scope = LAYOUT_PICKER_ROWS[row];
                            ui.horizontal(|ui| {
                                ui.add_space(left_pad(ui, 150.0 * 2.0 + ui.spacing().item_spacing.x));
                                app.nav.open(scope);
                                for (slot, grid) in slots
                                    .iter()
                                    .enumerate()
                                    .filter(|(index, _)| *index >= pair && *index < pair + 2)
                                {
                                    let grid = *grid;
                                    ui.vertical(|ui| {
                                        ui.set_width(150.0);
                                        let (rect, response) =
                                            ui.allocate_exact_size(vec2(150.0, 116.0), Sense::click());
                                        let painter = ui.painter();
                                        let active = match app.scheduler.layout() {
                                            GridLayout::Custom(g) => Some(g) == grid,
                                            _ => false,
                                        };
                                        let visuals = ui.style().interact_selectable(&response, active);
                                        let corner = CornerRadius::same(theme::radius::S);
                                        if active || response.hovered() {
                                            painter.rect_filled(rect, corner, visuals.weak_bg_fill);
                                        }
                                        if grid.is_some() && visuals.bg_stroke.width > 0.0 {
                                            painter.rect_stroke(rect, corner, visuals.bg_stroke, StrokeKind::Inside);
                                        }
                                        match grid {
                                            Some(grid) => {
                                                let thumb = Rect::from_min_size(
                                                    rect.left_top() + vec2(6.0, 6.0),
                                                    vec2(rect.width() - 12.0, rect.height() - 32.0),
                                                );
                                                grid::layout_thumbnail(painter, thumb, GridLayout::Custom(grid));
                                                painter.text(
                                                    egui::pos2(rect.center().x, rect.bottom() - 12.0),
                                                    Align2::CENTER_CENTER,
                                                    grid.display_label(),
                                                    FontId::proportional(13.0),
                                                    visuals.text_color(),
                                                );
                                            }
                                            None => {
                                                let frame = rect.shrink(6.0);
                                                let corners = [
                                                    frame.left_top(),
                                                    frame.right_top(),
                                                    frame.right_bottom(),
                                                    frame.left_bottom(),
                                                    frame.left_top(),
                                                ];
                                                for dash in egui::Shape::dashed_line(
                                                    &corners,
                                                    egui::Stroke::new(1.0_f32, theme::TILE_BORDER),
                                                    8.0,
                                                    6.0,
                                                ) {
                                                    painter.add(dash);
                                                }
                                                painter.text(
                                                    rect.center(),
                                                    Align2::CENTER_CENTER,
                                                    "+",
                                                    FontId::proportional(28.0),
                                                    theme::TEXT_DIM,
                                                );
                                                if (response.hovered() || response.has_focus())
                                                    && visuals.bg_stroke.width > 0.0 {
                                                        painter.rect_stroke(rect, corner, visuals.bg_stroke, StrokeKind::Inside);
                                                    }
                                            }
                                        }
                                        let response = app.nav.tracked(response);
                                        app.name(&response, "layout-picker-custom-slot");
                                        let tile_id = response.id;
                                        if response.clicked() {
                                            match grid {
                                                Some(grid) => chosen = Some(GridLayout::Custom(grid)),
                                                None => {
                                                    let from = (!response.clicked_by(egui::PointerButton::Primary))
                                                        .then_some(tile_id);
                                                    edit_slot = Some((slot, from, from));
                                                }
                                            }
                                        }
                                        // The pencil belongs to the tile, not
                                        // to a row below it: it sits in the
                                        // tile's bottom-right corner so the two
                                        // cards keep one height and one row.
                                        let edit_rect = Rect::from_min_size(
                                            egui::pos2(rect.right() - 28.0, rect.bottom() - 28.0),
                                            vec2(24.0, 24.0),
                                        );
                                        let edit = icons::button_in(
                                            ui,
                                            edit_rect,
                                            egui::Id::new(("layout-picker-edit", slot)),
                                            icons::Icon::Edit,
                                            false,
                                            false,
                                            &monitor_i18n::tr("layout-picker-edit"),
                                            false,
                                        );
                                        let edit = app.nav.tracked(edit);
                                        app.name(&edit, "layout-picker-edit");
                                        if edit.clicked() {
                                            let from = (!edit.clicked_by(egui::PointerButton::Primary))
                                                .then_some(edit.id);
                                            // Confirm lands on the slot the pencil
                                            // edits; Cancel goes back to the pencil
                                            // itself. Pointer presses move no ring.
                                            edit_slot = Some((slot, from, from.map(|_| tile_id)));
                                        }
                                    });
                                }
                                app.nav.close();
                            });
                        }
                    }
                }

                if let Some((slot, from, slot_focus)) = edit_slot {
                    let existing = app.config.group().custom_layouts[slot];
                    let mut state = EditorState::open(slot, existing);
                    state.slot_focus = slot_focus;
                    app.layout_editor.open(state);
                    app.layout_editor.from = from;
                    app.layout_picker_tab = None;
                    // The focus belongs to the editor now, not back to the
                    // toolbar button that first opened the picker.
                    return Answer::CLOSE_LEAVING_THE_FOCUS;
                }
                if let Some(layout) = chosen {
                    app.set_layout(layout);
                    app.layout_picker_tab = None;
                    return Answer::CLOSE;
                }
                if close {
                    app.layout_picker_tab = None;
                    Answer::CLOSE
                } else {
                    Answer::STAY
                }
            },
        );
    }

    /// The modal that lists the cameras on the current portrait page and lets
    /// the remote cycle each one's picture fit. A row per camera: its number,
    /// its name, and the aspect it is drawn at - a press on the row turns the
    /// aspect to the next of the five.
    pub(super) fn draw_portrait_aspect_picker(&mut self, ctx: &egui::Context) {
        self.modal_frame(
            ctx,
            "xgview-portrait-aspect-picker",
            300.0,
            |app| &mut app.portrait_aspect_picker,
            |app, ui, ()| {
                let mut close = false;
                let mut aspect_change: Option<(usize, TileAspect)> = None;

                app.portrait_aspect_picker.anchor = None;
                app.nav.open("aspect-picker");
                let bar = dialogs::title_bar(
                    ui,
                    &monitor_i18n::tr("portrait-aspect-picker-title"),
                    true,
                    &monitor_i18n::tr("dialog-close"),
                );
                if let Some(close_button) = bar.close {
                    if app.nav.tracked(close_button).clicked() {
                        close = true;
                    }
                }
                let hint = ui.label(
                    RichText::new(monitor_i18n::tr("portrait-aspect-picker-hint"))
                        .small()
                        .color(theme::TEXT_DIM),
                );
                let hint_focus = ui.interact(
                    hint.rect,
                    hint.id.with("focus"),
                    egui::Sense::focusable_noninteractive(),
                );
                app.nav.item(&hint_focus);
                app.portrait_aspect_picker.anchor = Some(hint_focus.id);
                ui.add_space(theme::space::M);

                let layout = app.scheduler.layout();
                let page = app.scheduler.page();
                let total = app.config.enabled_count();
                let cells = grid::page_cells(layout, page, total);
                // A cell names a place on the wall, which is a place in the
                // active group's enabled cameras and not in the file: reading the
                // number straight out of `cameras` is how this dialog came to
                // list the built-in group's cameras on any wall. The slot is what
                // the file is indexed by, so the two are crossed through
                // `active_slots`.
                let slots = app.config.active_slots();

                for cell in cells.iter() {
                    let Some(index) = cell else { continue };
                    let Some(slot) = slots.get(*index).copied() else { continue };
                    let Some(camera) = app.config.cameras.get(slot) else { continue };
                    let name = camera.short_label(18);
                    let aspect = camera.portrait_aspect;
                    ui.horizontal(|ui| {
                        ui.label(format!("{}.", *index + 1));
                        ui.label(name);
                        let btn = ui
                            .small_button(
                                RichText::new(monitor_i18n::tr(aspect.label()))
                                    .small()
                                    .color(theme::ACCENT),
                            )
                            .on_hover_text(monitor_i18n::tr("cameras-aspect-tip"));
                        let btn = app.nav.tracked(btn);

                        app.name(&btn, "portrait-aspect-entry");
                        if btn.clicked() {
                            aspect_change = Some((slot, aspect.next()));
                        }
                    });
                    ui.add_space(4.0);
                }
                app.nav.close();

                if let Some((slot, aspect)) = aspect_change {
                    if let Some(camera) = app.config.cameras.get_mut(slot) {
                        camera.portrait_aspect = aspect;
                    }
                    app.mark_dirty();
                }
                if close {
                    Answer::CLOSE
                } else {
                    Answer::STAY
                }
            },
        );
    }

    /// The modal that lets the viewer pick which atomic items a combined-info
    /// slot stacks.  Two columns: available items on the left, selected items
    /// on the right.  Keyboard is handled in `handle_combined_info_keys`;
    /// this method only draws the UI and handles mouse clicks.
    pub(super) fn draw_combined_info_picker(&mut self, ctx: &egui::Context) {
        self.modal_frame(
            ctx,
            "xgview-combined-info-picker",
            420.0,
            |app| &mut app.combined_info_picker,
            |app, ui, ()| {
                let slot = app.combined_info_picker_slot;
                let mut selected: Vec<OsdItem> = app.config.combined_info[slot].clone();
                let mut close = false;
                let mut changed = false;
                let mut click_add: Option<usize> = None;

                let focus_id = Id::new("xgview-combined-info-focus");
                let (mut focus_col, mut focus_idx): (usize, usize) =
                    ui.ctx().data(|d| d.get_temp(focus_id).unwrap_or((0, 0)));

                app.nav.open("combined-info-picker");

                let bar = dialogs::title_bar(
                    ui,
                    &monitor_i18n::tr_args(
                        "combined-info-title",
                        &[("n", (slot + 1).to_string().into())],
                    ),
                    true,
                    &monitor_i18n::tr("dialog-close"),
                );
                if let Some(close_btn) = bar.close {
                    if app.nav.tracked(close_btn).clicked() {
                        close = true;
                    }
                }
                // The rule is the dialog's first stop; the bar's own rule runs
                // to the border, so the focus ring is kept inside the content.
                let anchor = ui.interact(
                    bar.rule
                        .shrink2(vec2(f32::from(ui.style().spacing.menu_margin.left), 0.0)),
                    ui.id().with("title-anchor"),
                    egui::Sense::focusable_noninteractive(),
                );
                app.nav.item(&anchor);
                app.combined_info_picker.anchor = Some(anchor.id);
                ui.add_space(4.0);

                let available: Vec<OsdItem> = OsdItem::ATOMIC
                    .iter()
                    .filter(|atom| !selected.contains(atom))
                    .copied()
                    .collect();

                let mut click_remove: Option<usize> = None;

                ui.columns(2, |columns| {
                    columns[0].spacing_mut().item_spacing.y = 2.0;
                    columns[0].label(
                        RichText::new(monitor_i18n::tr("combined-info-available"))
                            .small()
                            .strong(),
                    );
                    for (i, atom) in available.iter().enumerate() {
                        let is_focused = focus_col == 0 && focus_idx == i;
                        let response = columns[0].selectable_label(
                            is_focused,
                            RichText::new(monitor_i18n::tr(atom.label())).small(),
                        );
                        if response.clicked() {
                            click_add = Some(i);
                        }
                    }

                    columns[1].spacing_mut().item_spacing.y = 2.0;
                    columns[1].label(
                        RichText::new(monitor_i18n::tr("combined-info-selected"))
                            .small()
                            .strong(),
                    );
                    for (i, atom) in selected.iter().enumerate() {
                        let is_focused = focus_col == 1 && focus_idx == i;
                        let response = columns[1].selectable_label(
                            is_focused,
                            RichText::new(monitor_i18n::tr(atom.label())).small(),
                        );
                        if response.clicked() {
                            click_remove = Some(i);
                        }
                    }
                });

                if let Some(i) = click_add {
                    let atom = available[i];
                    selected.push(atom);
                    changed = true;
                    if available.len() > 1 {
                        focus_col = 0;
                        focus_idx = i.min(available.len() - 2);
                    } else {
                        focus_col = 1;
                        focus_idx = selected.len() - 1;
                    }
                }
                if let Some(i) = click_remove {
                    selected.remove(i);
                    changed = true;
                    if selected.is_empty() {
                        focus_col = 0;
                        focus_idx = i.min(available.len().saturating_sub(1));
                    } else {
                        focus_col = 1;
                        focus_idx = i.min(selected.len() - 1);
                    }
                }

                app.nav.close();

                if changed {
                    ui.ctx().data_mut(|d| d.insert_temp(focus_id, (focus_col, focus_idx)));
                    app.config.combined_info[slot] = selected;
                    app.mark_dirty();
                }
                if close {
                    Answer::CLOSE
                } else {
                    Answer::STAY
                }
            },
        );
    }
}
