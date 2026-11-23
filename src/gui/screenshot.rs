//! Development only (cargo feature `screenshot`): saves a PNG of the window
//! at a chosen moment, then closes it.

use std::fs::File;
use std::io::BufWriter;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use eframe::egui::{self, ColorImage, ViewportCommand};

use super::cli::{CaptureWhen, Screenshot};
use super::model::{Length, Model, RunState};

/// Give up if the moment to capture has not come by then.
const TIMEOUT: Duration = Duration::from_secs(180);

/// Frames to wait after the chosen moment, so layout and fonts have settled.
const SETTLE_FRAMES: u32 = 4;

/// Shared slot for the outcome: `None` until the capture is done or failed.
pub type Outcome = Arc<Mutex<Option<Result<(), String>>>>;

/// Drives one capture.
pub struct Capture {
    request: Screenshot,
    started: Instant,
    /// Frames seen since the chosen moment arrived.
    settled: u32,
    requested: bool,
    auto_started: bool,
    outcome: Outcome,
}

impl Capture {
    /// Prepares `request`; its outcome will be written to `outcome`.
    pub fn new(request: Screenshot, outcome: Outcome) -> Capture {
        Capture {
            request,
            started: Instant::now(),
            settled: 0,
            requested: false,
            auto_started: false,
            outcome,
        }
    }

    /// Called once per frame, before the window is drawn.
    pub fn step(&mut self, ctx: &egui::Context, model: &mut Model) {
        ctx.request_repaint();
        if lock(&self.outcome).is_some() {
            ctx.send_viewport_cmd(ViewportCommand::Close);
            return;
        }
        if self.started.elapsed() > TIMEOUT {
            *lock(&self.outcome) = Some(Err("timed out waiting for the moment to capture".into()));
            return;
        }
        if self.request.when != CaptureWhen::Idle && !self.auto_started {
            self.auto_started = true;
            if self.request.when == CaptureWhen::Running {
                // Several passes, so the run is still going when captured.
                model.length = Length::Thorough;
            }
            model.start();
        }
        if self.requested {
            return;
        }
        let moment = match self.request.when {
            CaptureWhen::Idle => true,
            CaptureWhen::Running => match &model.run {
                RunState::Running(run) => {
                    run.progress.phase == memory_tester::engine::Phase::Testing
                        && run.progress.overall_fraction.unwrap_or(0.0) >= 0.1
                }
                RunState::Finished(_) => {
                    *lock(&self.outcome) = Some(Err("the run ended before it was captured".into()));
                    return;
                }
                RunState::Idle => false,
            },
            CaptureWhen::Done => model.finished().is_some(),
        };
        // While running, capture as soon as possible so the run is still going.
        let needed = if self.request.when == CaptureWhen::Running {
            0
        } else {
            SETTLE_FRAMES
        };
        if !moment {
            self.settled = 0;
            return;
        }
        self.settled += 1;
        if self.settled <= needed {
            return;
        }
        self.requested = true;
        let path = self.request.path.clone();
        let outcome = Arc::clone(&self.outcome);
        let repaint = ctx.clone();
        ctx.request_screenshot(move |image| {
            let result = save_png(&path, &image);
            if result.is_ok() {
                println!("saved {}", path.display());
            }
            *lock(&outcome) = Some(result);
            repaint.request_repaint();
        });
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Writes `image` to `path` as an 8-bit RGBA PNG.
fn save_png(path: &Path, image: &ColorImage) -> Result<(), String> {
    let [w, h] = image.size;
    let data: Vec<u8> = image
        .pixels
        .iter()
        .flat_map(|p| p.to_srgba_unmultiplied())
        .collect();
    let file = File::create(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let width = u32::try_from(w).map_err(|_| "image too wide".to_string())?;
    let height = u32::try_from(h).map_err(|_| "image too tall".to_string())?;
    let mut encoder = png::Encoder::new(BufWriter::new(file), width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().map_err(|e| e.to_string())?;
    writer.write_image_data(&data).map_err(|e| e.to_string())?;
    writer.finish().map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use eframe::egui::Color32;

    #[test]
    fn png_is_written_with_the_image_size() {
        let path =
            std::env::temp_dir().join(format!("memory-tester-shot-{}.png", std::process::id()));
        let image = ColorImage::new([3, 2], vec![Color32::RED; 6]);
        save_png(&path, &image).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n");
        // IHDR: width and height, big-endian, right after the chunk header.
        assert_eq!(&bytes[16..24], &[0, 0, 0, 3, 0, 0, 0, 2]);
    }
}
