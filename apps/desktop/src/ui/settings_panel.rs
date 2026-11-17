use egui::{Align, Button, Color32, CornerRadius, Frame, Layout, Margin, RichText, Stroke};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum SettingsPage {
    Terminal,
    Appearance,
    System,
}

impl SettingsPage {
    const ALL: [Self; 3] = [Self::Terminal, Self::Appearance, Self::System];

    fn label(self) -> &'static str {
        match self {
            Self::Terminal => "Terminal",
            Self::Appearance => "Appearance",
            Self::System => "Advanced",
        }
    }
}

const TEXT: Color32 = Color32::from_rgb(235, 239, 238);
const MUTED_TEXT: Color32 = Color32::from_rgb(174, 183, 181);
const ACCENT: Color32 = Color32::from_rgb(155, 225, 220);

pub(super) fn show(
    context: &egui::Context,
    page: &mut SettingsPage,
    content: impl FnOnce(&mut egui::Ui, SettingsPage),
) -> bool {
    let screen = context.screen_rect();
    let compact = screen.width() < 670.0;
    let padding = if compact { 14 } else { 22 };
    let outer_width = (screen.width() - if compact { 20.0 } else { 72.0 }).clamp(1.0, 744.0);
    let inner_width = (outer_width - 2.0 * f32::from(padding) - 2.0).max(1.0);
    let chrome_height = if compact { 156.0 } else { 111.0 };
    let body_limit = if compact { 500.0 } else { 470.0 };
    let body_height = (screen.height() - chrome_height).clamp(26.0, body_limit);
    let inherited = &context.style().visuals;
    let tint = inherited.window_fill.to_srgba_unmultiplied()[0];
    let alpha = 225 + (u16::from(inherited.window_fill.a()) * 30 / 255) as u8;
    let fill = Color32::from_rgba_unmultiplied(tint + 3, tint + 6, tint + 7, alpha);
    let frame = Frame::new()
        .inner_margin(Margin::same(padding))
        .fill(fill)
        .stroke(Stroke::new(1.0_f32, Color32::from_white_alpha(39)))
        .corner_radius(inherited.window_corner_radius)
        .shadow(inherited.window_shadow);

    let id = egui::Id::new("glass_settings");
    let response = egui::Modal::new(id)
        .area(egui::Modal::default_area(id).fade_in(false))
        .frame(frame)
        .backdrop_color(Color32::from_black_alpha(115))
        .show(context, |ui| {
            skin(ui, fill, compact);
            ui.set_width(inner_width);
            ui.set_max_height((screen.height() - 2.0 * f32::from(padding) - 24.0).max(26.0));
            let close = header(ui, compact);
            ui.add_space(6.0);
            if compact {
                ui.horizontal(|ui| {
                    let width = (ui.available_width() - 2.0 * ui.spacing().item_spacing.x) / 3.0;
                    for target in SettingsPage::ALL {
                        page_button(ui, page, target, width.max(1.0));
                    }
                });
                ui.add_space(6.0);
                ui.separator();
                scroll_page(ui, *page, body_height, content);
            } else {
                ui.separator();
                ui.horizontal_top(|ui| {
                    ui.vertical(|ui| {
                        ui.set_width(132.0);
                        for target in SettingsPage::ALL {
                            page_button(ui, page, target, 132.0);
                        }
                    });
                    ui.add_space(12.0);
                    ui.vertical(|ui| {
                        ui.set_width(ui.available_width());
                        scroll_page(ui, *page, body_height, content);
                    });
                });
            }
            close
        });
    response.inner || response.should_close()
}

fn skin(ui: &mut egui::Ui, fill: Color32, compact: bool) {
    let style = ui.style_mut();
    let control_height = if compact { 34.0 } else { 30.0 };
    style.spacing.interact_size.y = control_height;
    style.spacing.item_spacing = egui::vec2(6.0, 8.0);
    style.spacing.button_padding = egui::vec2(10.0, 4.0);
    style.spacing.slider_width = if compact { 132.0 } else { 168.0 };
    for text_style in [egui::TextStyle::Body, egui::TextStyle::Button] {
        style.text_styles.get_mut(&text_style).unwrap().size = if compact { 14.0 } else { 15.0 };
    }
    style
        .text_styles
        .get_mut(&egui::TextStyle::Small)
        .unwrap()
        .size = 12.0;
    let visuals = &mut style.visuals;
    visuals.override_text_color = None;
    visuals.window_fill = fill;
    visuals.panel_fill = fill;
    visuals.extreme_bg_color = Color32::from_rgb(16, 20, 21);
    visuals.selection.bg_fill = Color32::from_rgb(38, 76, 77);
    visuals.selection.stroke.color = ACCENT;
    visuals.text_cursor.stroke.color = ACCENT;
    visuals.widgets.noninteractive.fg_stroke.color = MUTED_TEXT;
    visuals.widgets.noninteractive.bg_stroke.color = Color32::from_white_alpha(19);
    for (widget, fill) in [
        (&mut visuals.widgets.inactive, Color32::from_rgb(48, 53, 55)),
        (&mut visuals.widgets.hovered, Color32::from_rgb(58, 66, 67)),
        (&mut visuals.widgets.active, Color32::from_rgb(45, 78, 78)),
        (&mut visuals.widgets.open, Color32::from_rgb(47, 70, 70)),
    ] {
        widget.bg_fill = fill;
        widget.weak_bg_fill = fill;
        widget.bg_stroke.color = Color32::from_rgb(71, 78, 79);
        widget.fg_stroke.color = TEXT;
    }
}

