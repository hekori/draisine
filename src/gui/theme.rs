//! Look & feel: palette (light + dark, follows the system), fonts, and small custom widgets.

use eframe::egui::{
    self, Align2, Color32, CornerRadius, CursorIcon, FontData, FontDefinitions, FontFamily, FontId, Margin,
    Response, RichText, Sense, Stroke, TextStyle, Theme, Ui, Vec2, vec2,
};
use std::sync::Arc;

pub const BOLD: &str = "bold";
pub const ICONS: &str = "mdi";

/// Font for Material Design Icons glyphs (see `mdi` for code points).
pub fn icon_font(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name(ICONS.into()))
}

#[derive(Clone, Copy)]
pub struct Pal {
    pub bg: Color32,
    pub sidebar: Color32,
    pub surface: Color32,
    pub surface2: Color32,
    pub border: Color32,
    pub text: Color32,
    pub muted: Color32,
    pub accent: Color32,
    pub accent_soft: Color32,
    pub green: Color32,
    pub blue: Color32,
    pub orange: Color32,
    pub red: Color32,
    pub hover: Color32,
}

const LIGHT: Pal = Pal {
    bg: Color32::from_rgb(0xf4, 0xf5, 0xf7),
    sidebar: Color32::from_rgb(0xeb, 0xed, 0xf0),
    surface: Color32::from_rgb(0xff, 0xff, 0xff),
    surface2: Color32::from_rgb(0xf7, 0xf8, 0xfa),
    border: Color32::from_rgb(0xdf, 0xe2, 0xe8),
    text: Color32::from_rgb(0x1c, 0x20, 0x27),
    muted: Color32::from_rgb(0x6b, 0x72, 0x80),
    accent: Color32::from_rgb(0x1f, 0x8a, 0x5b),
    accent_soft: Color32::from_rgb(0xdc, 0xf1, 0xe6),
    green: Color32::from_rgb(0x1f, 0x8a, 0x5b),
    blue: Color32::from_rgb(0x25, 0x63, 0xeb),
    orange: Color32::from_rgb(0xc2, 0x6a, 0x00),
    red: Color32::from_rgb(0xc8, 0x2a, 0x2a),
    hover: Color32::from_rgb(0xe2, 0xe5, 0xea),
};

const DARK: Pal = Pal {
    bg: Color32::from_rgb(0x16, 0x18, 0x1d),
    sidebar: Color32::from_rgb(0x1b, 0x1e, 0x24),
    surface: Color32::from_rgb(0x21, 0x24, 0x2b),
    surface2: Color32::from_rgb(0x27, 0x2b, 0x33),
    border: Color32::from_rgb(0x32, 0x37, 0x41),
    text: Color32::from_rgb(0xe8, 0xea, 0xee),
    muted: Color32::from_rgb(0x9a, 0xa1, 0xad),
    accent: Color32::from_rgb(0x3d, 0xb8, 0x7e),
    accent_soft: Color32::from_rgb(0x1f, 0x3a, 0x2e),
    green: Color32::from_rgb(0x4c, 0xc2, 0x8a),
    blue: Color32::from_rgb(0x6b, 0x9b, 0xff),
    orange: Color32::from_rgb(0xf0, 0xa0, 0x3c),
    red: Color32::from_rgb(0xf0, 0x6a, 0x6a),
    hover: Color32::from_rgb(0x2a, 0x2e, 0x37),
};

pub fn pal(ui: &Ui) -> Pal {
    if ui.visuals().dark_mode { DARK } else { LIGHT }
}

