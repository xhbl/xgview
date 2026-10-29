//! The three pickers: layouts, the portrait fit, and the combined information
//! slots.

use super::*;

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
        if !self.layout_picker.is_open() {
            return;
        }
        let mut chosen: Option<GridLayout> = None;
        let mut close = false;

        egui::Modal::new(Id::new("xgview-layout-picker")).show(ctx, |ui| {
            ui.set_width(360.0);
            // The way out, at the far end of the title bar: a cross rather
            // than the word, which stays as its tooltip.
            let bar = dialogs::title_bar(
                ui,
                &monitor_i18n::tr("layout-picker-title"),
                true,
                &monitor_i18n::tr("dialog-close"),
            );
            if let Some(close_button) = bar.close {
                if self.nav.tracked(close_button).clicked() {
                    close = true;
                }
            }

            // Three to a row, each row a scope of the navigation layer: the
            // sideways arrows walk a row and the vertical ones cross between
            // them at the same column, so a remote reaches all nine.
            self.layout_picker.anchor = None;
            for (row, chunk) in GridLayout::EXTENDED.chunks(3).enumerate() {
                let scope = LAYOUT_PICKER_ROWS[row];
                ui.horizontal(|ui| {
                    self.nav.open(scope);
                    for layout in chunk {
                        let selected = self.scheduler.layout() == *layout;
                        let (rect, response) =
                            ui.allocate_exact_size(vec2(108.0, 84.0), Sense::click());
                        if self.layout_picker.anchor.is_none() {
                            self.layout_picker.anchor = Some(response.id);
                        }
                        if ui.is_rect_visible(rect) {
                            let painter = ui.painter();
                            // The two states the rest of the interface draws:
                            // the layout in use carries the accent fill, and the
                            // thumbnail the remote is on carries the focus ring.
                            // Both come from the same `WidgetVisuals` a
                            // `selectable_label` would use, so the fill and the
                            // number match the toolbar's own layout buttons.
                            let visuals = ui.style().interact_selectable(&response, selected);
                            let corner = CornerRadius::same(theme::radius::S);
                            if selected || response.hovered() {
                                painter.rect_filled(rect, corner, visuals.weak_bg_fill);
                            }
                            if visuals.bg_stroke.width > 0.0 {
                                painter.rect_stroke(
                                    rect,
                                    corner,
                                    visuals.bg_stroke,
                                    StrokeKind::Inside,
                                );
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
                        let response = self.nav.tracked(response);
                        self.name(&response, layout_hint(*layout));
                        if response.clicked() {
                            chosen = Some(*layout);
                        }
                    }
                    self.nav.close();
                });
            }
        });

        if let Some(layout) = chosen {
            self.set_layout(layout);
            self.layout_picker.close();
            self.restore_focus = self.layout_picker.from.take();
        } else if close {
            self.layout_picker.close();
            self.restore_focus = self.layout_picker.from.take();
        }
    }

    /// The modal that lists the cameras on the current portrait page and lets
    /// the remote cycle each one's picture fit. A row per camera: its number,
    /// its name, and the aspect it is drawn at - a press on the row turns the
    /// aspect to the next of the five.
    pub(super) fn draw_portrait_aspect_picker(&mut self, ctx: &egui::Context) {
        if !self.portrait_aspect_picker.is_open() {
            return;
        }
        let mut close = false;
        let mut aspect_change: Option<(usize, TileAspect)> = None;

        egui::Modal::new(Id::new("xgview-portrait-aspect-picker")).show(ctx, |ui| {
            ui.set_width(300.0);
            self.portrait_aspect_picker.anchor = None;
            self.nav.open("aspect-picker");
            let bar = dialogs::title_bar(
                ui,
                &monitor_i18n::tr("portrait-aspect-picker-title"),
                true,
                &monitor_i18n::tr("dialog-close"),
            );
            if let Some(close_button) = bar.close {
                if self.nav.tracked(close_button).clicked() {
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
            self.nav.item(&hint_focus);
            self.portrait_aspect_picker.anchor = Some(hint_focus.id);
            ui.add_space(theme::space::M);

            let layout = self.scheduler.layout();
            let page = self.scheduler.page();
            let total = self.config.enabled_count();
            let cells = grid::page_cells(layout, page, total);
            // A cell names a place on the wall, which is a place in the active
            // group's enabled cameras and not in the file: reading the number
            // straight out of `cameras` is how this dialog came to list the
            // built-in group's cameras on any wall. The slot is what the file
            // is indexed by, so the two are crossed through `active_slots`.
            let slots = self.config.active_slots();

            for cell in cells.iter() {
                let Some(index) = cell else { continue };
                let Some(slot) = slots.get(*index).copied() else { continue };
                let Some(camera) = self.config.cameras.get(slot) else { continue };
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
                    let btn = self.nav.tracked(btn);

                    self.name(&btn, "portrait-aspect-entry");
                    if btn.clicked() {
                        aspect_change = Some((slot, aspect.next()));
                    }
                });
                ui.add_space(4.0);
            }
            self.nav.close();
        });

        if let Some((slot, aspect)) = aspect_change {
            if let Some(camera) = self.config.cameras.get_mut(slot) {
                camera.portrait_aspect = aspect;
            }
            self.mark_dirty();
        }
        if close {
            self.portrait_aspect_picker.close();
            self.restore_focus = self.portrait_aspect_picker.from.take();
        }
    }

    /// The modal that lets the viewer pick which atomic items a combined-info
    /// slot stacks.  Two columns: available items on the left, selected items
    /// on the right.  Keyboard is handled in `handle_combined_info_keys`;
    /// this method only draws the UI and handles mouse clicks.
    pub(super) fn draw_combined_info_picker(&mut self, ctx: &egui::Context) {
        if !self.combined_info_picker.is_open() {
            return;
        }
        let slot = self.combined_info_picker_slot;
        let mut selected: Vec<OsdItem> = self.config.combined_info[slot].clone();
        let mut close = false;
        let mut changed = false;
        let mut click_add: Option<usize> = None;

        let focus_id = Id::new("xgview-combined-info-focus");
        let (mut focus_col, mut focus_idx): (usize, usize) =
            ctx.data(|d| d.get_temp(focus_id).unwrap_or((0, 0)));

        egui::Modal::new(Id::new("xgview-combined-info-picker")).show(ctx, |ui| {
            ui.set_width(420.0);

            self.nav.open("combined-info-picker");

            let bar = dialogs::title_bar(
                ui,
                &monitor_i18n::tr_args("combined-info-title", &[("n", (slot + 1).to_string().into())]),
                true,
                &monitor_i18n::tr("dialog-close"),
            );
            if let Some(close_btn) = bar.close {
                if self.nav.tracked(close_btn).clicked() {
                    close = true;
                }
            }
            // The rule is the dialog's first stop; the bar's own rule runs to
            // the border, so the focus ring is kept inside the content.
            let anchor = ui.interact(
                bar.rule.shrink2(vec2(f32::from(ui.style().spacing.menu_margin.left), 0.0)),
                ui.id().with("title-anchor"),
                egui::Sense::focusable_noninteractive(),
            );
            self.nav.item(&anchor);
            self.combined_info_picker.anchor = Some(anchor.id);
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
                    RichText::new(monitor_i18n::tr("combined-info-available")).small().strong(),
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
                    RichText::new(monitor_i18n::tr("combined-info-selected")).small().strong(),
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

            self.nav.close();
        });

        if changed {
            ctx.data_mut(|d| d.insert_temp(focus_id, (focus_col, focus_idx)));
            self.config.combined_info[slot] = selected;
            self.mark_dirty();
        }
        if close {
            self.combined_info_picker.close();
            self.restore_focus = self.combined_info_picker.from.take();
        }
    }
}