fn header(ui: &mut egui::Ui, compact: bool) -> bool {
    let mut close = false;
    ui.horizontal(|ui| {
        ui.heading(
            RichText::new("Settings")
                .size(if compact { 23.0 } else { 27.0 })
                .color(TEXT)
                .strong(),
        );
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            close = ui
                .add(Button::new(RichText::new("×").size(22.0)).min_size(egui::vec2(34.0, 34.0)))
                .on_hover_text("Close settings")
                .clicked();
        });
    });
    close
}

fn page_button(ui: &mut egui::Ui, page: &mut SettingsPage, target: SettingsPage, width: f32) {
    let active = *page == target;
    let mut button =
        Button::new(RichText::new(target.label()).color(if active { ACCENT } else { TEXT }))
            .selected(active)
            .min_size(egui::vec2(width, 34.0))
            .corner_radius(CornerRadius::same(
                ui.visuals().window_corner_radius.nw.min(8),
            ));
    if active {
        button = button.fill(Color32::from_rgb(34, 55, 56));
    }
    if ui.add(button).clicked() {
        *page = target;
    }
}

fn scroll_page(
    ui: &mut egui::Ui,
    page: SettingsPage,
    height: f32,
    content: impl FnOnce(&mut egui::Ui, SettingsPage),
) {
    egui::ScrollArea::vertical()
        .id_salt(("glass_settings_page", page.label()))
        .max_height(height)
        .auto_shrink([false, false])
        .show(ui, |ui| content(ui, page));
}

// FVPROV1 6724 W3siYm9keSI6eyJhY3Rvcl9rZXlfaWQiOiI5ZjAwOTVhYmY4ODEwZGQwY2Y4NTdlZDBlNDI0ODg0NmRlMmFkMDc2ZmZhYTY2MWIzNmM5Yzc3YWFjOGVlOWMzIiwiYWxnb3JpdGhtIjoiRWQyNTUxOSIsImF1dGhvciI6IkoteC0gWiIsImNvbnRlbnRfc2hhMjU2IjoiYTM0NWRiMWVlMzBhODI4OTIxNjAxNTM1NjI5YzQ0ZGQyMWM1ZTA3N2Q5ZDYxYTFjZDJkMjc0NmVkNjg2OTg2OSIsImNvcHlfaWQiOiJ2MC4xLjAtYmV0YS4xIiwiY3JlYXRlZF9hdCI6IjIwMjYtMTEtMTgiLCJlZGl0aW9uIjoiYmV0YSIsImlzc3Vlcl9wb2xpY3kiOm51bGwsImtpbmQiOiJjcmVhdGUiLCJtb2RlIjoic2VsZl9zaWduIiwicGFyZW50X3ZlcnNpb24iOm51bGwsInByZXZpb3VzX3JlY29yZF9zaGEyNTYiOm51bGwsInNjaGVtYSI6ImZpbGV2ZWlsLnByb3ZlbmFuY2Uuc2lnbmF0dXJlLnYyIiwic2VxdWVuY2UiOjAsInRyYW5zZmVyX3RvX2tleV9pZCI6bnVsbCwidmVyc2lvbiI6InYwLjEuMC1iZXRhLjEiLCJ3b3JrX2lkIjoib3JnLnBseXJhLnRlcm1pbmFsIn0sInNpZ25hdHVyZXMiOlt7ImtleV9pZCI6IjlmMDA5NWFiZjg4MTBkZDBjZjg1N2VkMGU0MjQ4ODQ2ZGUyYWQwNzZmZmFhNjYxYjM2YzljNzdhYWM4ZWU5YzMiLCJwdWJsaWNfa2V5X2hleCI6ImMzZTkyZDhkZDNkZWM4NTliODdlOTY4MjIyYjI4OWJhNzg5M2NjOGM4NWZjOTE2MjI2YjZiNjI5NjFiOTU5ZDciLCJzaWduYXR1cmVfaGV4IjoiNmM4NGUxNDg3ZTIwYmQwZGVmMWI3ZWMwMDE1YTk0ODJhYzkyMTMzMjkzMWI4YjE4ODkwMjQyODI0NDgxZmQzZDM0ZDI3ZTM4NTE1MmRjNTM0NjRlODA3MjBhZDEzODFiMDZiNmQ1ZjE1MTgwNDliNzNkYmI0Y2IzOThkMmE3MDUifV19XQ== FVPROV1-END
