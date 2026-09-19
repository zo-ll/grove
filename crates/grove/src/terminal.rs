//! Terminal setup and teardown.
//!
//! The one invariant: **the terminal is restored no matter how we leave**.
//! Grove puts the terminal in raw mode on the alternate screen, and a process
//! that exits without undoing that leaves the user with no echo, no line
//! editing and no visible cursor — recoverable only by `reset` or a new shell.
//! A panic in a rendering path is not an unlikely way to exit, so restoration
//! cannot live only on the happy path.
//!
//! Three mechanisms, because no single one covers every exit:
//!
//! - [`Guard`]'s `Drop` covers normal returns and unwinding panics.
//! - The panic hook restores *before* the default hook prints, so the message
//!   lands on a usable terminal instead of a raw-mode one that smears it
//!   diagonally down the screen.
//! - The signal handler covers SIGTERM and SIGHUP, which unwind nothing at all.
//!
//! Restoration is idempotent, since more than one of these can fire.

use std::io::{self, Stdout, Write};
use std::sync::atomic::{AtomicBool, Ordering};

use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
};
use ratatui::crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::crossterm::{cursor, execute};

/// Set once the terminal has been put into raw mode, cleared once restored.
/// Consulted by every restoration path so that running twice is harmless.
static RAW: AtomicBool = AtomicBool::new(false);

/// Restore the terminal if it is currently raw. Safe to call any number of
/// times, from a panic hook, a signal handler, or `Drop`.
pub fn restore() {
    if !RAW.swap(false, Ordering::SeqCst) {
        return;
    }
    let mut out = io::stdout();
    // Best-effort: if stdout is gone there is nothing left to restore, and
    // failing here would mask whatever is actually killing the process.
    let _ = execute!(
        out,
        DisableBracketedPaste,
        DisableMouseCapture,
        LeaveAlternateScreen,
        cursor::Show
    );
    let _ = disable_raw_mode();
    let _ = out.flush();
}

/// Owns the terminal's raw state for the lifetime of the program.
pub struct Guard;

impl Guard {
    /// Enter raw mode and install the panic hook and signal handlers.
    pub fn new() -> io::Result<(Self, Terminal<CrosstermBackend<Stdout>>)> {
        enable_raw_mode()?;
        RAW.store(true, Ordering::SeqCst);

        let mut out = io::stdout();
        execute!(
            out,
            EnterAlternateScreen,
            EnableMouseCapture,
            EnableBracketedPaste
        )?;

        // Restore before the default hook prints, so the panic message is
        // readable rather than stair-stepped across a raw-mode screen.
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            restore();
            previous(info);
        }));

        install_signal_handlers();

        let terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
        Ok((Self, terminal))
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        restore();
    }
}

/// SIGTERM and SIGHUP unwind nothing, so neither `Drop` nor the panic hook
/// runs. Without this, `kill` or closing the terminal emulator leaves raw mode
/// set in whatever shell survives.
///
/// Uses `signal-hook` rather than raw `libc`: the workspace forbids `unsafe`,
/// and a hand-rolled handler is not worth an exception to that.
#[cfg(unix)]
fn install_signal_handlers() {
    use signal_hook::consts::{SIGHUP, SIGTERM};
    use signal_hook::iterator::Signals;

    // A blocking iterator, not a polled flag: the issue requires near-zero idle
    // CPU, and a thread waking on a timer to check an atomic is exactly the
    // spin that rules out. This thread sleeps until a signal actually arrives.
    let Ok(mut signals) = Signals::new([SIGTERM, SIGHUP]) else {
        // Registration failing is not worth aborting over — Drop and the panic
        // hook still cover every exit that unwinds.
        return;
    };
    std::thread::spawn(move || {
        if let Some(sig) = signals.forever().next() {
            restore();
            // The conventional status for a signal. Exiting rather than
            // re-raising keeps this free of `unsafe`, which the workspace
            // forbids; the cost is that the parent sees an exit code instead of
            // a signalled death.
            std::process::exit(128 + sig);
        }
    });
}

#[cfg(not(unix))]
fn install_signal_handlers() {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restore_is_idempotent() {
        // Several paths can fire — Drop, the panic hook, a signal — and more
        // than one may fire for a single exit. Running twice must be harmless.
        RAW.store(true, Ordering::SeqCst);
        restore();
        assert!(!RAW.load(Ordering::SeqCst));
        restore();
        assert!(!RAW.load(Ordering::SeqCst));
    }

    #[test]
    fn restore_is_a_noop_when_not_raw() {
        // A restore that runs without raw mode having been entered must not
        // emit escape sequences into a terminal grove never took over — this is
        // the case when the daemon connection fails before setup.
        RAW.store(false, Ordering::SeqCst);
        restore();
        assert!(!RAW.load(Ordering::SeqCst));
    }
}

#[cfg(test)]
mod panic_tests {
    use super::*;

    #[test]
    fn a_panic_restores_before_unwinding() {
        // The hook must run restore() before the default hook prints, or the
        // panic message lands on a raw-mode screen and stair-steps across it.
        RAW.store(true, Ordering::SeqCst);
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            restore();
            previous(info);
        }));
        let result = std::panic::catch_unwind(|| panic!("boom"));
        assert!(result.is_err());
        assert!(
            !RAW.load(Ordering::SeqCst),
            "terminal must be restored by the hook"
        );
        let _ = std::panic::take_hook();
    }
}
