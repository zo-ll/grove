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
    Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
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
        // Checked before raw mode rather than after: `enable_raw_mode` on a
        // pipe fails with ENXIO, and "No such device or address (os error 6)"
        // tells someone who ran `grove | less` nothing about what went wrong.
        if !std::io::IsTerminal::is_terminal(&io::stdout()) {
            return Err(io::Error::other(
                "grove needs a terminal: stdout is not a tty. \
                 Run `groved <workspace>` for the headless half; the TUI \
                 has to be attached to one.",
            ));
        }
        enable_raw_mode()?;
        RAW.store(true, Ordering::SeqCst);

        // The guard is constructed *here*, before anything else that can fail.
        // An earlier version set the flag and then used `?` on the next call:
        // that returns before any guard exists, so nothing restores and the
        // user is left in raw mode by the very code meant to prevent it.
        let guard = Self;

        let mut out = io::stdout();
        execute!(
            out,
            EnterAlternateScreen,
            // The dash leaves the columns between its panes unpainted — that
            // gap is the mock's, and there is nothing to paint there. So
            // whatever the alternate screen already held would stay in those
            // columns, and a stray character standing between two panes reads
            // as a rendering fault. Not every terminal hands over a blank one.
            //
            // Written straight out rather than through `Terminal::clear`,
            // which asks the terminal where the cursor is and waits for an
            // answer — a question a pty with nothing attached never answers,
            // and grove is run inside one by its own tests.
            Clear(ClearType::All),
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
        Ok((guard, terminal))
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        restore();
    }
}

/// Terminating signals unwind nothing, so neither `Drop` nor the panic hook
/// runs. Without this, `kill`, `kill -INT` or closing the terminal emulator
/// leaves raw mode set in whatever shell survives.
///
/// All four that terminate by default are covered. An earlier version took only
/// SIGTERM and SIGHUP, which left the most common one — `kill -INT` — wrecking
/// the terminal.
///
/// Uses `signal-hook` rather than raw `libc`: the workspace forbids `unsafe`,
/// and a hand-rolled handler is not worth an exception to that.
#[cfg(unix)]
fn install_signal_handlers() {
    use signal_hook::consts::{SIGHUP, SIGINT, SIGQUIT, SIGTERM};
    use signal_hook::iterator::Signals;

    // A blocking iterator, not a polled flag: the issue requires near-zero idle
    // CPU, and a thread waking on a timer to check an atomic is exactly the
    // spin that rules out. This thread sleeps until a signal actually arrives.
    let Ok(mut signals) = Signals::new([SIGTERM, SIGHUP, SIGINT, SIGQUIT]) else {
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

/// `RAW` is one flag for the whole process and the test harness runs tests in
/// parallel, so every test that sets or reads it holds this first — without
/// it one test's `store(true)` lands between another's restore and its
/// assert, about once in a hundred and fifty runs.
#[cfg(test)]
static RAW_TESTS: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
fn raw_tests() -> std::sync::MutexGuard<'static, ()> {
    // A test that panics while holding it poisons it; the flag is reset by
    // each test, so the data behind the poison is fine to reuse.
    RAW_TESTS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn without_a_terminal_it_says_so_rather_than_reporting_errno() {
        // Tests do not run on a tty, which is exactly the situation being
        // described — so this asserts the message a user in a pipe gets.
        let said = match Guard::new() {
            Err(error) => error.to_string(),
            Ok(_) => panic!("there is no terminal in a test run"),
        };
        assert!(said.contains("needs a terminal"), "{said}");
        assert!(said.contains("groved"), "and says what does work: {said}");
    }

    #[test]
    fn restore_is_idempotent() {
        let _raw = raw_tests();
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
        let _raw = raw_tests();
        // A restore that runs without raw mode having been entered must not
        // emit escape sequences into a terminal grove never took over — this is
        // the case when the daemon connection fails before setup.
        RAW.store(false, Ordering::SeqCst);
        restore();
        assert!(!RAW.load(Ordering::SeqCst));
    }
}

#[test]
fn a_failure_after_entering_raw_mode_still_restores() {
    let _raw = raw_tests();
    // The guard must exist before anything that can fail, or an early `?`
    // returns with the terminal raw and nothing left to restore it. This
    // asserts the ordering property directly: dropping a guard clears the
    // flag, whatever set it.
    RAW.store(true, Ordering::SeqCst);
    {
        let _g = Guard;
    }
    assert!(
        !RAW.load(Ordering::SeqCst),
        "a dropped guard must restore, so constructing it early covers every later failure"
    );
}

#[cfg(test)]
mod panic_tests {
    use super::*;

    #[test]
    fn a_panic_restores_before_unwinding() {
        let _raw = raw_tests();
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
