use super::{Action, SettingsPage, UiState};
use crate::config::{Effect, GpuPowerPreference, Material, PanelMaterial, Settings, ShellProfile};
use egui::{Color32, ComboBox, Frame, RichText, Slider, Stroke, TextEdit, Ui};
use session_manager::ShellIntegration;

impl UiState {
    pub(super) fn draw_settings_page(
        &mut self,
        ui: &mut Ui,
        page: SettingsPage,
        fonts: &[String],
        actions: &mut Vec<Action>,
    ) {
        ui.spacing_mut().item_spacing.y = 9.0;
        ui.spacing_mut().slider_width = (ui.available_width() * 0.3).clamp(80.0, 156.0);
        match page {
            SettingsPage::Terminal => self.terminal_settings(ui, fonts, actions),
            SettingsPage::Appearance => self.appearance_settings(ui),
            SettingsPage::System => self.system_settings(ui, actions),
        }
    }

    fn terminal_settings(&mut self, ui: &mut Ui, fonts: &[String], actions: &mut Vec<Action>) {
        group(ui, "Shell", |ui| {
            #[cfg(target_os = "android")]
            let bundled_arch = crate::android_arch::bundled();
            #[cfg(target_os = "android")]
            let default_shell = if bundled_arch {
                "Arch Linux (default)"
            } else {
                "System default"
            };
            #[cfg(not(target_os = "android"))]
            let default_shell = "System default";
            #[cfg(target_os = "android")]
            let aether_available = crate::android_arch::aether_shell().is_ok()
                && crate::android_arch::system_bridge().is_ok();
            #[cfg(not(target_os = "android"))]
            let aether_available = true;

            let shell = self.settings.shell.program.as_deref();
            let current_shell = match shell {
                None => default_shell,
                Some("@aether") if !aether_available => "Aether Shell (unavailable)",
                Some("@aether") => "Aether Shell",
                #[cfg(target_os = "android")]
                Some("/system/bin/sh") => "Android Shell",
                Some(_) => "Custom executable",
            };
            let mut selection = None;
            row(ui, "New sessions", |ui| {
                selector(ui, "settings_shell", current_shell).show_ui(ui, |ui| {
                    if ui
                        .selectable_label(shell.is_none(), default_shell)
                        .clicked()
                    {
                        selection = Some(None);
                    }
                    #[cfg(target_os = "android")]
                    if bundled_arch
                        && ui
                            .selectable_label(shell == Some("/system/bin/sh"), "Android Shell")
                            .clicked()
                    {
                        selection = Some(Some("/system/bin/sh"));
                    }
                    if aether_available
                        && ui
                            .selectable_label(shell == Some("@aether"), "Aether Shell")
                            .clicked()
                    {
                        selection = Some(Some("@aether"));
                    }
                });
            });
            if let Some(program) = selection {
                self.apply_shell_program(program.map(str::to_owned));
            }
            if self.settings.shell.program.as_deref() == Some("@aether") {
                if let Err(error) = self.settings.shell.resolved(None, None) {
                    ui.colored_label(Color32::LIGHT_RED, error.to_string());
                }
                let mut native = self.settings.shell.native_presentation.unwrap_or(true);
                if ui
                    .checkbox(&mut native, "Native candidates and structured previews")
                    .changed()
                {
                    self.settings.shell.native_presentation = Some(native);
                    self.settings_dirty = true;
                }
            }
            let custom_profile = self
                .settings
                .shell
                .program
                .as_deref()
                .is_some_and(|program| {
                    program != "@aether"
                        && !(cfg!(target_os = "android") && program == "/system/bin/sh")
                });
            egui::CollapsingHeader::new("Custom executable")
                .default_open(custom_profile)
                .show(ui, |ui| {
                    let use_custom = ui
                        .horizontal_wrapped(|ui| {
                            let width = match ui.available_width() {
                                width if width >= 420.0 => width - 108.0,
                                width => width,
                            };
                            ui.add(
                                TextEdit::singleline(&mut self.shell_program_editor)
                                    .hint_text("Absolute executable path")
                                    .desired_width(width),
                            );
                            ui.button("Use custom Shell").clicked()
                        })
                        .inner;
                    if use_custom {
                        let candidate = ShellProfile {
                            program: Some(self.shell_program_editor.clone()),
                            ..Default::default()
                        };
                        match candidate.validate() {
                            Ok(()) => self.apply_shell_program(candidate.program),
                            Err(error) => self.notice = Some(error.to_string()),
                        }
                    }
                });
            #[cfg(target_os = "android")]
            if crate::android_arch::available() && ui.button("Open Arch Linux").clicked() {
                actions.push(Action::ArchLinux);
            }
            #[cfg(not(target_os = "android"))]
            let _ = actions;
        });
        group(ui, "Typography", |ui| {
            row(ui, "Font", |ui| {
                selector(ui, "settings_font", &self.settings.font_family).show_ui(ui, |ui| {
                    for font in fonts {
                        self.settings_dirty |= ui
                            .selectable_value(&mut self.settings.font_family, font.clone(), font)
                            .changed();
                    }
                });
            });
            row(ui, "Font size", |ui| {
                self.settings_dirty |= ui
                    .add(Slider::new(&mut self.settings.font_size, 10.0..=28.0).suffix(" pt"))
                    .changed();
            });
        });
    }

