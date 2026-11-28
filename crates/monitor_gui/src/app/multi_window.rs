//! Desktop workspace: page selection is independent of native viewport identity.
use super::*;
use monitor_core::config::{LocalWorkspace, WallWindow};

impl XgViewApp {
    pub(super) fn persist_wall_windows(&mut self) {
        if self.workspace_saved.is_none() { return; }
        let cameras = self.config.active_cameras();
        let snapshot = LocalWorkspace {
            windows: self.extra_windows.clone(), layouts: self.scheduler.layouts().to_vec(),
            order: self.scheduler.order().iter().filter_map(|&i| cameras.get(i).map(|c| c.id.clone())).collect(),
            page: self.scheduler.page(), group: self.config.active_index(),
        };
        if self.workspace_saved.as_ref() != Some(&snapshot) {
            match snapshot.save(&LocalWorkspace::path_for(&self.config_path)) {
                Ok(()) => self.workspace_saved = Some(snapshot),
                Err(e) => tracing::warn!("workspace save failed: {e}"),
            }
        }
    }
    pub(super) fn wall_window_controls(&mut self, ui: &mut egui::Ui, items: &mut Vec<(Id, Rect)>) {
        let add = icons::button(ui, Icon::WindowPlus, false, &monitor_i18n::tr("settings-add-wall-window"));
        items.push((add.id, add.rect));
        if add.clicked() {
            let id = (1..).find(|id| !self.extra_windows.iter().any(|w| w.id == *id)).unwrap();
            let page = (self.scheduler.page()+1..self.scheduler.page_count()).find(|p| !self.extra_windows.iter().any(|w| w.page == *p)).unwrap_or(self.scheduler.page());
            self.wall_opened.remove(&id);
            self.extra_windows.push(WallWindow { id, page, geometry: None, fullscreen: false });
            self.needs_sync = true;
        }
        let pin = icons::button(ui, Icon::WallPin, self.wall_pinned, &monitor_i18n::tr("wall-pin"));
        items.push((pin.id,pin.rect));
        if pin.clicked() { self.wall_pinned = !self.wall_pinned; self.wall_drag = None; self.slide = Slide::default(); self.wall_slides.clear(); }
        let save = icons::button(ui, Icon::WallSave, self.workspace_saved.is_some(), &monitor_i18n::tr("wall-save"));
        items.push((save.id, save.rect));
        if save.clicked() {
            if self.workspace_saved.is_some() { self.clear_wall_confirmation = true; }
            else {
                let initial = LocalWorkspace::default();
                if initial.save(&LocalWorkspace::path_for(&self.config_path)).is_ok() { self.workspace_saved = Some(initial); self.persist_wall_windows(); }
            }
        }
    }
    pub(super) fn wall_edit_drag(&mut self, ui: &egui::Ui, area: Rect, view: &PageView, actions: &TileActions) {
        if ui.input(|i| i.key_pressed(egui::Key::Escape)) { self.wall_drag = None; return; }
        if actions.drag_started { self.wall_drag = actions.drag_camera; }
        if let Some(pos) = wall_pointer(ui) {
            if let Some((cell, target)) = view.cells.iter().enumerate().find(|(cell,_)| grid::tile_rect(area,view.layout,*cell,grid::LINE).contains(pos)) {
                if self.wall_drag.is_some() && target.is_some() {
                    ui.painter().rect_stroke(grid::tile_rect(area,view.layout,cell,grid::LINE), 0.0, egui::Stroke::new(2.0_f32,egui::Color32::LIGHT_BLUE), egui::StrokeKind::Inside);
                }
                if self.wall_drag.is_some() { self.wall_drop_target = *target; }
            }
        }
        #[cfg(not(windows))]
        { self.wall_release |= actions.drag_stopped || ui.input(|i| i.pointer.primary_released()); }
        if let (Some(index), Some(pos)) = (self.wall_drag, wall_pointer(ui)) {
            let cameras = self.config.active_cameras();
            {
                let cell = view.cells.iter().position(|v| *v == Some(index));
                let source = grid::tile_rect(area, view.layout, cell.unwrap_or(0), grid::LINE);
                if cell.is_some() { ui.painter().rect_filled(source, 0.0, egui::Color32::from_black_alpha(140)); }
                let rect = Rect::from_center_size(pos, source.size());
                let mut tile = self.tile_of(Id::new("wall-drag-preview"), Some(index), view.first + cell.unwrap_or(0), &cameras, view.cells.len(), false, false);
                tile.channel = self.channels.get(&index);
                tile.video = self.textures.get(&index).map(|t| t.surface);
                tile.on_video_layer = false; tile.rotated = false; tile.picture_zoom = None;
                tile.state_frame = None; tile.recording = None; tile.clock = None; tile.focused = false;
                let painter = ui.ctx().layer_painter(egui::LayerId::new(egui::Order::Tooltip, Id::new("wall-drag-preview")));
                grid::paint_visual(&painter.with_clip_rect(rect), rect, &tile);
                painter.rect_stroke(rect, 0.0, egui::Stroke::new(2.0_f32, egui::Color32::LIGHT_BLUE), egui::StrokeKind::Inside);
                ui.ctx().request_repaint();
            }
        }
    }
    pub(super) fn finish_wall_drag(&mut self, ctx: &egui::Context) {
        if self.wall_drag.is_none() { return; }
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) { self.wall_drag = None; return; }
        #[cfg(windows)]
        { self.wall_release |= !native_wall::primary_down(); }
        if self.wall_release {
            if let (Some(a), Some(b)) = (self.wall_drag.take(), self.wall_drop_target.take()) { self.scheduler.swap_channels(a,b); self.needs_sync = true; self.persist_wall_windows(); }
        } else { ctx.request_repaint(); }
    }
    pub(super) fn draw_extra_windows(&mut self, ctx: &egui::Context) {
        if self.clear_wall_confirmation {
            egui::Window::new(monitor_i18n::tr("wall-save")).collapsible(false).show(ctx, |ui| {
                ui.label(monitor_i18n::tr("wall-clear-question"));
                if ui.button(monitor_i18n::tr("wall-clear-yes")).clicked() {
                    let result = std::fs::remove_file(LocalWorkspace::path_for(&self.config_path));
                    if result.is_ok() || result.as_ref().is_err_and(|e| e.kind()==std::io::ErrorKind::NotFound) { self.workspace_saved = None; self.clear_wall_confirmation = false; }
                }
                if ui.button(monitor_i18n::tr("wall-clear-no")).clicked() { self.clear_wall_confirmation = false; }
            });
        }
        let root_rect = ctx.input(|i| i.viewport().outer_rect.or(i.viewport().inner_rect));
        let root_monitor = ctx.input(|i| i.viewport().monitor_size);
        let windows = self.extra_windows.clone();
        for mut window in windows {
            let old_page = window.page;
            window.page = window.page.min(self.scheduler.page_count()-1);
            if old_page != window.page { self.needs_sync = true; }
            let id = egui::ViewportId::from_hash_of(("xgview-extra-wall",window.id));
            let mut builder = egui::ViewportBuilder::default().with_title(format!("{} — {}",monitor_core::APP_DISPLAY_NAME,window.id)).with_inner_size([960.,540.]).with_min_inner_size([320.,200.]);
            // Never feed observed geometry back into the viewport builder every frame.
            // Native decorations make inner and outer origins different.
            let first = self.wall_opened.insert(window.id);
            #[cfg(windows)]
            if first && window.geometry.is_some_and(|[x,y,w,h]| native_wall::monitor_missing(Rect::from_min_size(egui::pos2(x,y),egui::vec2(w,h)),ctx.pixels_per_point())) { window.geometry=None; window.fullscreen=false; }
            if first {
                if let Some([x,y,w,h]) = window.geometry.filter(|g| g.iter().all(|v| v.is_finite()) && g[2] >= 320. && g[3] >= 200.) {
                    builder = builder.with_position([x,y]).with_inner_size([w,h]);
                } else if let Some(root) = root_rect {
                    let size = root_monitor.map(|m| egui::vec2(960.0_f32.min(m.x * 0.7), 540.0_f32.min(m.y * 0.7))).unwrap_or(egui::vec2(960.,540.));
                    builder = builder.with_position(root.center() - size * 0.5).with_inner_size(size);
                }
                if window.fullscreen { builder = builder.with_fullscreen(true); }
            } else {
                builder = egui::ViewportBuilder::default().with_title(format!("{} — {}",monitor_core::APP_DISPLAY_NAME,window.id)).with_min_inner_size([320.,200.]);
            }
            let mut closed = false;
            let mut slide = self.wall_slides.remove(&window.id).unwrap_or_default();
            let old_preview = slide_neighbour(old_page, &slide, self.scheduler.page_count());
            ctx.show_viewport_immediate(id,builder,|child,class| {
                if class == egui::ViewportClass::Embedded || child.input(|i| i.viewport().close_requested()) { closed = true; return; }
                #[cfg(windows)]
                if let Some(rect)=child.input(|i| i.viewport().outer_rect) {
                    if native_wall::monitor_missing(rect,child.pixels_per_point()) {
                        if let Some(root)=root_rect {
                            let size=root_monitor.unwrap_or(root.size())*0.7;
                            window.fullscreen=false;
                            child.send_viewport_cmd(egui::ViewportCommand::Fullscreen(false));
                            child.send_viewport_cmd(egui::ViewportCommand::InnerSize(size));
                            child.send_viewport_cmd(egui::ViewportCommand::OuterPosition(root.center()-size*0.5));
                            window.geometry=Some([root.center().x-size.x*0.5,root.center().y-size.y*0.5,size.x,size.y]);
                            child.request_repaint();
                            return;
                        }
                    }
                }
                if let Some(fullscreen) = child.input(|i| i.viewport().fullscreen) { window.fullscreen = fullscreen; }
                if !window.fullscreen { if let Some((outer, inner)) = child.input(|i| i.viewport().outer_rect.zip(i.viewport().inner_rect)) { window.geometry = Some([outer.min.x,outer.min.y,inner.width(),inner.height()]); } }
                egui::TopBottomPanel::top("extra-toolbar").show(child,|ui| {
                    ui.horizontal_wrapped(|ui| {
                        for layout in GridLayout::ALL { if ui.selectable_label(self.scheduler.layout_at(window.page)==layout,layout.label()).clicked() { self.scheduler.set_page_layout(window.page,layout); self.needs_sync = true; } }
                        if ui.button("‹").clicked() && window.page > 0 { slide.start(-1., Some(self.wall_page_view(window.page))); window.page -= 1; self.needs_sync = true; }
                        ui.label(format!("{} / {}",window.page+1,self.scheduler.page_count()));
                        if ui.button("›").clicked() && window.page+1 < self.scheduler.page_count() { slide.start(1., Some(self.wall_page_view(window.page))); window.page += 1; self.needs_sync = true; }
                        let fullscreen = icons::button(ui, if window.fullscreen { Icon::Collapse } else { Icon::Expand }, window.fullscreen, &monitor_i18n::tr(if window.fullscreen { "toolbar-fullscreen-leave-tip" } else { "toolbar-fullscreen-enter-tip" }));
                        if fullscreen.clicked() || child.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::F11)) { window.fullscreen = !window.fullscreen; child.send_viewport_cmd(egui::ViewportCommand::Fullscreen(window.fullscreen)); }
                    });
                });
                egui::CentralPanel::default().frame(egui::Frame::default().fill(theme::BACKGROUND)).show(child,|ui| {
                    let area = ui.available_rect_before_wrap();
                    if self.blackout_on { ui.painter().rect_filled(area,0.,egui::Color32::BLACK); return; }
                    let view = self.wall_page_view(window.page);
                    let cameras = self.config.active_cameras();
                    let dt = child.input(|i| i.stable_dt).clamp(0.,0.05);
                    if !slide.dragging { slide.drag *= 1. - (dt*16.).min(1.); if slide.drag.abs()<0.5 { slide.drag=0.; } }
                    slide.offset *= 1. - (dt*11.).min(1.);
                    if slide.offset.abs()<0.002 { slide.offset=0.; slide.outgoing=None; }
                    let width = area.width().max(1.);
                    if let Some(outgoing) = &slide.outgoing {
                        self.paint_view(ui,area,&cameras,outgoing,(slide.offset-slide.dir)*width+slide.drag,false,3);
                    } else if slide.drag.abs()>1. {
                        let neighbour = if slide.drag<0. { Some(window.page+1) } else { window.page.checked_sub(1) };
                        if let Some(page)=neighbour.filter(|p| *p<self.scheduler.page_count()) {
                            self.paint_view(ui,area,&cameras,&self.wall_page_view(page),slide.drag-slide.drag.signum()*width,false,3);
                        }
                    }
                    let actions = self.paint_view(ui,area,&cameras,&view,slide.offset*width+slide.drag,true,3);
                    self.apply_wall_camera_actions(&actions);
                    if self.wall_pinned {
                        if actions.drag_started { slide=Slide::default(); slide.dragging=true; }
                        if slide.dragging { slide.drag=(slide.drag+actions.drag_delta).clamp(-width,width); }
                        if actions.drag_stopped || (slide.dragging && !child.input(|i| i.pointer.primary_down())) {
                            slide.dragging=false;
                            let next=if slide.drag < -width*0.18 { Some(window.page+1) } else if slide.drag > width*0.18 { window.page.checked_sub(1) } else { None };
                            if let Some(page)=next.filter(|p| *p<self.scheduler.page_count()) {
                                let dir=if page>window.page {1.} else {-1.};
                                let offset=dir+slide.drag/width;
                                slide.start(dir,Some(view.clone())); slide.offset=offset; window.page=page; self.needs_sync=true;
                            }
                        }
                    } else { slide=Slide::default(); self.wall_edit_drag(ui,area,&view,&actions); }
                    if slide.is_animating() || slide.dragging { child.request_repaint(); ctx.request_repaint(); }

                });
            });
            if closed { self.wall_opened.remove(&window.id); self.extra_windows.retain(|w| w.id != window.id); self.needs_sync = true; }
            else {
                if old_preview != slide_neighbour(window.page, &slide, self.scheduler.page_count()) { self.needs_sync = true; }
                self.wall_slides.insert(window.id, slide);
            }
            if !closed { if let Some(w) = self.extra_windows.iter_mut().find(|w| w.id == window.id) { *w = window; } }
        }
        self.persist_wall_windows();
        if !self.extra_windows.is_empty() { ctx.request_repaint_after(LIVE_REPAINT); }
    }
}

