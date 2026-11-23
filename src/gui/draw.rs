//! Colours and custom painting: the memory stick drawings.

use eframe::egui::{
    self, Color32, CornerRadius, Painter, Pos2, Rect, Shape, Stroke, StrokeKind, pos2, vec2,
};

use memory_tester::types::StickStatus;

use super::text::Shown;

/// Window background.
pub const BACKGROUND: Color32 = Color32::from_rgb(0xF7, 0xF8, 0xFA);
/// Main text.
pub const INK: Color32 = Color32::from_rgb(0x1F, 0x29, 0x37);
/// Secondary text.
pub const INK_SOFT: Color32 = Color32::from_rgb(0x5B, 0x64, 0x70);
/// Hairlines and outlines.
pub const LINE: Color32 = Color32::from_rgb(0xD5, 0xDA, 0xE1);
/// The Start button.
pub const ACCENT: Color32 = Color32::from_rgb(0x25, 0x63, 0xEB);
/// The DEMO tag; deliberately unlike any status colour.
pub const DEMO_TAG: Color32 = Color32::from_rgb(0x6D, 0x28, 0xD9);

/// Gold contacts along the bottom edge.
const GOLD: Color32 = Color32::from_rgb(0xD6, 0xB0, 0x4C);
const GOLD_DARK: Color32 = Color32::from_rgb(0xA9, 0x85, 0x2C);
/// Memory chips.
const CHIP: Color32 = Color32::from_rgb(0x23, 0x2B, 0x35);
const CHIP_LIT: Color32 = Color32::from_rgb(0x5E, 0x7A, 0xA8);
/// The outline of an empty slot.
const EMPTY_OUTLINE: Color32 = Color32::from_rgb(0xA3, 0xAA, 0xB4);

/// How one status is coloured.
///
/// The four result colours differ in lightness as well as hue (dark red,
/// mid blue-green, light amber, neutral grey) so that they stay apart for
/// people with red-green colour blindness; the words carry the rest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Look {
    /// Fill of the stick's board.
    pub board: Color32,
    /// Text in this colour on a light background, e.g. the status word.
    pub ink: Color32,
    /// Very light background for the result banner.
    pub tint: Color32,
}

/// The colours for what a slot shows.
pub fn look(shown: Shown) -> Look {
    let (board, ink, tint) = match shown {
        Shown::Empty => (BACKGROUND, INK_SOFT, BACKGROUND),
        Shown::Testing => (0x7C93B8, 0x2F5597, 0xEAF0F8).rgb(),
        Shown::Status(StickStatus::Untested) => (0x9AA1AB, 0x4B5563, 0xEEF0F3).rgb(),
        Shown::Status(StickStatus::Passed) => (0x12936A, 0x0B6E4F, 0xE3F4EC).rgb(),
        Shown::Status(StickStatus::Failed) => (0xC42B2B, 0xA61B1B, 0xFBE9E9).rgb(),
        Shown::Status(StickStatus::Suspect) => (0xEBAA1E, 0x7F5200, 0xFDF3D8).rgb(),
    };
    Look { board, ink, tint }
}

trait Rgb3 {
    fn rgb(self) -> (Color32, Color32, Color32);
}

impl Rgb3 for (u32, u32, u32) {
    fn rgb(self) -> (Color32, Color32, Color32) {
        let c = |v: u32| Color32::from_rgb((v >> 16) as u8, (v >> 8) as u8, v as u8);
        (c(self.0), c(self.1), c(self.2))
    }
}

/// Height of a stick drawing for a given width (real sticks are about
/// 4.3 : 1; slightly taller reads better at small sizes).
pub fn stick_height(width: f32) -> f32 {
    (width / 3.6).round()
}

