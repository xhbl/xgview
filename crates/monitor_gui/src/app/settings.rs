//! The settings panel, and the tabs that are about neither cameras nor sound:
//! display, language, blackout, system and about.

use super::*;

impl XgViewApp {
    /// Opens a link the way the desktop does.
    ///
    /// egui would hand the URL to eframe, which opens it through the
    /// `webbrowser` crate - and on Windows that crate always runs the *default
    /// browser*, passing the URL to it as an argument. A `mailto:` therefore
    /// reaches a browser tab instead of the mail client, because it never meets
    /// the shell that resolves a protocol to its handler. The shell is asked
    /// here instead: `ShellExecuteW` on Windows, and the desktop's opener
    /// elsewhere.
    ///
    /// Whatever answers opens a window of its own - a browser, the mail client -
    /// and a wall held on top would be sitting in front of it, so the wall is
    /// taken off the top while it happens and put back on a clock. The clock is
    /// what says when: nothing reports that another program's window has
    /// appeared.
    pub(super) fn open_link(&mut self, ctx: &egui::Context, url: &str) {
        self.set_on_top(ctx, OnTop::SuspendedUntil(Instant::now() + LINK_TAKEOVER));
        #[cfg(windows)]
        {
            #[link(name = "shell32")]
            extern "system" {
                fn ShellExecuteW(
                    hwnd: *mut core::ffi::c_void,
                    operation: *const u16,
                    file: *const u16,
                    parameters: *const u16,
                    directory: *const u16,
                    show: i32,
                ) -> *mut core::ffi::c_void;
            }
            fn wide(text: &str) -> Vec<u16> {
                use std::os::windows::ffi::OsStrExt;
                std::ffi::OsStr::new(text).encode_wide().chain(std::iter::once(0)).collect()
            }
            const SW_SHOWNORMAL: i32 = 1;
            let operation = wide("open");
            let file = wide(url);
            // Safety: two NUL terminated wide strings that outlive the call,
            // and null for the rest; the return value is only compared.
            let opened = unsafe {
                ShellExecuteW(
                    std::ptr::null_mut(),
                    operation.as_ptr(),
                    file.as_ptr(),
                    std::ptr::null(),
                    std::ptr::null(),
                    SW_SHOWNORMAL,
                )
            };
            // `ShellExecuteW` returns a value greater than 32 on success.
            if opened as isize > 32 {
                return;
            }
        }
        #[cfg(target_os = "macos")]
        {
            let _ = std::process::Command::new("open").arg(url).spawn();
            return;
        }
        #[cfg(all(unix, not(target_os = "macos"), not(target_os = "android")))]
        {
            let _ = std::process::Command::new("xdg-open").arg(url).spawn();
            return;
        }
        // Android has no shell to ask. egui's own opener is compiled out on this
        // target - eframe is built without its `webbrowser` feature - so the
        // activity is asked to open it with an Intent instead, which is what lets
        // a `mailto:` reach the mail app.
        #[cfg(target_os = "android")]
        crate::android::open_uri(url);
    }