fn visuals(p: Pal, dark: bool) -> egui::Visuals {
    let mut v = if dark { egui::Visuals::dark() } else { egui::Visuals::light() };
    v.panel_fill = p.bg;
    v.window_fill = p.surface;
    v.window_stroke = Stroke::new(1.0, p.border);
    v.window_corner_radius = CornerRadius::same(14);
    v.menu_corner_radius = CornerRadius::same(10);
    v.extreme_bg_color = p.surface2;
    v.faint_bg_color = p.surface2;
    v.text_edit_bg_color = Some(p.surface2);
    v.override_text_color = Some(p.text);
    v.hyperlink_color = p.accent;
    v.selection.bg_fill = p.accent.gamma_multiply(0.35);
    v.selection.stroke = Stroke::new(1.0, p.accent);
    v.striped = false;
    v.indent_has_left_vline = false;
    let r = CornerRadius::same(8);
    let w = &mut v.widgets;
    w.noninteractive.bg_stroke = Stroke::new(1.0, p.border);
    w.noninteractive.fg_stroke = Stroke::new(1.0, p.text);
    w.noninteractive.corner_radius = r;
    for (wv, fill) in [(&mut w.inactive, p.surface2), (&mut w.hovered, p.hover), (&mut w.active, p.hover), (&mut w.open, p.hover)] {
        wv.bg_fill = fill;
        wv.weak_bg_fill = fill;
        wv.corner_radius = r;
        wv.bg_stroke = Stroke::new(1.0, p.border);
        wv.fg_stroke = Stroke::new(1.0, p.text);
        wv.expansion = 0.0;
    }
    w.hovered.bg_stroke = Stroke::new(1.0, p.muted);
    w.active.bg_stroke = Stroke::new(1.0, p.accent);
    v
}

pub fn install(ctx: &egui::Context) {
    let mut fonts = FontDefinitions::default();
    fonts.font_data.insert("noto".into(), Arc::new(FontData::from_static(include_bytes!("../../assets/fonts/NotoSans-Regular.ttf"))));
    fonts.font_data.insert("noto-semibold".into(), Arc::new(FontData::from_static(include_bytes!("../../assets/fonts/NotoSans-SemiBold.ttf"))));
    let fallback = fonts.families.get(&FontFamily::Proportional).cloned().unwrap_or_default();
    fonts.families.get_mut(&FontFamily::Proportional).unwrap().insert(0, "noto".into());
    let mut bold = vec!["noto-semibold".to_string()];
    bold.extend(fallback);
    fonts.families.insert(FontFamily::Name(BOLD.into()), bold);
    // Material Design Icons 6.5.95 (Pictogrammers, Apache-2.0): only used via `icon_font`.
    fonts.font_data.insert(ICONS.into(), Arc::new(FontData::from_static(include_bytes!("../../assets/fonts/MaterialDesignIcons.ttf"))));
    fonts.families.insert(FontFamily::Name(ICONS.into()), vec![ICONS.into()]);
    ctx.set_fonts(fonts);

    ctx.set_visuals_of(Theme::Light, visuals(LIGHT, false));
    ctx.set_visuals_of(Theme::Dark, visuals(DARK, true));
    ctx.set_theme(egui::ThemePreference::System);
    ctx.all_styles_mut(|s| {
        s.text_styles = [
            (TextStyle::Heading, FontId::new(24.0, FontFamily::Name(BOLD.into()))),
            (TextStyle::Body, FontId::new(14.5, FontFamily::Proportional)),
            (TextStyle::Button, FontId::new(14.5, FontFamily::Proportional)),
            (TextStyle::Small, FontId::new(12.5, FontFamily::Proportional)),
            (TextStyle::Monospace, FontId::new(12.5, FontFamily::Monospace)),
        ]
        .into();
        s.spacing.item_spacing = vec2(8.0, 8.0);
        s.spacing.button_padding = vec2(14.0, 7.0);
        s.spacing.interact_size.y = 32.0;
        s.spacing.window_margin = Margin::same(20);
        s.spacing.icon_width = 18.0;
        s.spacing.icon_width_inner = 10.0;
    });
}

// ------------------------------------------------------------------ text helpers

pub fn bold(text: impl Into<String>, size: f32) -> RichText {
    RichText::new(text).family(FontFamily::Name(BOLD.into())).size(size)
}

pub fn muted(ui: &Ui, text: impl Into<String>) -> RichText {
    RichText::new(text).color(pal(ui).muted)
}

pub fn page_header(ui: &mut Ui, title: &str, subtitle: &str) {
    ui.label(bold(title, 24.0));
    if !subtitle.is_empty() {
        ui.label(muted(ui, subtitle));
    }
    ui.add_space(8.0);
}

pub fn section(ui: &mut Ui, title: &str) {
    ui.add_space(6.0);
    ui.label(bold(title.to_uppercase(), 11.5).color(pal(ui).muted));
}

// ------------------------------------------------------------------ containers

pub fn card<R>(ui: &mut Ui, add: impl FnOnce(&mut Ui) -> R) -> R {
    let p = pal(ui);
    egui::Frame::new()
        .fill(p.surface)
        .stroke(Stroke::new(1.0, p.border))
        .corner_radius(12)
        .inner_margin(Margin::same(18))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui)
        })
        .inner
}

