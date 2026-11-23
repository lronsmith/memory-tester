//! The window: lays out and paints what the [`Model`] says.

use std::time::Duration;

use eframe::egui::{
    self, Align, Align2, Color32, CornerRadius, FontId, Layout, Margin, Rect, RichText, Sense,
    Stroke, TextStyle, Ui, ViewportCommand, pos2, scroll_area::ScrollBarVisibility, vec2,
};

use memory_tester::types::StickStatus;

use super::draw::{self, INK, INK_SOFT};
use super::model::{Model, RunState, overall_status};
use super::text::{self, Bar, Shown};

/// How often the window redraws while a run is going.
const RUNNING_REPAINT: Duration = Duration::from_millis(100);

/// Narrowest and widest a stick card may be.
const CARD_MIN_W: f32 = 150.0;
const CARD_MAX_W: f32 = 340.0;
/// Space between cards, and inside a card around its contents.
const CARD_GAP: f32 = 12.0;
const CARD_PAD: f32 = 12.0;
/// At most this many cards side by side.
const MAX_COLUMNS: usize = 4;

/// The eframe application.
pub struct App {
    model: Model,
    /// Physical memory size from the OS, for the "Installed memory" block.
    total_bytes: Option<u64>,
    /// The window was closed during a run: close it once the run has stopped.
    close_when_stopped: bool,
    #[cfg(feature = "screenshot")]
    capture: Option<super::screenshot::Capture>,
}

impl App {
    /// Wraps a model and applies the window's look to `ctx`.
    pub fn new(ctx: &egui::Context, model: Model) -> App {
        setup_style(ctx);
        App {
            model,
            total_bytes: memory_tester::engine::total_physical_bytes(),
            close_when_stopped: false,
            #[cfg(feature = "screenshot")]
            capture: None,
        }
    }

    /// Makes the window capture itself as `capture` says, then close.
    #[cfg(feature = "screenshot")]
    pub fn with_capture(mut self, capture: super::screenshot::Capture) -> App {
        self.capture = Some(capture);
        self
    }

    /// Closing the window during a run: stop the run first, keep the window
    /// responsive while it winds down, then close.
    fn handle_close(&mut self, ctx: &egui::Context) {
        if ctx.input(|i| i.viewport().close_requested()) && self.model.is_running() {
            ctx.send_viewport_cmd(ViewportCommand::CancelClose);
            self.model.stop();
            self.close_when_stopped = true;
        }
        if self.close_when_stopped && !self.model.is_running() {
            ctx.send_viewport_cmd(ViewportCommand::Close);
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.model.poll();
        self.handle_close(&ctx);
        #[cfg(feature = "screenshot")]
        if let Some(capture) = &mut self.capture {
            capture.step(&ctx, &mut self.model);
        }

        egui::Panel::bottom("controls")
            .frame(
                egui::Frame::new()
                    .fill(Color32::WHITE)
                    .inner_margin(Margin::symmetric(20, 14)),
            )
            .show(ui, |ui| self.controls(ui));

        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(draw::BACKGROUND))
            .show(ui, |ui| {
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .scroll_bar_visibility(ScrollBarVisibility::VisibleWhenNeeded)
                    .show(ui, |ui| {
                        egui::Frame::new()
                            .inner_margin(Margin::symmetric(20, 16))
                            .show(ui, |ui| self.body(ui));
                    });
            });

        if self.model.is_running() {
            ctx.request_repaint_after(RUNNING_REPAINT);
        }
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        // Normally the run has already stopped (see `handle_close`); this
        // also covers any other way the window goes away.
        self.model.stop_and_wait();
    }

    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        draw::BACKGROUND.to_normalized_gamma_f32()
    }
}

