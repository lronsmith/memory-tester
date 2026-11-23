//! Entry point: reads the command line and opens the window.

// Release builds on Windows are GUI programs: no console window opens.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod gui;

use std::process::ExitCode;

use gui::cli::{self, CliError};

fn main() -> ExitCode {
    install_panic_hook();
    // `args_os` rather than `args`: a non-UTF-8 argument must give a usage
    // message, not a panic.
    let args: Vec<String> = std::env::args_os()
        .skip(1)
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    let options = match cli::parse(&args) {
        Ok(options) => options,
        Err(CliError::Help) => {
            tell(&cli::usage(), false);
            return ExitCode::SUCCESS;
        }
        Err(CliError::Bad(message)) => {
            tell(
                &format!("memory-tester: {message}\n\n{}", cli::usage()),
                true,
            );
            return ExitCode::from(2);
        }
    };
    match gui::run(options) {
        Ok(()) => ExitCode::SUCCESS,
        Err(gui::RunError::Window(reason)) => {
            tell(
                &format!(
                    "Memory Tester could not open its window.\n\n{reason}\n\n\
                     Updating the graphics driver may help."
                ),
                true,
            );
            ExitCode::FAILURE
        }
        #[cfg(feature = "screenshot")]
        Err(gui::RunError::Screenshot(reason)) => {
            eprintln!("memory-tester: screenshot failed: {reason}");
            ExitCode::FAILURE
        }
    }
}

/// On a panic in the window's thread: ask a running test to stop, print the
/// usual panic message, and on a Windows release build (which has no
/// console) also say what happened in a message box. Panics in the engine's
/// own threads are left to the engine, which ends the run and reports it.
fn install_panic_hook() {
    let print = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let main_thread = std::thread::current().name() == Some("main");
        if main_thread {
            gui::stop_active_run();
        }
        print(info);
        #[cfg(all(windows, not(debug_assertions)))]
        if main_thread {
            message_box(&crash_message(&info.to_string()), true);
        }
    }));
}

/// The text shown when the program has to close because of an internal error.
#[cfg(any(all(windows, not(debug_assertions)), test))]
fn crash_message(detail: &str) -> String {
    format!(
        "Memory Tester hit an internal error and has to close. \
         Any test that was running has been stopped.\n\n{detail}"
    )
}

/// Shows a message to the person. It is printed; on Windows it also appears
/// in a message box, because a release build has no console to print to.
fn tell(message: &str, is_error: bool) {
    if is_error {
        eprintln!("{message}");
    } else {
        println!("{message}");
    }
    #[cfg(windows)]
    message_box(message, is_error);
}

#[cfg(windows)]
fn message_box(message: &str, is_error: bool) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        MB_ICONERROR, MB_ICONINFORMATION, MB_OK, MessageBoxW,
    };
    let wide = |s: &str| -> Vec<u16> { s.encode_utf16().chain(std::iter::once(0)).collect() };
    let text = wide(message);
    let caption = wide("Memory Tester");
    let icon = if is_error {
        MB_ICONERROR
    } else {
        MB_ICONINFORMATION
    };
    // SAFETY: both strings are NUL-terminated UTF-16 buffers that live until
    // the call returns; a null owner window is allowed.
    unsafe {
        MessageBoxW(
            std::ptr::null_mut(),
            text.as_ptr(),
            caption.as_ptr(),
            MB_OK | icon,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crash_message_says_the_test_stopped_and_includes_the_detail() {
        let text = crash_message("panicked at src/gui/app.rs:1:1: boom");
        assert!(text.starts_with("Memory Tester hit an internal error"));
        assert!(text.contains("has been stopped"));
        assert!(text.ends_with("panicked at src/gui/app.rs:1:1: boom"));
    }
}