/// Card with a coloured left accent (for warnings, errors, success).
pub fn callout<R>(ui: &mut Ui, color: Color32, add: impl FnOnce(&mut Ui) -> R) -> R {
    egui::Frame::new()
        .fill(color.gamma_multiply(0.10))
        .stroke(Stroke::new(1.0, color.gamma_multiply(0.45)))
        .corner_radius(10)
        .inner_margin(Margin::symmetric(14, 10))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui)
        })
        .inner
}

pub fn badge(ui: &mut Ui, text: &str, color: Color32) -> Response {
    // Painted at its exact size (instead of a Frame) so it never stretches to fill
    // the available height, e.g. inside a right-to-left / centered layout.
    let galley = egui::WidgetText::from(RichText::new(text).color(color).size(12.5)).into_galley(
        ui,
        Some(egui::TextWrapMode::Extend),
        f32::INFINITY,
        TextStyle::Body,
    );
    let pad = vec2(9.0, 3.0);
    let (rect, resp) = ui.allocate_exact_size(galley.size() + 2.0 * pad, Sense::hover());
    if ui.is_rect_visible(rect) {
        ui.painter().rect_filled(rect, CornerRadius::same(20), color.gamma_multiply(0.14));
        ui.painter().galley(rect.min + pad, galley, color);
    }
    resp
}

// ------------------------------------------------------------------ buttons

fn sized(text: RichText) -> egui::Button<'static> {
    egui::Button::new(text).min_size(vec2(0.0, 34.0)).corner_radius(8)
}

pub fn primary(ui: &mut Ui, text: &str) -> Response {
    let p = pal(ui);
    ui.add(sized(bold(text, 14.5).color(Color32::WHITE)).fill(p.accent).stroke(Stroke::NONE))
        .on_hover_cursor(CursorIcon::PointingHand)
}

pub fn primary_enabled(ui: &mut Ui, enabled: bool, text: &str) -> Response {
    let p = pal(ui);
    let fill = if enabled { p.accent } else { p.accent.gamma_multiply(0.4) };
    ui.add_enabled(enabled, sized(bold(text, 14.5).color(Color32::WHITE)).fill(fill).stroke(Stroke::NONE))
}

pub fn secondary(ui: &mut Ui, text: &str) -> Response {
    let p = pal(ui);
    ui.add(sized(RichText::new(text)).fill(p.surface).stroke(Stroke::new(1.0, p.border)))
        .on_hover_cursor(CursorIcon::PointingHand)
}

pub fn secondary_enabled(ui: &mut Ui, enabled: bool, text: &str) -> Response {
    let p = pal(ui);
    ui.add_enabled(enabled, sized(RichText::new(text)).fill(p.surface).stroke(Stroke::new(1.0, p.border)))
}

pub fn danger(ui: &mut Ui, text: &str) -> Response {
    let p = pal(ui);
    ui.add(sized(bold(text, 14.5).color(Color32::WHITE)).fill(p.red).stroke(Stroke::NONE))
        .on_hover_cursor(CursorIcon::PointingHand)
}

/// Borderless text button (links, small actions).
pub fn link(ui: &mut Ui, text: &str) -> Response {
    let p = pal(ui);
    ui.add(egui::Button::new(RichText::new(text).color(p.accent)).frame(false)).on_hover_cursor(CursorIcon::PointingHand)
}

// ------------------------------------------------------------------ toggle switch

pub fn toggle(ui: &mut Ui, on: &mut bool) -> Response {
    let p = pal(ui);
    let size = vec2(40.0, 22.0);
    let (rect, mut resp) = ui.allocate_exact_size(size, Sense::click());
    if resp.clicked() {
        *on = !*on;
        resp.mark_changed();
    }
    if ui.is_rect_visible(rect) {
        let t = ui.ctx().animate_bool_responsive(resp.id, *on);
        let off = if ui.visuals().dark_mode { Color32::from_gray(80) } else { Color32::from_gray(190) };
        let bg = lerp_color(off, p.accent, t);
        let r = rect.height() / 2.0;
        ui.painter().rect_filled(rect, r, bg);
        let x = egui::lerp((rect.left() + r)..=(rect.right() - r), t);
        ui.painter().circle_filled(egui::pos2(x, rect.center().y), r - 3.0, Color32::WHITE);
    }
    resp.on_hover_cursor(CursorIcon::PointingHand)
}