/// Light theme, comfortable text sizes, roomier controls.
fn setup_style(ctx: &egui::Context) {
    ctx.set_theme(egui::Theme::Light);
    ctx.all_styles_mut(|style| {
        use egui::FontFamily::{Monospace, Proportional};
        style.text_styles = [
            (TextStyle::Small, FontId::new(12.5, Proportional)),
            (TextStyle::Body, FontId::new(14.5, Proportional)),
            (TextStyle::Button, FontId::new(14.5, Proportional)),
            (TextStyle::Heading, FontId::new(22.0, Proportional)),
            (TextStyle::Monospace, FontId::new(12.0, Monospace)),
        ]
        .into();
        style.spacing.item_spacing = vec2(8.0, 6.0);
        style.spacing.button_padding = vec2(10.0, 5.0);
        style.spacing.interact_size.y = 26.0;
        style.visuals.override_text_color = Some(INK);
        style.visuals.panel_fill = draw::BACKGROUND;
        // A scroll bar that is always drawn when there is more to see, so
        // nobody misses the content below the fold.
        style.spacing.scroll = egui::style::ScrollStyle::solid();
    });
}

// --- Main area -------------------------------------------------------------

impl App {
    fn body(&mut self, ui: &mut Ui) {
        self.header(ui);
        ui.add_space(12.0);
        if let Some(finished) = self.model.finished() {
            result_banner(ui, finished);
            ui.add_space(14.0);
        }
        self.sticks(ui);
        if let Some(finished) = self.model.finished() {
            ui.add_space(10.0);
            details(ui, finished);
        }
        let past = self.model.past_results();
        if !past.is_empty() {
            ui.add_space(14.0);
            past_results(ui, &past);
        }
        let has_saved = !self.model.history.records().is_empty();
        if !self.model.is_running() && (has_saved || self.model.forget_error.is_some()) {
            ui.add_space(14.0);
            ui.horizontal_wrapped(|ui| self.forget_control(ui));
        }
    }

    fn header(&self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            ui.heading("Memory Tester");
            if self.model.demo_kind().is_some() {
                ui.add_space(6.0);
                egui::Frame::new()
                    .fill(draw::DEMO_TAG)
                    .corner_radius(CornerRadius::same(4))
                    .inner_margin(Margin::symmetric(7, 2))
                    .show(ui, |ui| {
                        ui.label(RichText::new("DEMO").small().color(Color32::WHITE));
                    });
            }
        });
        let mut summary = text::installed_summary(&self.model.slots);
        if self.model.demo_kind().is_some() {
            summary.push_str(" Demo mode: results are not saved.");
        }
        ui.label(RichText::new(summary).color(INK_SOFT));
    }

    /// The grid of stick cards (or the single "Installed memory" block).
    fn sticks(&self, ui: &mut Ui) {
        let cards = self.cards();
        let avail = ui.available_width();
        let fit = ((avail + CARD_GAP) / (CARD_MIN_W + CARD_GAP))
            .floor()
            .max(1.0) as usize;
        let columns = cards.len().clamp(1, MAX_COLUMNS).min(fit);
        let card_w = ((avail - CARD_GAP * (columns - 1) as f32) / columns as f32).min(CARD_MAX_W);
        let row_w = card_w * columns as f32 + CARD_GAP * (columns - 1) as f32;
        let card_h = card_height(ui, card_w, &cards);
        let rows = cards.len().div_ceil(columns);
        let total_h = card_h * rows as f32 + CARD_GAP * rows.saturating_sub(1) as f32;
        let (area, _) = ui.allocate_exact_size(vec2(avail, total_h), Sense::hover());
        let left = area.left() + ((avail - row_w) / 2.0).max(0.0);
        let time = ui.input(|i| i.time);
        for (i, card) in cards.iter().enumerate() {
            let (row, col) = (i / columns, i % columns);
            let min = pos2(
                left + col as f32 * (card_w + CARD_GAP),
                area.top() + row as f32 * (card_h + CARD_GAP),
            );
            paint_card(
                ui,
                Rect::from_min_size(min, vec2(card_w, card_h)),
                card,
                time,
                i,
            );
        }
    }

    fn cards(&self) -> Vec<Card> {
        if self.model.sticks_unknown() {
            let mut lines = Vec::new();
            if let Some(bytes) = self.total_bytes {
                lines.push(Line::normal(memory_tester::verdict::format_gb(bytes)));
            }
            lines.push(Line::soft("Individual sticks not detected".to_string()));
            return vec![Card {
                label: "Installed memory".to_string(),
                shown: self.model.unknown_sticks_view(),
                lines,
            }];
        }
        self.model
            .slots
            .iter()
            .zip(self.model.slot_views())
            .enumerate()
            .map(|(i, (slot, view))| {
                let mut lines = Vec::new();
                if let Some(stick) = &slot.stick {
                    lines.push(Line::normal(text::size_text(stick)));
                    lines.extend(text::name_text(stick).map(Line::soft));
                    lines.extend(text::kind_text(stick).map(Line::soft));
                }
                if let Some(note) = view.note {
                    lines.push(Line::note(note));
                }
                Card {
                    label: text::slot_label(slot, i),
                    shown: view.shown,
                    lines,
                }
            })
            .collect()
    }
}