    fn apply_shell_program(&mut self, program: Option<String>) {
        let integration = if program.as_deref() == Some("@aether") {
            ShellIntegration::AetherV1
        } else {
            ShellIntegration::None
        };
        self.settings.shell.login = program.as_ref().map(|_| false);
        self.settings.shell.program = program;
        self.settings.shell.integration = integration;
        self.settings.shell.args.clear();
        self.settings_dirty = true;
    }

    fn appearance_settings(&mut self, ui: &mut Ui) {
        group(ui, "Window", |ui| {
            row(ui, "Glass effect", |ui| {
                for (effect, label) in [
                    (Effect::Off, "Off"),
                    (Effect::Balanced, "Balanced"),
                    (Effect::Full, "Full"),
                ] {
                    self.settings_dirty |= ui
                        .selectable_value(&mut self.settings.effect, effect, label)
                        .changed();
                }
            });
            row(ui, "Material", |ui| {
                let labels = [
                    (Material::LiquidClear, "Liquid Clear"),
                    (Material::LiquidRegular, "Liquid Regular"),
                    (Material::Hud, "HUD"),
                    (Material::Sidebar, "Sidebar"),
                    (Material::UnderWindowBackground, "Under Window Background"),
                ];
                selector(
                    ui,
                    "settings_material",
                    label_for(&labels, self.settings.material),
                )
                .show_ui(ui, |ui| {
                    for (material, label) in labels {
                        self.settings_dirty |= ui
                            .selectable_value(&mut self.settings.material, material, label)
                            .changed();
                    }
                });
            });
            row(ui, "Background opacity", |ui| {
                self.settings_dirty |= opacity_slider(ui, &mut self.settings.opacity, 0.0);
            });
            ui.small("Higher opacity makes background content less visible.");
            #[cfg(windows)]
            {
                let parameters = &mut self.settings.appearance_ui.material_parameters;
                let mut blur = parameters.backdrop_blur_radius_px > 0.0;
                if ui
                    .checkbox(&mut blur, "Acrylic background blur")
                    .on_hover_text("Windows sets the blur strength.")
                    .changed()
                {
                    parameters.backdrop_blur_radius_px = if blur { 24.0 } else { 0.0 };
                    self.settings_dirty = true;
                }
            }
            row(ui, "Window corners", |ui| {
                self.settings_dirty |= ui
                    .add(
                        Slider::new(
                            &mut self.settings.appearance_ui.window_corner_radius,
                            0.0..=48.0,
                        )
                        .suffix(" pt"),
                    )
                    .changed();
            });
        });
        group(ui, "Panels", |ui| {
            let appearance = &mut self.settings.appearance_ui;
            row(ui, "Panel material", |ui| {
                let labels = [
                    (PanelMaterial::LiquidClear, "Liquid Clear"),
                    (PanelMaterial::LiquidRegular, "Liquid Regular"),
                ];
                selector(
                    ui,
                    "settings_panel_material",
                    label_for(&labels, appearance.panel_material),
                )
                .show_ui(ui, |ui| {
                    for (material, label) in labels {
                        self.settings_dirty |= ui
                            .selectable_value(&mut appearance.panel_material, material, label)
                            .changed();
                    }
                });
            });
            row(ui, "Panel opacity", |ui| {
                self.settings_dirty |= opacity_slider(ui, &mut appearance.panel_opacity, 0.2);
            });
            row(ui, "Panel corners", |ui| {
                self.settings_dirty |= ui
                    .add(Slider::new(&mut appearance.panel_corner_radius, 0..=48).suffix(" pt"))
                    .changed();
            });
        });
        group(ui, "Motion", |ui| {
            self.settings_dirty |= ui
                .checkbox(&mut self.settings.reduce_motion, "Reduce motion")
                .changed();
            let appearance = &mut self.settings.appearance_ui;
            self.settings_dirty |= ui
                .checkbox(&mut appearance.animations, "Interaction animations")
                .changed();
            if appearance.animations {
                row(ui, "Animation duration", |ui| {
                    self.settings_dirty |= ui
                        .add(
                            Slider::new(&mut appearance.animation_duration_ms, 0..=500)
                                .suffix(" ms"),
                        )
                        .changed();
                });
            }
            ui.small("System accessibility preferences take precedence.");
        });
    }