fn lerp_color(a: Color32, b: Color32, t: f32) -> Color32 {
    let l = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t) as u8;
    Color32::from_rgb(l(a.r(), b.r()), l(a.g(), b.g()), l(a.b(), b.b()))
}

// ------------------------------------------------------------------ navigation

pub fn nav_item(ui: &mut Ui, icon: &str, label: &str, selected: bool, badge_count: usize) -> Response {
    let p = pal(ui);
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 38.0), Sense::click());
    let bg = if selected {
        p.accent_soft
    } else if resp.hovered() {
        p.hover
    } else {
        Color32::TRANSPARENT
    };
    let painter = ui.painter();
    painter.rect_filled(rect, 8.0, bg);
    let fg = if selected { p.accent } else { p.muted };
    // Icons are centered on a common axis so narrow and wide glyphs line up.
    let ic = rect.left_center() + vec2(22.0, 0.0);
    if icon == "ℹ" {
        // The text font renders ℹ as a bare "i"; draw a circled i instead.
        painter.circle_stroke(ic, 7.5, Stroke::new(1.6, fg));
        painter.text(ic + vec2(0.0, 0.5), Align2::CENTER_CENTER, "i", FontId::new(11.5, FontFamily::Name(BOLD.into())), fg);
    } else {
        painter.text(ic, Align2::CENTER_CENTER, icon, FontId::proportional(16.0), fg);
    }
    let font = if selected { FontId::new(14.5, FontFamily::Name(BOLD.into())) } else { FontId::proportional(14.5) };
    painter.text(rect.left_center() + vec2(42.0, 0.0), Align2::LEFT_CENTER, label, font, if selected { p.text } else { p.text.gamma_multiply(0.85) });
    if badge_count > 0 {
        let c = rect.right_center() - vec2(20.0, 0.0);
        painter.circle_filled(c, 10.0, p.orange);
        painter.text(c, Align2::CENTER_CENTER, badge_count.to_string(), FontId::proportional(11.5), Color32::WHITE);
    }
    resp.on_hover_cursor(CursorIcon::PointingHand)
}

/// Selectable card (provider / option choice). Returns the response of the whole card.
pub fn choice_card(ui: &mut Ui, size: Vec2, selected: bool, icon: &str, title: &str, sub: &str) -> Response {
    let p = pal(ui);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    let stroke = if selected {
        Stroke::new(2.0, p.accent)
    } else if resp.hovered() {
        Stroke::new(1.0, p.muted)
    } else {
        Stroke::new(1.0, p.border)
    };
    let fill = if selected { p.accent_soft } else { p.surface };
    let painter = ui.painter();
    painter.rect(rect, 12.0, fill, stroke, egui::StrokeKind::Inside);
    // `icon` is a glyph, or "logo:<rclone type>" for that provider's icon.
    if let Some(typ) = icon.strip_prefix("logo:") {
        paint_provider_icon(painter, rect.left_top() + vec2(30.0, 27.0), typ, 28.0, p.accent);
    } else {
        painter.text(rect.left_top() + vec2(16.0, 14.0), Align2::LEFT_TOP, icon, FontId::proportional(22.0), p.accent);
    }
    painter.text(rect.left_top() + vec2(16.0, 48.0), Align2::LEFT_TOP, title, FontId::new(15.0, FontFamily::Name(BOLD.into())), p.text);
    if !sub.is_empty() {
        painter.text(rect.left_top() + vec2(16.0, 70.0), Align2::LEFT_TOP, sub, FontId::proportional(12.5), p.muted);
    }
    resp.on_hover_cursor(CursorIcon::PointingHand)
}

/// Step indicator for the setup assistant.
pub fn steps(ui: &mut Ui, labels: &[&str], current: usize) {
    let p = pal(ui);
    ui.horizontal(|ui| {
        for (i, l) in labels.iter().enumerate() {
            let (dot, txt) = if i <= current {
                (p.accent, p.text)
            } else {
                (p.border, p.muted)
            };
            let (rect, _) = ui.allocate_exact_size(vec2(24.0, 24.0), Sense::hover());
            ui.painter().circle_filled(rect.center(), 12.0, dot);
            let mark = if i < current { "✔".to_string() } else { (i + 1).to_string() };
            let mc = if i <= current { Color32::WHITE } else { p.muted };
            ui.painter().text(rect.center(), Align2::CENTER_CENTER, mark, FontId::proportional(12.0), mc);
            let rt = if i == current { bold(*l, 13.5).color(txt) } else { RichText::new(*l).size(13.5).color(txt) };
            ui.label(rt);
            if i + 1 < labels.len() {
                let (r, _) = ui.allocate_exact_size(vec2(28.0, 24.0), Sense::hover());
                ui.painter().hline(r.x_range(), r.center().y, Stroke::new(1.5, if i < current { p.accent } else { p.border }));
            }
        }
    });
}