/// One stick card's content.
struct Card {
    label: String,
    shown: Shown,
    lines: Vec<Line>,
}

/// One caption line under a stick.
struct Line {
    text: String,
    font: TextStyle,
    color: Color32,
    /// Wraps onto further lines instead of being cut short.
    wrap: bool,
}

impl Line {
    fn normal(text: String) -> Line {
        Line {
            text,
            font: TextStyle::Body,
            color: INK,
            wrap: false,
        }
    }
    fn soft(text: String) -> Line {
        Line {
            text,
            font: TextStyle::Body,
            color: INK_SOFT,
            wrap: false,
        }
    }
    /// Notes carry a date that must not be cut off, so they wrap.
    fn note(text: String) -> Line {
        Line {
            text,
            font: TextStyle::Small,
            color: INK_SOFT,
            wrap: true,
        }
    }

    /// Height this line takes in a card whose text is `width` wide.
    fn height(&self, ui: &Ui, width: f32) -> f32 {
        let font = draw::font(ui, self.font.clone());
        if self.wrap {
            ui.painter()
                .layout(self.text.clone(), font, self.color, width)
                .size()
                .y
                + 3.0
        } else {
            line_height(ui, &font)
        }
    }
}

/// Font for the status word on a card.
fn status_font() -> FontId {
    FontId::proportional(16.5)
}

fn line_height(ui: &Ui, font: &FontId) -> f32 {
    ui.fonts_mut(|f| f.row_height(font)) + 3.0
}

/// All cards in the grid share the height of the tallest, so their stick
/// drawings and status words line up.
fn card_height(ui: &Ui, card_w: f32, cards: &[Card]) -> f32 {
    let body = draw::font(ui, TextStyle::Body);
    let text_w = card_w - 2.0 * CARD_PAD;
    let lines = cards
        .iter()
        .map(|c| c.lines.iter().map(|l| l.height(ui, text_w)).sum::<f32>())
        .fold(0.0, f32::max);
    let stick_h = draw::stick_height(text_w);
    CARD_PAD
        + line_height(ui, &body)
        + 4.0
        + stick_h
        + 8.0
        + line_height(ui, &status_font())
        + lines
        + CARD_PAD
}

