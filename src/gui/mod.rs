//! The window: shows the memory sticks, runs the test and shows the result.
//!
//! - [`cli`]    command-line flags (demo mode, screenshots)
//! - [`demo`]   the pretend computers behind `--demo`
//! - [`model`]  the window's state and the rules for what each slot shows
//! - [`text`]   every piece of text built from data
//! - [`draw`]   colours and the custom-painted stick drawings
//! - [`app`]    the eframe application that lays it all out

mod app;
pub mod cli;
mod demo;
mod draw;
mod model;
#[cfg(feature = "screenshot")]
mod screenshot;
mod text;

use eframe::egui;
use memory_tester::history::History;

use cli::Options;
use model::Model;

pub use model::stop_active_run;

/// Default and smallest window size, in points.
const WINDOW_SIZE: [f32; 2] = [760.0, 560.0];
const MIN_WINDOW_SIZE: [f32; 2] = [520.0, 420.0];

/// Why the window could not do its job.
#[derive(Debug)]
pub enum RunError {
    /// The window could not be created (for example, broken graphics drivers).
    Window(String),
    /// A screenshot was asked for but not saved.
    #[cfg(feature = "screenshot")]
    Screenshot(String),
}

/// The window title.
fn title(demo: Option<cli::Demo>) -> &'static str {
    if demo.is_some() {
        "Memory Tester (demo)"
    } else {
        "Memory Tester"
    }
}

/// Opens the window and runs until it is closed.
pub fn run(options: Options) -> Result<(), RunError> {
    let model = match options.demo {
        Some(demo) => Model::demo(demo),
        // Failed detection shows one "Installed memory" block; the test
        // itself does not need to know the sticks.
        None => Model::new(
            memory_tester::smbios::detect_slots().unwrap_or_default(),
            History::load_default(),
        ),
    };
    let title = title(options.demo);
    let native = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(title)
            .with_inner_size(WINDOW_SIZE)
            .with_min_inner_size(MIN_WINDOW_SIZE)
            .with_resizable(true),
        centered: true,
        ..Default::default()
    };

    #[cfg(feature = "screenshot")]
    let outcome = screenshot::Outcome::default();
    #[cfg(feature = "screenshot")]
    let capture = options
        .screenshot
        .clone()
        .map(|request| screenshot::Capture::new(request, std::sync::Arc::clone(&outcome)));

    eframe::run_native(
        title,
        native,
        Box::new(move |cc| {
            let app = app::App::new(&cc.egui_ctx, model);
            #[cfg(feature = "screenshot")]
            let app = match capture {
                Some(capture) => app.with_capture(capture),
                None => app,
            };
            Ok(Box::new(app))
        }),
    )
    .map_err(|e| RunError::Window(e.to_string()))?;

    #[cfg(feature = "screenshot")]
    if options.screenshot.is_some() {
        let result = outcome.lock().unwrap_or_else(|e| e.into_inner()).take();
        match result {
            Some(Ok(())) => {}
            Some(Err(e)) => return Err(RunError::Screenshot(e)),
            None => {
                return Err(RunError::Screenshot(
                    "the window closed before the screenshot was taken".into(),
                ));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn demo_mode_says_so_in_the_title() {
        assert_eq!(title(None), "Memory Tester");
        assert_eq!(title(Some(cli::Demo::Pass)), "Memory Tester (demo)");
    }
}