/// One-line path with icon, home dir shown as ~, truncated with "…"; full path on hover.
pub fn path_line(ui: &mut Ui, icon: &str, path: &str) -> Response {
    ui.add(egui::Label::new(RichText::new(format!("{icon}  {}", short_path(path))).color(pal(ui).muted)).truncate())
        .on_hover_text(path)
}

/// `path` with the home directory shortened to `~`.
pub fn short_path(path: &str) -> String {
    if let Some(h) = directories::UserDirs::new() {
        let home = h.home_dir().display().to_string();
        if let Some(rest) = path.strip_prefix(&home) {
            return format!("~{rest}");
        }
    }
    path.to_string()
}

// ------------------------------------------------------------------ provider logos

/// Material Design Icons code points (https://pictogrammers.com/library/mdi/, v6.5.95).
pub mod mdi {
    pub const GOOGLE_DRIVE: char = '\u{F02B6}';
    pub const DROPBOX: char = '\u{F01E3}';
    pub const MICROSOFT_ONEDRIVE: char = '\u{F03CA}';
    pub const CLOUD_OUTLINE: char = '\u{F0163}';
    pub const BUCKET_OUTLINE: char = '\u{F1416}';
    pub const FOLDER_NETWORK_OUTLINE: char = '\u{F0C81}';
    pub const SERVER_SECURITY: char = '\u{F0492}';
}

/// Brand icon and color for an rclone backend type, if it has one.
pub fn provider_glyph(typ: &str) -> Option<(char, Color32)> {
    match typ {
        "drive" => Some((mdi::GOOGLE_DRIVE, Color32::from_rgb(0x1f, 0xa4, 0x63))),
        "dropbox" => Some((mdi::DROPBOX, Color32::from_rgb(0x00, 0x61, 0xff))),
        "onedrive" => Some((mdi::MICROSOFT_ONEDRIVE, Color32::from_rgb(0x00, 0x78, 0xd4))),
        "s3" => Some((mdi::BUCKET_OUTLINE, Color32::from_rgb(0xe2, 0x6a, 0x12))),
        "webdav" => Some((mdi::FOLDER_NETWORK_OUTLINE, Color32::from_rgb(0x00, 0x82, 0xc9))),
        "sftp" => Some((mdi::SERVER_SECURITY, Color32::from_rgb(0x5b, 0x6b, 0x7c))),
        _ => None,
    }
}

/// rclone backend types that have a brand icon.
pub fn has_logo(typ: &str) -> bool {
    provider_glyph(typ).is_some()
}

/// Human-readable name of an rclone backend type.
pub fn provider_name(typ: &str) -> &str {
    match typ {
        "drive" => "Google Drive",
        "dropbox" => "Dropbox",
        "onedrive" => "OneDrive",
        "pcloud" => "pCloud",
        "box" => "Box",
        "s3" => "S3",
        "webdav" => "WebDAV",
        "sftp" => "SFTP",
        t => t,
    }
}

/// Paints the provider's icon (or a cloud for unknown types) centered at `center`.
pub fn paint_provider_icon(painter: &egui::Painter, center: egui::Pos2, typ: &str, size: f32, fallback: Color32) {
    let (glyph, color) = provider_glyph(typ).unwrap_or((mdi::CLOUD_OUTLINE, fallback));
    painter.text(center, Align2::CENTER_CENTER, glyph, icon_font(size), color);
}

/// Square provider icon: the provider's brand icon, or a cloud for unknown types.
pub fn provider_icon(ui: &mut Ui, typ: &str, size: f32) -> Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(size, size), Sense::hover());
    if ui.is_rect_visible(rect) {
        paint_provider_icon(ui.painter(), rect.center(), typ, size, pal(ui).accent);
    }
    resp.on_hover_text(provider_name(typ))
}