/// Paints one card: label, stick drawing, status word, details.
fn paint_card(ui: &mut Ui, rect: Rect, card: &Card, time: f64, index: usize) {
    let painter = ui.painter_at(rect);
    draw::card_outline(&painter, rect);
    let inner = rect.shrink(CARD_PAD);
    let mut y = inner.top();

    let body = draw::font(ui, TextStyle::Body);
    put_text(
        ui,
        &card.label,
        body.clone(),
        INK,
        inner.left(),
        y,
        inner.width(),
        (index, 0),
    );
    y += line_height(ui, &body) + 4.0;

    let stick_rect = Rect::from_min_size(
        pos2(inner.left(), y),
        vec2(inner.width(), draw::stick_height(inner.width())),
    );
    draw::stick(&painter, stick_rect, card.shown, time);
    y = stick_rect.bottom() + 8.0;

    let look = draw::look(card.shown);
    let status = text::status_word(card.shown);
    put_text(
        ui,
        status,
        status_font(),
        look.ink,
        inner.left(),
        y,
        inner.width(),
        (index, 1),
    );
    y += line_height(ui, &status_font());

    for (n, line) in card.lines.iter().enumerate() {
        let font = draw::font(ui, line.font.clone());
        if line.wrap {
            let galley = painter.layout(line.text.clone(), font, line.color, inner.width());
            painter.galley(pos2(inner.left(), y), galley, line.color);
            y += line.height(ui, inner.width());
            continue;
        }
        put_text(
            ui,
            &line.text,
            font.clone(),
            line.color,
            inner.left(),
            y,
            inner.width(),
            (index, n + 2),
        );
        y += line_height(ui, &font);
    }
}

/// Paints one line of text at (x, y), cut with an ellipsis to `max_w`. A cut
/// line shows its full text when hovered.
#[expect(clippy::too_many_arguments, reason = "plain drawing parameters")]
fn put_text(
    ui: &mut Ui,
    full: &str,
    font: FontId,
    color: Color32,
    x: f32,
    y: f32,
    max_w: f32,
    id: (usize, usize),
) {
    let painter = ui.painter().clone();
    let measure = |s: &str| {
        painter
            .layout_no_wrap(s.to_string(), font.clone(), color)
            .size()
            .x
    };
    let shown = text::truncate_to_width(full, max_w, measure);
    let rect = painter.text(pos2(x, y), Align2::LEFT_TOP, &shown, font, color);
    if shown != full {
        ui.interact(rect, ui.scope_id().with(("card-text", id)), Sense::hover())
            .on_hover_text(full);
    }
}

/// The large coloured result banner with the verdict's words.
fn result_banner(ui: &mut Ui, finished: &super::model::Finished) {
    let status = overall_status(Some(finished.verdict.overall));
    let look = draw::look(Shown::Status(status));
    egui::Frame::new()
        .fill(look.tint)
        .stroke(Stroke::new(1.5, look.board))
        .corner_radius(CornerRadius::same(10))
        .inner_margin(Margin::symmetric(18, 14))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                if status != StickStatus::Untested {
                    let (rect, _) = ui.allocate_exact_size(vec2(30.0, 30.0), Sense::hover());
                    draw::badge(ui.painter(), rect.center(), 14.0, status, look.ink);
                    ui.add_space(4.0);
                }
                ui.label(
                    RichText::new(&finished.verdict.headline)
                        .size(22.0)
                        .color(look.ink),
                );
            });
            ui.add_space(4.0);
            ui.label(RichText::new(&finished.verdict.advice).color(INK));
            let notes = text::lock_note(&finished.summary)
                .map(str::to_string)
                .into_iter()
                .chain(finished.save_error.clone());
            for note in notes {
                ui.add_space(2.0);
                ui.label(RichText::new(note).small().color(INK_SOFT));
            }
        });
}