/// Draws a memory stick filling `rect`: a board in the status colour with
/// chips, gold contacts with a notch, latch cut-outs at the ends, and a
/// badge whose symbol repeats the status for those who cannot tell the
/// colours apart. `time` (seconds) drives the gentle animation while testing.
pub fn stick(painter: &Painter, rect: Rect, shown: Shown, time: f64) {
    if shown == Shown::Empty {
        empty_slot(painter, rect);
        return;
    }
    let look = look(shown);
    let (w, h) = (rect.width(), rect.height());
    let radius = CornerRadius::same(3);
    painter.rect_filled(rect, radius, look.board);

    // Gold contacts along the bottom edge.
    let contacts_h = (h * 0.16).max(4.0);
    let contacts = Rect::from_min_max(
        pos2(rect.left() + w * 0.035, rect.bottom() - contacts_h),
        pos2(rect.right() - w * 0.035, rect.bottom() - 1.0),
    );
    // The dark rim keeps the contacts distinct on an amber board too.
    painter.rect(
        contacts,
        CornerRadius::same(1),
        GOLD,
        Stroke::new(1.0, GOLD_DARK),
        StrokeKind::Inside,
    );
    let pitch = 4.0;
    let mut x = contacts.left() + pitch;
    while x < contacts.right() - 1.0 {
        painter.vline(x, contacts.y_range(), Stroke::new(1.0, GOLD_DARK));
        x += pitch;
    }

    // Key notch in the contacts (off centre, as on DDR4), and the latch
    // cut-outs at both ends.
    let notch_w = (w * 0.025).max(4.0);
    let notch_x = rect.left() + w * 0.58;
    let notch = Rect::from_min_max(
        pos2(notch_x - notch_w / 2.0, rect.bottom() - contacts_h * 1.25),
        pos2(notch_x + notch_w / 2.0, rect.bottom() + 1.0),
    );
    painter.rect_filled(
        notch,
        CornerRadius {
            nw: 2,
            ne: 2,
            sw: 0,
            se: 0,
        },
        BACKGROUND,
    );
    let latch_r = (h * 0.07).max(2.5);
    let latch_y = rect.top() + h * 0.55;
    painter.circle_filled(pos2(rect.left(), latch_y), latch_r, BACKGROUND);
    painter.circle_filled(pos2(rect.right(), latch_y), latch_r, BACKGROUND);

    // Memory chips in two groups, with the middle of the board kept free
    // for the result badge (or, before there is a result, the small
    // configuration chip real sticks have there).
    let chips = if w >= 260.0 { 8 } else { 4 };
    let area = Rect::from_min_max(
        pos2(rect.left() + w * 0.06, rect.top() + h * 0.13),
        pos2(
            rect.right() - w * 0.06,
            rect.bottom() - contacts_h - h * 0.13,
        ),
    );
    let badge_r = (h * 0.22).clamp(7.0, 16.0);
    let gap = (w * 0.022).max(3.0);
    let middle = 2.0 * badge_r + 2.0 * gap;
    let chip_w = (area.width() - middle - gap * (chips - 2) as f32) / chips as f32;
    for i in 0..chips {
        let mut left = area.left() + i as f32 * (chip_w + gap);
        if i >= chips / 2 {
            left += middle - gap;
        }
        let chip = Rect::from_min_size(pos2(left, area.top()), vec2(chip_w, area.height()));
        let fill = if shown == Shown::Testing {
            // A slow wave of light moving along the chips.
            let phase = (time * 2.0 - i as f64 * 0.7).sin() * 0.5 + 0.5;
            lerp_color(CHIP, CHIP_LIT, (phase * phase) as f32)
        } else {
            CHIP
        };
        painter.rect_filled(chip, CornerRadius::same(2), fill);
    }

    match shown {
        Shown::Status(status) if status != StickStatus::Untested => {
            badge(painter, area.center(), badge_r, status, look.ink);
        }
        _ => {
            let side = (badge_r * 0.9).min(area.height() * 0.5);
            let spd = Rect::from_center_size(
                pos2(area.center().x, area.bottom() - side / 2.0),
                vec2(side, side),
            );
            painter.rect_filled(spd, CornerRadius::same(1), CHIP);
        }
    }
}

/// A white disc of radius `r` with a symbol for the result: a tick, a cross
/// or "!". Draws nothing for `Untested`.
pub fn badge(painter: &Painter, center: Pos2, r: f32, status: StickStatus, ink: Color32) {
    if status == StickStatus::Untested {
        return;
    }
    painter.circle(center, r, Color32::WHITE, Stroke::new(1.0, ink));
    let stroke = Stroke::new((r * 0.24).max(1.6), ink);
    let p = |dx: f32, dy: f32| center + vec2(dx * r, dy * r);
    match status {
        StickStatus::Passed => {
            painter.line(vec![p(-0.45, 0.02), p(-0.12, 0.35), p(0.48, -0.32)], stroke);
        }
        StickStatus::Failed => {
            painter.line_segment([p(-0.38, -0.38), p(0.38, 0.38)], stroke);
            painter.line_segment([p(-0.38, 0.38), p(0.38, -0.38)], stroke);
        }
        StickStatus::Suspect => {
            painter.line_segment([p(0.0, -0.5), p(0.0, 0.12)], stroke);
            painter.circle_filled(p(0.0, 0.45), stroke.width * 0.6, ink);
        }
        StickStatus::Untested => {}
    }
}