    /// The settings side panel: a tab strip that stays put, and the tab's body
    /// scrolling underneath it.
    pub(super) fn settings_panel(&mut self, ui: &mut egui::Ui) {
        // The tab strip is a row of the navigation layer: Left and Right move
        // between the tabs and select as they land - the panel follows the
        // focus the way it follows a click - and Down drops into the body.
        ui.horizontal_wrapped(|ui| {
            let mut ids = [Id::NULL; SettingsTab::COUNT];
            let mut focused = None;
            self.nav.open("settings-tabs");
            for (slot, tab) in SettingsTab::ALL.into_iter().enumerate() {
                let selected = self.settings_tab == tab;
                let label = match tab {
                    SettingsTab::Cameras => monitor_i18n::tr_args(
                        "settings-tab-cameras-count",
                        &[("count", self.config.group_camera_count(self.config.active_group).into())],
                    ),
                    other => monitor_i18n::tr(other.label()),
                };
                let response = self.nav.tracked(ui.selectable_label(selected, label));
                self.name(&response, tab.label());
                if response.clicked() {
                    self.settings_tab = tab;
                    // A press takes the focus away from whatever had it and
                    // never gives it to what was pressed, so the tab just
                    // chosen takes it here and now.
                    response.request_focus();
                }
                if response.has_focus() {
                    focused = Some(tab);
                }
                ids[slot] = response.id;
            }
            // An arrow lands on a tab without a press; the panel has to follow
            // it, or the body would keep showing the old tab under the focus.
            if let Some(tab) = focused {
                self.settings_tab = tab;
            }
            // The tab in use is where an Up out of the body comes back to.
            let slot = SettingsTab::ALL.iter().position(|tab| *tab == self.settings_tab).unwrap_or(0);
            self.settings_anchor = Some(ids[slot]);
            self.nav.set_entry("settings-tabs", ids[slot]);
            self.nav.close();
        });
        ui.separator();

        // The body is one column of the navigation layer. See the "Add
        // devices" window for why sideways is spent inside a body.
        self.nav.open("settings-body");
        let mut asked = None;
        let output = egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                // The scroll a direction press asked for when it had nowhere to
                // move the focus: it is spent here because this is the only
                // place egui reads a scroll request from - see `body_scroll`.
                if let Some(points) = self.body_scroll.take() {
                    // Negative is downwards, the sign a wheel delta carries:
                    // egui inverts the value again before adding it to the
                    // offset, so a positive one here would scroll up and be
                    // clamped away at the top. See the test in `crate::nav`.
                    ui.scroll_with_delta(egui::vec2(0.0, -points));
                    asked = Some(points);
                }
                match self.settings_tab {
                    SettingsTab::Display => self.settings_display(ui),
                    SettingsTab::Cameras => self.settings_cameras(ui),
                    SettingsTab::Streams => self.settings_streams(ui),
                    SettingsTab::System => self.settings_system(ui),
                    SettingsTab::Blackout => self.settings_blackout(ui),
                    SettingsTab::About => {
                        self.settings_about(ui);
                        false
                    }
                }
            });
        if let Some(points) = asked {
            tracing::debug!(
                target: "xgview::gui",
                points,
                offset = output.state.offset.y,
                content = output.content_size.y,
                view = output.inner_rect.height(),
                "the remote scrolled the settings body"
            );
        }
        let dirty = output.inner;
        self.nav.close();
        if dirty {
            self.mark_dirty();
        }
    }

    /// The wall itself: the grid, what the tiles show, and how the window opens.
    pub(super) fn settings_display(&mut self, ui: &mut egui::Ui) -> bool {
        let mut dirty = false;


        ui.label(RichText::new(monitor_i18n::tr("settings-grid")).strong());
        ui.horizontal_wrapped(|ui| {
            for layout in GridLayout::STANDARD {
                let selected = self.scheduler.layout() == layout;
                let response = self.nav.tracked(ui.selectable_label(selected, layout.label()));
                self.name(&response, layout_hint(layout));
                if response.clicked() {
                    self.set_layout(layout);
                }
            }
            let current = self.scheduler.layout();
            let active = !current.is_standard();
            let more = self
                .nav
                .tracked(ui.selectable_label(active || self.layout_picker.is_open(), current.numeric_label().unwrap_or("…")));
            self.name(&more, "toolbar-layout-more");
            if more.clicked() {
                if self.layout_picker.is_open() {
                    self.layout_picker.close();
                } else {
                    self.layout_picker.open(());
                }
                // Only for a press that came from the keyboard or a remote: a
                // pointer does not move the focus anywhere else in this program,
                // so it must not appear to move it here either.
                self.layout_picker.from = (self.layout_picker.is_open()
                    && !more.clicked_by(egui::PointerButton::Primary))
                .then_some(more.id);
            }
        });
        ui.add_space(theme::space::S);
        ui.label(RichText::new(monitor_i18n::tr("settings-osd")).strong());
        ui.label(
            RichText::new(monitor_i18n::tr("settings-osd-hint"))
                .small()
                .color(theme::TEXT_DIM),
        );
        let corners: [(&str, &mut OsdItem); 4] = [
            ("corner-top-left", &mut self.config.osd.top_left),
            ("corner-top-right", &mut self.config.osd.top_right),
            ("corner-bottom-left", &mut self.config.osd.bottom_left),
            ("corner-bottom-right", &mut self.config.osd.bottom_right),
        ];
        let cycle_opts = OsdItem::cycle_options(&self.config.combined_info);
        for (corner, item) in corners {
            ui.horizontal(|ui| {
                ui.label(monitor_i18n::tr(corner));
                let button = controls::cycle_button(
                    &mut self.nav,
                    ui,
                    &cycle_opts,
                    item,
                    |choice| monitor_i18n::tr(choice.label()),
                );
                self.focus_names.insert(button.id, corner);
                if button.changed() {
                    dirty = true;
                }
            });
        }
        ui.add_space(theme::space::S);
        ui.horizontal(|ui| {
            ui.label(monitor_i18n::tr("settings-combined-info"));
            for slot in 0..4 {
                let label = monitor_i18n::tr_args(
                    "settings-combined-info-slot",
                    &[("n", (slot + 1).to_string().into())],
                );
                let response = self.nav.tracked(ui.button(label));
                self.name(&response, "combined-info-button");
                if response.clicked() {
                    self.combined_info_picker.open(());
                    self.combined_info_picker_slot = slot;
                    self.combined_info_picker.from =
                        (!response.clicked_by(egui::PointerButton::Primary)).then_some(response.id);
                    ui.ctx().data_mut(|d| {
                        d.insert_temp(Id::new("xgview-combined-info-focus"), (0usize, 0usize))
                    });
                }
            }
        });
        ui.add_space(theme::space::S);
        let mut fullscreen = self.fullscreen;
        let full = self.nav.tracked(ui.checkbox(&mut fullscreen, monitor_i18n::tr("settings-fullscreen")));
        if full.changed() {
            self.set_fullscreen(ui.ctx(), fullscreen);
        }
        let mut start_fullscreen = self.config.start_fullscreen;
        let start = self.nav.tracked(ui.checkbox(&mut start_fullscreen, monitor_i18n::tr("settings-start-fullscreen")));
        if start.changed() {
            self.config.start_fullscreen = start_fullscreen;
            dirty = true;
        }
        // Windows, macOS and X11 can hold a window above the others; a Wayland
        // session and Android cannot. There it is greyed out rather than left to
        // be a switch that does nothing, the way `monitor_core::power` treats the
        // switches it cannot honour. See `crate::window`.
        let on_top_supported = window::supported();
        let mut on_top = self.config.always_on_top_fullscreen;
        ui.add_enabled_ui(on_top_supported, |ui| {
            let on_top_check = self.nav.tracked(ui.checkbox(&mut on_top, monitor_i18n::tr("settings-on-top")));
            if on_top_check.changed() {
                self.config.always_on_top_fullscreen = on_top;
                self.apply_window_level(ui.ctx());
                dirty = true;
            }
        });
        if !on_top_supported {
            ui.label(RichText::new(monitor_i18n::tr("settings-on-top-unsupported")).small().color(theme::TEXT_DIM));
        }

        // Android only: whether the navigation bar keeps its strip (the wall is
        // narrower) or is hidden with the status bar (immersive). The activity is
        // told from the frame's layout; see `update`.
        #[cfg(target_os = "android")]
        {
            let mut reserve = self.config.reserve_navigation_bar;
            let response = self.nav.tracked(ui.checkbox(&mut reserve, monitor_i18n::tr("settings-reserve-navigation-bar")));
            if response.changed() {
                self.config.reserve_navigation_bar = reserve;
                dirty = true;
            }
            ui.label(
                RichText::new(monitor_i18n::tr("settings-reserve-navigation-bar-hint"))
                    .small()
                    .color(theme::TEXT_DIM),
            );
        }

        ui.add_space(theme::space::S);
        let idle = controls::slider(
            &mut self.nav,
            ui,
            &mut self.config.chrome_idle_secs,
            0.0..=10.0,
            0.5,
            "s",
            &monitor_i18n::tr("settings-chrome-idle"),
            3.5,
        );
        if idle.changed() {
            dirty = true;
        }
        ui.label(
            RichText::new(monitor_i18n::tr("settings-chrome-idle-hint"))
                .small()
                .color(theme::TEXT_DIM),
        );

        ui.add_space(theme::space::S);
        if self.settings_language(ui) {
            dirty = true;
        }

        dirty
    }

    /// The interface language picker.
    ///
    /// Switching it takes effect at once: the catalogue is swapped, the fonts
    /// are reinstalled for the language's own glyphs, and a repaint draws the
    /// new text. The choice is written back to the configuration like any other
    /// setting, so the next start comes up in it.
    pub(super) fn settings_language(&mut self, ui: &mut egui::Ui) -> bool {
        let mut dirty = false;
        ui.label(RichText::new(monitor_i18n::tr("settings-language")).strong());
        ui.label(
            RichText::new(monitor_i18n::tr("settings-language-hint"))
                .small()
                .color(theme::TEXT_DIM),
        );

        // The languages are one cycling button, drawn by `controls::cycle_button`
        // - the same helper the OSD corners below use: the language in force is
        // shown, and a Left / Right press - or Enter, as on those buttons -
        // steps to the next one. The whole list never has to fit on the panel,
        // and the remote needs no other gesture to reach it.
        let chosen = monitor_i18n::current_language();
        let available = monitor_i18n::available();
        let is_auto = self.config.language.is_empty() || self.config.language == "auto";
        // `None` is auto, then a language by id in the order the catalogue
        // lists them - English first.
        let mut choices: Vec<Option<String>> = Vec::with_capacity(available.len() + 1);
        choices.push(None);
        choices.extend(available.iter().map(|info| Some(info.id.clone())));
        let mut choice = if is_auto { None } else { Some(chosen) };
        let auto_label = monitor_i18n::tr("settings-language-auto");
        let button = controls::cycle_button(&mut self.nav, ui, &choices, &mut choice, |choice| {
            match choice {
                None => auto_label.clone(),
                Some(id) => available
                    .iter()
                    .find(|info| info.id == *id)
                    .map(|info| info.name.clone())
                    .unwrap_or_else(|| id.clone()),
            }
        });
        self.focus_names.insert(button.id, "settings-language");
        if button.changed() {
            if let Some(id) = choice {
                // Always record the explicit choice so the user leaves auto
                // mode even when the language is the same as the one
                // auto-detection picked.
                let switched = monitor_i18n::set_language(&id);
                self.config.language = id.clone();
                if switched {
                    fonts::install_for(ui.ctx(), &id);
                }
            } else {
                self.config.language = "auto".to_string();
                monitor_i18n::set_language("auto");
                fonts::install_for(ui.ctx(), &monitor_i18n::current_language());
            }
            ui.ctx().request_repaint();
            dirty = true;
        }

        dirty
    }

    /// The hours the wall blacks itself out, and what that does.
    pub(super) fn settings_blackout(&mut self, ui: &mut egui::Ui) -> bool {
        // The tab strip already says which tab this is, so the body opens with
        // its hint - small and dim, as the other tabs open.
        ui.label(
            RichText::new(monitor_i18n::tr("settings-blackout-hint"))
                .small()
                .color(theme::TEXT_DIM),
        );
        ui.add_space(theme::space::M);

        // The navigation layer and the schedule are taken as locals for the
        // whole body: its rows are drawn inside layout closures, and a closure
        // cannot borrow `self` while the rows are walking it.
        let nav = &mut self.nav;
        let blackout = &mut self.config.blackout;
        let enabled = blackout.enabled;
        let mut dirty = false;

        // The switch comes first because everything below it is inert while it
        // is off, and the rows are greyed rather than hidden so that what is
        // being switched off can still be read. It carries its own label, as
        // every other standalone setting does - a bare box under a heading left
        // it unclear what it switched.
        let switch = ui.checkbox(&mut blackout.enabled, monitor_i18n::tr("settings-blackout-on"));
        nav.item(&switch);
        dirty |= switch.changed();

        // The wait after an interruption. Zero is a real choice - the
        // interruption then lasts the period out - so it is a slider value
        // rather than something hidden behind a checkbox.
        let resume = controls::slider(
            nav,
            ui,
            &mut blackout.resume_after_minutes,
            0..=60,
            1.0,
            &monitor_i18n::tr("settings-blackout-minutes"),
            &monitor_i18n::tr("settings-blackout-resume"),
            1,
        );
        dirty |= resume.changed();
        // Shown whether or not the wait is zero: what zero means has to be
        // readable at the moment it is chosen.
        ui.label(
            RichText::new(monitor_i18n::tr("settings-blackout-resume-hint"))
                .small()
                .color(theme::TEXT_DIM),
        );
        ui.add_space(theme::space::M);

        // Which period is in force, or why none can be: a viewer should be able
        // to tell a wall that is about to blank from one that will not.
        let status = if !blackout.enabled {
            monitor_i18n::tr("settings-blackout-off")
        } else if let Some(window) = monitor_core::blackout::current(blackout) {
            monitor_i18n::tr_args(
                "settings-blackout-active",
                &[("until", monitor_core::blackout::format_clock(window.end).into())],
            )
        } else if blackout.is_empty() {
            monitor_i18n::tr("settings-blackout-empty")
        } else {
            monitor_i18n::tr("settings-blackout-idle")
        };
        ui.label(RichText::new(status).small().color(theme::TEXT_DIM));
        ui.add_space(theme::space::M);

        // Borrowed only now, so that the status line above could read the whole
        // schedule: a mutable borrow of the entries would have excluded it.
        let entries = &mut blackout.entries;
        let mut remove = None;
        ui.add_enabled_ui(enabled, |ui| {
            for (index, window) in entries.iter_mut().enumerate() {
                ui.horizontal_wrapped(|ui| {
                    let switch = ui.checkbox(&mut window.enabled, "");
                    nav.item(&switch);
                    dirty |= switch.changed();
                    // Last, because it takes the response with it.
                    switch.on_hover_text(monitor_i18n::tr("settings-blackout-enabled"));

                    ui.label(monitor_i18n::tr("settings-blackout-days"));
                    for day in 0..7u8 {
                        let on = window.days & (1 << day) != 0;
                        let label = monitor_i18n::tr(crate::blackout::DAY_KEYS[usize::from(day)]);
                        let toggle = ui.selectable_label(on, label);
                        nav.item(&toggle);
                        if toggle.clicked() {
                            window.days ^= 1 << day;
                            dirty = true;
                        }
                    }
                });
                ui.horizontal_wrapped(|ui| {
                    dirty |= clock_field(nav, ui, &mut window.start);
                    ui.label(monitor_i18n::tr("settings-blackout-to"));
                    dirty |= clock_field(nav, ui, &mut window.end);
                    let trash = icons::button(
                        ui,
                        icons::Icon::Trash,
                        false,
                        &monitor_i18n::tr("settings-blackout-remove"),
                    );
                    nav.item(&trash);
                    if trash.clicked() {
                        remove = Some(index);
                    }
                });
                ui.add_space(8.0);
            }

            if let Some(index) = remove {
                entries.remove(index);
                dirty = true;
            }
            let add = ui.add_enabled(
                enabled,
                egui::Button::new(monitor_i18n::tr("settings-blackout-add")),
            );
            nav.item(&add);
            if add.clicked() {
                entries.push(monitor_core::config::BlackoutWindow::default());
                dirty = true;
            }
        });

        if dirty {
            // The blank is worked out from the configuration on every frame, so
            // there is nothing to hand to anything: the next frame acts on this.
            self.needs_sync = true;
        }
        dirty
    }

    /// What the box does around the viewer: it starts it at boot, and it is the
    /// machine the decoder is chosen for.
    pub(super) fn settings_system(&mut self, ui: &mut egui::Ui) -> bool {
        // Only the desktop start-up section below changes anything on this tab.
        #[cfg(not(target_os = "android"))]
        let mut dirty = false;
        #[cfg(target_os = "android")]
        let mut dirty = false;

        ui.label(RichText::new(monitor_i18n::tr("settings-startup")).strong());
        // Desktop: the registration is ours to make, so it is a checkbox. On
        // Android it belongs to the manifest and the system, and the section
        // below shows the two things the device asks for instead.
        #[cfg(not(target_os = "android"))]
        {
            let supported = self.autostart.supported;
            let mut enabled = self.config.autostart;
            ui.add_enabled_ui(supported, |ui| {
                let boot = ui.checkbox(&mut enabled, monitor_i18n::tr("settings-autostart"));
                self.nav.item(&boot);
                if boot.changed() {
                    match autostart::set_enabled(enabled) {
                        Ok(()) => {
                            self.config.autostart = enabled;
                            self.autostart = autostart::status();
                            dirty = true;
                            self.flash(
                                monitor_i18n::tr(if enabled { "toast-autostart-on" } else { "toast-autostart-off" }),
                                ToastKind::Info,
                            );
                        }
                        Err(err) => {
                            self.autostart = autostart::status();
                            self.flash(
                                monitor_i18n::tr_args(
                                    "toast-autostart-failed",
                                    &[("error", err.to_string().into())],
                                ),
                                ToastKind::Error,
                            );
                        }
                    }
                }
            });
            if !supported {
                ui.label(RichText::new(monitor_i18n::tr("settings-autostart-unsupported")).small().color(theme::TEXT_DIM));
            }
            ui.label(
                RichText::new(monitor_i18n::tr_args(
                    "settings-mechanism",
                    &[("name", monitor_i18n::tr(&self.autostart.mechanism).into())],
                ))
                .small()
                .color(theme::TEXT_DIM),
            );
            ui.label(RichText::new(&self.autostart.detail).small().color(theme::TEXT_DIM));

            ui.add_space(theme::space::S);
            // One window per configuration: a second launch of this
            // configuration asks the one that is up to come forward instead of
            // opening another wall over it. It takes effect on the next launch,
            // like the start-up full screen above, and the lock follows the
            // configuration file rather than the installation - see
            // `crate::instance`.
            let mut one_instance = self.config.single_instance;
            let only = ui.checkbox(&mut one_instance, monitor_i18n::tr("settings-single-instance"));
            self.nav.item(&only);
            if only.changed() {
                self.config.single_instance = one_instance;
                dirty = true;
            }
        }
        #[cfg(target_os = "android")]
        self.android_boot_section(ui);

        ui.add_space(theme::space::L);
        ui.label(RichText::new(monitor_i18n::tr("settings-power")).strong());
        // Two switches, one request: `monitor_core::power` normalises them, so
        // the line under them says what the pair actually asks the machine for
        // rather than leaving the viewer to work out why one tick moved both.
        let power_supported = self.power.supported;
        let mut prevent_sleep = self.config.prevent_sleep;
        let mut keep_screen_on = self.config.keep_screen_on;
        ui.add_enabled_ui(power_supported, |ui| {
            let sleep = ui.checkbox(&mut prevent_sleep, monitor_i18n::tr("settings-prevent-sleep"));
            self.nav.item(&sleep);
            if sleep.changed() {
                dirty |= self.apply_power(prevent_sleep, keep_screen_on);
            }
            let screen = ui.checkbox(&mut keep_screen_on, monitor_i18n::tr("settings-keep-screen-on"));
            self.nav.item(&screen);
            if screen.changed() {
                dirty |= self.apply_power(prevent_sleep, keep_screen_on);
            }
        });
        ui.label(RichText::new(monitor_i18n::tr("settings-power-hint")).small().color(theme::TEXT_DIM));
        if !power_supported {
            ui.label(RichText::new(monitor_i18n::tr("settings-power-unsupported")).small().color(theme::TEXT_DIM));
        }
        ui.label(
            RichText::new(monitor_i18n::tr_args(
                "settings-mechanism",
                &[("name", monitor_i18n::tr(self.power.mechanism).into())],
            ))
            .small()
            .color(theme::TEXT_DIM),
        );
        if let Some(error) = &self.power.error {
            ui.label(RichText::new(error).small().color(theme::ERROR));
        }

        ui.add_space(theme::space::L);
        ui.label(RichText::new(monitor_i18n::tr("settings-keys")).strong());
        // One shortcut per row, the key in a monospace column of its own so the
        // list reads down the keys and not across a line of dots.
        egui::Grid::new("xgview-system-keys")
            .num_columns(2)
            .spacing([theme::space::L, theme::space::XS])
            .show(ui, |ui| {
                for (keys, action) in [
                    ("DPAD / arrows", "key-move-focus"),
                    ("Enter", "key-enter"),
                    ("Esc / Back", "key-back"),
                    ("Num (1-4)", "key-grid"),
                    ("PgUp / PgDn", "key-page"),
                    ("F1", "key-settings"),
                    ("F2", "key-add-devices"),
                    ("F11", "key-fullscreen"),
                ] {
                    ui.label(RichText::new(keys).monospace().strong());
                    ui.label(RichText::new(monitor_i18n::tr(action)).small().color(theme::TEXT_DIM));
                    ui.end_row();
                }
            });

        ui.add_space(theme::space::L);
        ui.label(RichText::new(monitor_i18n::tr("settings-actions")).strong());
        // Two actions that end the session. Exit leaves at once — a viewer that
        // pressed it has read the label. Reboot is behind a confirmation window:
        // a press beside a neighbour must not restart the machine on its own,
        // and it is drawn only where the process has the privilege to do it.
        let mut do_exit = false;
        let mut ask_reboot: Option<Option<Id>> = None;
        ui.horizontal(|ui| {
            let exit_label = monitor_i18n::tr("action-exit");
            let exit_btn = self.nav.tracked(ui.button(exit_label));
            if exit_btn.clicked() {
                do_exit = true;
            }
            if reboot::is_supported() {
                let reboot_label = monitor_i18n::tr("action-reboot");
                let reboot_btn = self.nav.tracked(ui.button(reboot_label));
                if reboot_btn.clicked() {
                    // Only for a press that came from the keyboard or a remote:
                    // a pointer does not move the focus anywhere else in this
                    // program, so it must not appear to move it here either.
                    ask_reboot = Some(
                        (!reboot_btn.clicked_by(egui::PointerButton::Primary))
                            .then_some(reboot_btn.id),
                    );
                }
            }
        });
        if do_exit {
            self.quit(ui.ctx());
        }
        if let Some(from) = ask_reboot {
            self.system_confirm.open(());
            self.system_confirm.from = from;
        }

        dirty
    }

    /// The Android start-on-boot section.
    ///
    /// Android starts nothing from the boot broadcast unless the system allows
    /// it, and there is nothing to toggle from here: the receiver is declared in
    /// the manifest. What the device asks for is one of two things, so both are
    /// shown with the screen that sets each, and the adb route is written out
    /// for a deployment that has a computer at hand.
    #[cfg(target_os = "android")]
    pub(super) fn android_boot_section(&mut self, ui: &mut egui::Ui) {
        // The two states only change while the viewer is away in the system
        // settings, so reading them twice a second is enough and keeps the JNI
        // calls off every frame.
        if self.time - self.android_boot_at > 0.5 {
            self.android_boot = (crate::android::overlay_allowed(), crate::android::is_home_app());
            self.android_boot_at = self.time;
        }
        let (overlay, home) = self.android_boot;

        ui.label(
            RichText::new(monitor_i18n::tr("android-boot-hint"))
                .small()
                .color(theme::TEXT_DIM),
        );
        ui.add_space(theme::space::S);

        ui.horizontal(|ui| {
            let button = ui.button(monitor_i18n::tr("android-home-button"));
            self.nav.item(&button);
            self.name(&button, "android-home-name");
            if button.clicked() {
                crate::android::open_home_settings();
                self.android_boot_at = f64::NEG_INFINITY;
            }
            ui.label(
                RichText::new(monitor_i18n::tr(if home { "android-home-yes" } else { "android-home-no" }))
                    .small()
                    .color(if home { theme::LIVE } else { theme::TEXT_DIM }),
            );
        });
        ui.horizontal(|ui| {
            let button = ui.button(monitor_i18n::tr("android-overlay-button"));
            self.nav.item(&button);
            self.name(&button, "android-overlay-name");
            if button.clicked() {
                crate::android::open_overlay_settings();
                self.android_boot_at = f64::NEG_INFINITY;
            }
            ui.label(
                RichText::new(monitor_i18n::tr(if overlay { "android-overlay-yes" } else { "android-overlay-no" }))
                    .small()
                    .color(if overlay { theme::LIVE } else { theme::TEXT_DIM }),
            );
        });

        ui.add_space(theme::space::S);
        ui.label(
            RichText::new(monitor_i18n::tr_args("android-adb-hint", &[("nl", "\n".into())]))
            .small()
            .color(theme::TEXT_DIM),
        );
    }

    /// What the program is, where its configuration lives, and the export /
    /// import of that configuration.
    pub(super) fn settings_about(&mut self, ui: &mut egui::Ui) {
        // Version and authors come from the manifest, declared once in
        // `[workspace.package]` and inherited by every crate, so the about
        // panel cannot drift from Cargo.toml. The description is translated
        // (`about-description`), with the English pack matching the manifest.
        // The name is the display name, not the crate name (this crate is
        // `monitor_gui`).
        ui.label(monitor_i18n::tr_args(
            "about-title",
            &[
                ("app", monitor_core::APP_DISPLAY_NAME.into()),
                ("description", monitor_i18n::tr("about-description").into()),
            ],
        ));
        // The author is a mailto link, with the version and the copyright years
        // either side of it so the line still reads as one sentence. The years
        // are the ones the binary was built in; see `copyright_years`.
        let (author, email) = monitor_core::author();
        ui.horizontal(|ui| {
            ui.label(monitor_i18n::tr_args("about-version-by", &[("version", env!("CARGO_PKG_VERSION").into())]));
            if email.is_empty() {
                ui.label(author);
            } else {
                // `Ui::link` rather than `hyperlink_to`: the click is opened
                // through the shell here, so a `mailto:` reaches the mail
                // client. See `open_link`.
                let link = ui.link(format!("{author} <{email}>"));
                self.nav.item(&link);
                self.name(&link, "about-author-name");
                if link.clicked() {
                    self.open_link(ui.ctx(), &monitor_core::author_mailto());
                }
            }
            ui.label(monitor_i18n::tr_args("about-copyright", &[("years", monitor_core::copyright_years().into())]));
        });
        // Where the project lives, right under the version line. A link like the
        // author's, but a wrapping label rather than `Ui::link`: the settings
        // panel is narrow and the URL is longer than one line there.
        ui.label(monitor_i18n::tr("about-repository"));
        let repository = ui.add(
            egui::Label::new(RichText::new(REPOSITORY_URL).color(theme::ACCENT))
                .wrap()
                .sense(egui::Sense::click()),
        );
        let repository = self.nav.tracked(repository);
        self.name(&repository, "about-repository");
        if repository.clicked() {
            self.open_link(ui.ctx(), REPOSITORY_URL);
        }
        #[cfg(juanz)]
        {
            let mut link: Option<String> = None;
            let juanz = crate::juanz::button(ui, &mut self.nav, &mut |url| link = Some(url.to_owned()));
            self.name(&juanz, "about-donate");
            if let Some(url) = link {
                self.open_link(ui.ctx(), &url);
            }
        }
        let decoder_mode = monitor_i18n::tr(if self.hardware_decoder {
            "about-decoder-hardware"
        } else {
            "about-decoder-software"
        });
        ui.label(monitor_i18n::tr_args(
            "about-decoder",
            &[("backend", self.decoder.into()), ("mode", decoder_mode.into())],
        ));
        ui.label(monitor_i18n::tr_args(
            "about-config",
            &[("path", self.config_path.display().to_string().into())],
        ));

        // The export / import pair. The export is sealed with a passphrase by
        // default, which the prompt asks for before the file is written;
        // unticking the box beside it writes the JSON in clear instead. The
        // desktop opens a file dialog for each, while Android writes into the
        // public Download/xgview folder and opens the system's document picker,
        // and the hint says so.
        ui.add_space(theme::space::M);
        ui.horizontal(|ui| {
            let export = ui.button(monitor_i18n::tr("about-export"));
            self.nav.item(&export);
            self.name(&export, "about-export-name");
            if export.clicked() {
                // The passphrase first: nothing is written until it has been
                // typed twice, and the file dialog that follows is opened from
                // the prompt. With the box unticked there is none to ask for,
                // and the dialog opens straight away.
                //
                // The button is remembered here, because by the time that dialog
                // is up the focus has been through the prompt on the way.
                //
                // Only for a press that came from the keyboard or a remote: a
                // pointer does not move the focus anywhere else in this program,
                // so it must not appear to move it here either.
                if !export.clicked_by(egui::PointerButton::Primary) {
                    self.file_dialog_anchor = Some(export.id);
                }
                if self.encrypt_export {
                    self.passphrase = Some(Passphrase::for_export());
                } else {
                    self.ask_for_file(FileJob::Export(None));
                }
            }
            let encrypt = ui.checkbox(&mut self.encrypt_export, monitor_i18n::tr("about-encrypt-export"));
            self.nav.item(&encrypt);
            self.name(&encrypt, "about-encrypt-export-name");
        });
        ui.horizontal(|ui| {
            let import = ui.button(monitor_i18n::tr("about-import"));
            self.nav.item(&import);
            self.name(&import, "about-import-name");
            if import.clicked() {
                // A sealed document opens the passphrase prompt instead, which
                // is drawn on the wall. See `ask_for_file`.
                //
                // Remembered for a keyboard or remote press only, as the export
                // is: a pointer leaves the focus where it found it.
                if !import.clicked_by(egui::PointerButton::Primary) {
                    self.file_dialog_anchor = Some(import.id);
                }
                self.ask_for_file(FileJob::Import);
            }
            let only = ui.checkbox(&mut self.import_cameras_only, monitor_i18n::tr("about-only-cameras"));
            self.nav.item(&only);
            self.name(&only, "about-only-cameras-name");
        });
        ui.label(RichText::new(self.import_hint()).small().color(theme::TEXT_DIM));

        // The machine itself, under everything else the page says - the one
        // place a viewer looks to see what this installation is running on. The
        // numbers are re-read at most once a second and only while the page is
        // open; the page asks for the repaint that keeps them moving.
        ui.add_space(theme::space::M);
        ui.separator();
        self.vitals.refresh(Duration::from_secs(1));
        ui.ctx().request_repaint_after(Duration::from_millis(500));
        match self.vitals.memory_percent() {
            Some(percent) if self.vitals.total_memory() > 0 => {
                let megabytes = |bytes: u64| (bytes / (1024 * 1024)) as usize;
                ui.label(monitor_i18n::tr_args(
                    "about-memory",
                    &[
                        ("used", megabytes(self.vitals.used_memory()).into()),
                        ("total", megabytes(self.vitals.total_memory()).into()),
                        ("percent", (percent as usize).into()),
                    ],
                ));
            }
            _ => {
                ui.label(monitor_i18n::tr("about-memory-unknown"));
            }
        }
        // Android does not let an application read the processor counters, so
        // the line is left out there rather than drawn as an unknown.
        #[cfg(not(target_os = "android"))]
        match self.vitals.cpu_percent() {
            Some(percent) => {
                ui.label(monitor_i18n::tr_args(
                    "about-cpu",
                    &[("percent", (percent.round() as usize).into())],
                ));
            }
            None => {
                ui.label(monitor_i18n::tr("about-cpu-unknown"));
            }
        }
        ui.label(monitor_i18n::tr_args("about-system", &[("os", self.vitals.os().into())]));
        ui.label(monitor_i18n::tr_args(
            "about-uptime",
            &[("duration", uptime_text(self.started.elapsed()).into())],
        ));
    }
}