// egui pointer coordinates are viewport-local. On Windows mouse capture can
// keep events in the source viewport, so sample the actual cursor and reject
// windows covered by another native window. Other platforms use egui events.
fn wall_pointer(ui: &egui::Ui) -> Option<egui::Pos2> {
    #[cfg(windows)]
    if let Some(rect) = ui.input(|i| i.viewport().inner_rect) {
        let scale = ui.ctx().pixels_per_point();
        return native_wall::cursor_in(rect, scale);
    }
    ui.input(|i| i.pointer.hover_pos())
}
#[cfg(windows)]
mod native_wall {
    use egui::{Pos2, Rect};
    #[repr(C)] #[derive(Clone, Copy, Default)] struct Point { x:i32,y:i32 }
    #[repr(C)] #[derive(Default)] struct NativeRect { left:i32,top:i32,right:i32,bottom:i32 }
    #[link(name="user32")]
    unsafe extern "system" {
        fn GetCursorPos(point:*mut Point)->i32;
        fn GetAsyncKeyState(key:i32)->i16;
        fn WindowFromPoint(point:Point)->isize;
        fn GetAncestor(window:isize,flags:u32)->isize;
        fn GetWindowRect(window:isize,rect:*mut NativeRect)->i32;
        fn MonitorFromRect(rect:*const NativeRect,flags:u32)->isize;
    }
    pub fn primary_down()->bool { unsafe { GetAsyncKeyState(1)<0 } }
    pub fn cursor_in(rect:Rect,scale:f32)->Option<Pos2> {
        let mut p=Point::default(); let mut outer=NativeRect::default();
        unsafe {
            if GetCursorPos(&mut p)==0 { return None; }
            let hwnd=GetAncestor(WindowFromPoint(p),2);
            if hwnd==0 || GetWindowRect(hwnd,&mut outer)==0 { return None; }
        }
        let global=egui::pos2(p.x as f32/scale,p.y as f32/scale);
        let native=Rect::from_min_max(egui::pos2(outer.left as f32/scale,outer.top as f32/scale),egui::pos2(outer.right as f32/scale,outer.bottom as f32/scale));
        if !rect.contains(global) || !native.expand(2.).contains_rect(rect) { return None; }
        Some(egui::pos2(global.x-rect.min.x,global.y-rect.min.y))
    }
    pub fn monitor_missing(rect:Rect,scale:f32)->bool {
        let r=NativeRect {left:(rect.min.x*scale) as i32,top:(rect.min.y*scale) as i32,right:(rect.max.x*scale) as i32,bottom:(rect.max.y*scale) as i32};
        unsafe { MonitorFromRect(&r,0)==0 }
    }
}

// Keep the adjacent stream alive only while a transition needs it.
pub(super) fn slide_neighbour(page: usize, slide: &Slide, count: usize) -> Option<usize> {
    let direction = if slide.outgoing.is_some() { -slide.dir } else { -slide.drag };
    let candidate = if direction > 0. { page.checked_add(1) } else if direction < 0. { page.checked_sub(1) } else { None };
    candidate.filter(|p| *p < count)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn slide_preview_respects_page_bounds() {
        let mut slide=Slide::default();
        assert_eq!(slide_neighbour(1,&slide,3),None);
        slide.drag=-100.;
        assert_eq!(slide_neighbour(1,&slide,3),Some(2));
        assert_eq!(slide_neighbour(2,&slide,3),None);
        slide.drag=100.;
        assert_eq!(slide_neighbour(1,&slide,3),Some(0));
        assert_eq!(slide_neighbour(0,&slide,3),None);
    }
}