/// A dashed outline where a stick would go.
fn empty_slot(painter: &Painter, rect: Rect) {
    let r = rect.shrink(1.0);
    let corners = [
        r.left_top(),
        r.right_top(),
        r.right_bottom(),
        r.left_bottom(),
        r.left_top(),
    ];
    let stroke = Stroke::new(1.5, EMPTY_OUTLINE);
    painter.extend(Shape::dashed_line(&corners, stroke, 6.0, 4.0));
    // Where the contacts would be, fainter.
    let y = rect.bottom() - (rect.height() * 0.16).max(4.0);
    painter.extend(Shape::dashed_line(
        &[
            pos2(rect.left() + rect.width() * 0.035, y),
            pos2(rect.right() - rect.width() * 0.035, y),
        ],
        Stroke::new(1.0, LINE),
        4.0,
        4.0,
    ));
}

/// A small filled dot in a status colour, for the "Previously tested" list.
pub fn dot(painter: &Painter, center: Pos2, shown: Shown) {
    painter.circle_filled(center, 5.0, look(shown).board);
}

/// A rounded outline around a card, drawn behind its contents.
pub fn card_outline(painter: &Painter, rect: Rect) {
    painter.rect(
        rect,
        CornerRadius::same(8),
        Color32::WHITE,
        Stroke::new(1.0, LINE),
        StrokeKind::Inside,
    );
}

fn lerp_color(a: Color32, b: Color32, t: f32) -> Color32 {
    let t = t.clamp(0.0, 1.0);
    let mix = |x: u8, y: u8| (f32::from(x) + (f32::from(y) - f32::from(x)) * t).round() as u8;
    Color32::from_rgb(mix(a.r(), b.r()), mix(a.g(), b.g()), mix(a.b(), b.b()))
}

/// Shorthand used by the window code for a text style's font.
pub fn font(ui: &egui::Ui, style: egui::TextStyle) -> egui::FontId {
    style.resolve(ui.style())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Relative luminance per WCAG 2.
    fn luminance(c: Color32) -> f32 {
        let lin = |v: u8| {
            let v = f32::from(v) / 255.0;
            if v <= 0.04045 {
                v / 12.92
            } else {
                ((v + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * lin(c.r()) + 0.7152 * lin(c.g()) + 0.0722 * lin(c.b())
    }

    fn contrast(a: Color32, b: Color32) -> f32 {
        let (la, lb) = (luminance(a), luminance(b));
        (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
    }

    const ALL: [Shown; 6] = [
        Shown::Empty,
        Shown::Testing,
        Shown::Status(StickStatus::Untested),
        Shown::Status(StickStatus::Passed),
        Shown::Status(StickStatus::Failed),
        Shown::Status(StickStatus::Suspect),
    ];

    #[test]
    fn status_words_are_readable_on_white_and_on_their_tint() {
        for shown in ALL {
            let l = look(shown);
            assert!(contrast(l.ink, Color32::WHITE) >= 4.5, "{shown:?} on white");
            assert!(contrast(l.ink, l.tint) >= 4.5, "{shown:?} on its tint");
        }
    }

    #[test]
    fn result_colours_differ_in_lightness_not_just_hue() {
        // Failed, Passed, Untested and Suspect boards ordered dark to light,
        // each clearly apart, so they do not collapse together when red and
        // green look alike.
        let l = |s| luminance(look(Shown::Status(s)).board);
        let ordered = [
            l(StickStatus::Failed),
            l(StickStatus::Passed),
            l(StickStatus::Untested),
            l(StickStatus::Suspect),
        ];
        for pair in ordered.windows(2) {
            assert!(pair[1] > pair[0] * 1.25, "{ordered:?}");
        }
    }

    #[test]
    fn stick_height_keeps_a_stick_shape() {
        assert_eq!(stick_height(360.0), 100.0);
        assert!(stick_height(160.0) >= 40.0);
    }
}