    fn system_settings(&mut self, ui: &mut Ui, actions: &mut Vec<Action>) {
        group(ui, "Graphics", |ui| {
            row(ui, "GPU power mode", |ui| {
                let labels = [
                    (GpuPowerPreference::Auto, "Auto"),
                    (GpuPowerPreference::LowPower, "Power saving"),
                    (GpuPowerPreference::HighPerformance, "High performance"),
                ];
                selector(
                    ui,
                    "settings_gpu",
                    label_for(&labels, self.settings.gpu_power_preference),
                )
                .show_ui(ui, |ui| {
                    for (preference, label) in labels {
                        self.settings_dirty |= ui
                            .selectable_value(
                                &mut self.settings.gpu_power_preference,
                                preference,
                                label,
                            )
                            .changed();
                    }
                });
            });
            ui.small("Restart Plyra Terminal to apply a new GPU mode.");
        });
        let config = Settings::path();
        group(ui, "Configuration", |ui| {
            ui.small("Edit JSON to set colors, Shell profiles, shortcuts and MCP servers.");
            show_path(ui, &config);
            ui.horizontal_wrapped(|ui| {
                if ui.button("Edit config.json").clicked() {
                    match self.settings.editor_text() {
                        Ok(text) => self.config_editor = Some(text),
                        Err(error) => self.notice = Some(error.to_string()),
                    }
                }
                if ui.button("Reload configuration").clicked() {
                    actions.push(Action::ReloadConfig);
                }
            });
            if let Some(text) = &mut self.config_editor {
                ui.add(
                    TextEdit::multiline(text)
                        .font(egui::TextStyle::Monospace)
                        .desired_width(f32::INFINITY)
                        .desired_rows(if cfg!(target_os = "android") { 8 } else { 15 })
                        .char_limit(crate::config::MAX_CONFIG_BYTES),
                );
                if ui.button("Validate, save and apply").clicked() {
                    actions.push(Action::SaveConfig(text.clone()));
                }
            }
        });
        group(ui, "Diagnostics", |ui| {
            show_path(ui, &config.with_file_name("diagnostics.json"));
            if ui.button("Save diagnostics").clicked() {
                actions.push(Action::SaveDiagnostics);
            }
        });
        group(ui, "Administrator access", |ui| {
            if ui
                .add(egui::Button::new("Revoke administrator authorization").wrap())
                .clicked()
            {
                self.system.revoke();
            }
        });
    }
}

fn group(ui: &mut Ui, title: &str, contents: impl FnOnce(&mut Ui)) {
    Frame::new()
        .inner_margin(egui::Margin::same(12))
        .corner_radius(ui.visuals().window_corner_radius.nw.min(12))
        .fill(Color32::from_rgb(40, 44, 46))
        .stroke(Stroke::new(1.0_f32, Color32::from_rgb(58, 64, 65)))
        .show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            ui.label(RichText::new(title).strong());
            ui.add_space(2.0);
            contents(ui);
        });
}