/// The collapsed "Details" section: amounts, passes, time and every
/// recorded error.
fn details(ui: &mut Ui, finished: &super::model::Finished) {
    egui::CollapsingHeader::new("Details")
        .default_open(false)
        .show(ui, |ui| {
            for line in text::details_summary(&finished.summary) {
                ui.label(RichText::new(line).color(INK_SOFT));
            }
            if finished.summary.errors.is_empty() {
                return;
            }
            ui.add_space(6.0);
            // Rows keep their columns: they never wrap, and scroll sideways
            // when the window is narrow.
            let row = |ui: &mut Ui, text: String, color: Color32| {
                ui.add(egui::Label::new(RichText::new(text).monospace().color(color)).extend());
            };
            egui::ScrollArea::horizontal().show(ui, |ui| {
                row(ui, text::error_table_header(), INK_SOFT);
                for error in &finished.summary.errors {
                    row(ui, text::error_row(error), INK);
                }
                if let Some(more) = text::unlisted_errors_note(&finished.summary) {
                    row(ui, more, INK_SOFT);
                }
            });
        });
}

/// The compact list of remembered sticks that are not installed now.
fn past_results(ui: &mut Ui, past: &[super::model::PastResult]) {
    ui.label(RichText::new("Previously tested").size(16.5));
    ui.label(
        RichText::new("Sticks this app remembers that are not in the computer now.")
            .small()
            .color(INK_SOFT),
    );
    ui.add_space(4.0);
    egui::Frame::new()
        .fill(Color32::WHITE)
        .stroke(Stroke::new(1.0, draw::LINE))
        .corner_radius(CornerRadius::same(8))
        .inner_margin(Margin::symmetric(12, 8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            for (i, row) in past.iter().enumerate() {
                let shown = Shown::Status(row.status);
                ui.horizontal(|ui| {
                    let (dot, _) = ui.allocate_exact_size(vec2(12.0, 18.0), Sense::hover());
                    draw::dot(ui.painter(), dot.center(), shown);
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.label(RichText::new(&row.date).color(INK_SOFT));
                        ui.add_space(8.0);
                        let word = text::status_word(shown);
                        ui.add_sized(
                            vec2(70.0, 18.0),
                            egui::Label::new(RichText::new(word).color(draw::look(shown).ink)),
                        );
                        ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                            let font = draw::font(ui, TextStyle::Body);
                            let w = ui.available_width();
                            let y = ui.cursor().top() + (18.0 - line_height(ui, &font) + 3.0) / 2.0;
                            let x = ui.cursor().left();
                            put_text(ui, &row.label, font, INK, x, y, w, (usize::MAX, i));
                        });
                    });
                });
            }
        });
}

// --- Controls --------------------------------------------------------------

impl App {
    fn controls(&mut self, ui: &mut Ui) {
        let running = self.model.is_running();
        if let RunState::Running(run) = &self.model.run {
            progress_bar(ui, &text::progress_bar(&run.progress));
            ui.add_space(4.0);
            status_line(ui, &text::status_parts(&run.progress));
            ui.add_space(8.0);
        }

        ui.horizontal(|ui| {
            self.start_stop_button(ui);
            ui.add_space(6.0);
            ui.add_enabled_ui(!running, |ui| {
                let length = &mut self.model.length;
                egui::ComboBox::from_id_salt("length")
                    .selected_text(length.label())
                    .width(180.0)
                    .show_ui(ui, |ui| {
                        for choice in super::model::Length::ALL {
                            ui.selectable_value(length, choice, choice.label());
                        }
                    });
            });
        });

        if matches!(self.model.run, RunState::Idle) {
            ui.add_space(4.0);
            ui.label(
                RichText::new("Close other programs first so more memory can be tested.")
                    .color(INK_SOFT),
            );
        }
    }

    fn start_stop_button(&mut self, ui: &mut Ui) {
        let big = |label: &str, fill: Color32| {
            egui::Button::new(RichText::new(label).size(18.0).color(Color32::WHITE))
                .fill(fill)
                .min_size(vec2(170.0, 42.0))
                .corner_radius(CornerRadius::same(8))
        };
        match &self.model.run {
            RunState::Running(run) if run.stopping => {
                ui.add_enabled(false, big("Stopping…", Color32::from_rgb(0x6B, 0x72, 0x80)));
            }
            RunState::Running(_) => {
                if ui
                    .add(big("Stop", Color32::from_rgb(0x37, 0x41, 0x51)))
                    .clicked()
                {
                    self.model.stop();
                }
            }
            RunState::Idle | RunState::Finished(_) => {
                if ui.add(big("Start test", draw::ACCENT)).clicked() {
                    self.model.start();
                }
            }
        }
    }

    /// "Forget saved results", which asks once before it acts.
    fn forget_control(&mut self, ui: &mut Ui) {
        if self.model.confirm_forget {
            ui.label(RichText::new("Forget all saved results?").color(INK_SOFT));
            if ui.button("Forget them").clicked() {
                self.model.forget();
            }
            if ui.button("Cancel").clicked() {
                self.model.confirm_forget = false;
            }
        } else if !self.model.history.records().is_empty() {
            let link = egui::Button::new(
                RichText::new("Forget saved results")
                    .small()
                    .color(INK_SOFT),
            )
            .frame(false);
            if ui.add(link).clicked() {
                self.model.ask_forget();
            }
        }
        if let Some(error) = &self.model.forget_error {
            ui.label(RichText::new(error).small().color(INK_SOFT));
        }
    }
}

/// A full-width progress bar; when nothing is measurable yet, a band slides
/// along it to show activity.
fn progress_bar(ui: &mut Ui, bar: &Bar) {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 24.0), Sense::hover());
    let painter = ui.painter();
    let radius = CornerRadius::same(6);
    painter.rect_filled(rect, radius, Color32::from_rgb(0xE5, 0xE8, 0xEC));
    match bar {
        Bar::Busy => {
            let t = ui.input(|i| i.time);
            let band = rect.width() * 0.25;
            let travel = rect.width() + band;
            let x = rect.left() - band + ((t * 0.35).fract() as f32) * travel;
            let fill = Rect::from_min_max(
                pos2(x.max(rect.left()), rect.top()),
                pos2((x + band).min(rect.right()), rect.bottom()),
            );
            if fill.width() > 0.0 {
                painter.rect_filled(fill, radius, Color32::from_rgb(0x9D, 0xB7, 0xE8));
            }
        }
        Bar::Fill { fraction, label } => {
            let w = (rect.width() * fraction).max(if *fraction > 0.0 { 12.0 } else { 0.0 });
            if w > 0.0 {
                let fill = Rect::from_min_size(rect.min, vec2(w, rect.height()));
                painter.rect_filled(fill, radius, draw::ACCENT);
            }
            // The label sits centred; dark on the track, white where filled.
            let font = draw::font(ui, TextStyle::Body);
            let galley = painter.layout_no_wrap(label.clone(), font, INK);
            let pos = rect.center() - galley.size() / 2.0;
            let text_rect = Rect::from_min_size(pos, galley.size());
            let filled_to = rect.left() + w;
            painter
                .with_clip_rect(Rect::from_min_max(pos2(filled_to, rect.top()), rect.max))
                .galley(pos, galley.clone(), INK);
            if filled_to > text_rect.left() {
                painter
                    .with_clip_rect(Rect::from_min_max(rect.min, pos2(filled_to, rect.bottom())))
                    .galley_with_override_text_color(pos, galley, Color32::WHITE);
            }
        }
    }
}

/// The one-line status under the progress bar; a non-zero error count is
/// highlighted.
fn status_line(ui: &mut Ui, parts: &[text::Part]) {
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 0.0;
        for (i, part) in parts.iter().enumerate() {
            if i > 0 {
                ui.label(RichText::new(text::PART_SEPARATOR).color(INK_SOFT));
            }
            let rich = if part.alert {
                let look = draw::look(Shown::Status(StickStatus::Failed));
                RichText::new(format!(" {} ", part.text))
                    .color(Color32::WHITE)
                    .background_color(look.board)
            } else {
                RichText::new(&part.text).color(INK_SOFT)
            };
            ui.label(rich);
        }
    });
}