fn row(ui: &mut Ui, label: &str, control: impl FnOnce(&mut Ui)) {
    if ui.available_width() >= 390.0 {
        ui.horizontal(|ui| {
            ui.allocate_ui_with_layout(
                egui::vec2(155.0, ui.spacing().interact_size.y),
                egui::Layout::left_to_right(egui::Align::Center),
                |ui| {
                    ui.label(label);
                },
            );
            ui.horizontal_wrapped(control);
        });
    } else {
        ui.label(label);
        ui.horizontal_wrapped(control);
    }
}

fn selector(ui: &Ui, id: &str, selected: &str) -> ComboBox {
    ComboBox::from_id_salt(id)
        .selected_text(selected)
        .width(ui.available_width().min(202.0))
        .truncate()
}

fn label_for<T: PartialEq + Copy>(labels: &[(T, &'static str)], selected: T) -> &'static str {
    labels
        .iter()
        .find(|(value, _)| *value == selected)
        .map_or("", |(_, label)| *label)
}

fn opacity_slider(ui: &mut Ui, opacity: &mut f32, minimum: f32) -> bool {
    let mut percent = *opacity * 100.0;
    if ui
        .add(
            Slider::new(&mut percent, (minimum * 100.0)..=100.0)
                .fixed_decimals(0)
                .suffix("%"),
        )
        .changed()
    {
        *opacity = percent / 100.0;
        true
    } else {
        false
    }
}

fn show_path(ui: &mut Ui, path: &std::path::Path) {
    ui.add(
        egui::Label::new(
            RichText::new(path.display().to_string())
                .monospace()
                .small(),
        )
        .wrap(),
    );
}

// FVPROV1 17027 W3siYm9keSI6eyJhY3Rvcl9rZXlfaWQiOiI5ZjAwOTVhYmY4ODEwZGQwY2Y4NTdlZDBlNDI0ODg0NmRlMmFkMDc2ZmZhYTY2MWIzNmM5Yzc3YWFjOGVlOWMzIiwiYWxnb3JpdGhtIjoiRWQyNTUxOSIsImF1dGhvciI6IkoteC0gWiIsImNvbnRlbnRfc2hhMjU2IjoiMDc1MzQ4OTA4YjBiOTc2NWQwNDE1YzYyNzU4ZjIxNTRiOGRhNzBmYWVlNjEzOGRiMDU0YjA5ZWRkMmQ1YjVmMyIsImNvcHlfaWQiOiJ2MC4xLjAtYmV0YS4xIiwiY3JlYXRlZF9hdCI6IjIwMjYtMTEtMTgiLCJlZGl0aW9uIjoiYmV0YSIsImlzc3Vlcl9wb2xpY3kiOm51bGwsImtpbmQiOiJjcmVhdGUiLCJtb2RlIjoic2VsZl9zaWduIiwicGFyZW50X3ZlcnNpb24iOm51bGwsInByZXZpb3VzX3JlY29yZF9zaGEyNTYiOm51bGwsInNjaGVtYSI6ImZpbGV2ZWlsLnByb3ZlbmFuY2Uuc2lnbmF0dXJlLnYyIiwic2VxdWVuY2UiOjAsInRyYW5zZmVyX3RvX2tleV9pZCI6bnVsbCwidmVyc2lvbiI6InYwLjEuMC1iZXRhLjEiLCJ3b3JrX2lkIjoib3JnLnBseXJhLnRlcm1pbmFsIn0sInNpZ25hdHVyZXMiOlt7ImtleV9pZCI6IjlmMDA5NWFiZjg4MTBkZDBjZjg1N2VkMGU0MjQ4ODQ2ZGUyYWQwNzZmZmFhNjYxYjM2YzljNzdhYWM4ZWU5YzMiLCJwdWJsaWNfa2V5X2hleCI6ImMzZTkyZDhkZDNkZWM4NTliODdlOTY4MjIyYjI4OWJhNzg5M2NjOGM4NWZjOTE2MjI2YjZiNjI5NjFiOTU5ZDciLCJzaWduYXR1cmVfaGV4IjoiOGNjYmI5N2VlYzVkNTg1ZGE5MjE3NTU0ZjJjYzRmNzQ4NjAxODcxNjVlZTE2MDgyZGRjOTFmY2M4YjhjNGNmMTQ4MjJlZDZiNTZhMjBhMGMzMThjYTJkMGFkN2ExY2UwODhlYmIyYjI4ODExMDM1NTQ2M2JiMTRjYjI1N2M2MGMifV19XQ== FVPROV1-END
