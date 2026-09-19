//! The event loop's input side.
//!
//! Three sources feed the TUI — the keyboard, the daemon, and terminal
//! resizes — and the loop must not block on any one of them. Polling all three
//! in turn would spin; blocking on one would stall the others. Both matter
//! here: SPEC's dashboard sits idle with live terminals attached, so a busy
//! loop would burn a core doing nothing, while a loop blocked on the keyboard
//! would freeze the pane whose output is the reason to look at it.
//!
//! So each source gets a thread that blocks on its own descriptor and forwards
//! onto one channel. The loop blocks on that channel alone: it sleeps at zero
//! cost when nothing happens and wakes the instant any source produces.

use std::io;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread;
use std::time::Duration;

use grove_proto::Event as DaemonEvent;
use ratatui::crossterm::event::{self, Event as TermEvent};

/// Anything the loop can wake up for.
#[derive(Debug)]
pub enum Input {
    /// A key, mouse, paste or resize event from the terminal.
    Terminal(TermEvent),
    /// A message from the daemon.
    Daemon(DaemonEvent),
    /// The daemon connection ended. The TUI reports this rather than exiting
    /// silently, because the user's terminals are still running inside it.
    DaemonGone(String),
    /// The terminal's input stream ended or errored.
    TerminalGone(String),
}

/// Fans the three sources onto one channel.
pub struct Inputs {
    rx: Receiver<Input>,
    /// Kept so callers can add sources (the fake daemon does this in tests)
    /// without reaching into the channel's construction.
    tx: Sender<Input>,
}

impl Inputs {
    /// Start reading the terminal. The daemon source is attached separately,
    /// because a TUI that cannot reach a daemon still has to draw an error.
    pub fn new() -> Self {
        let (tx, rx) = channel();
        spawn_terminal_reader(tx.clone());
        Self { rx, tx }
    }

    /// A sender for an additional source, such as the daemon reader.
    pub fn sender(&self) -> Sender<Input> {
        self.tx.clone()
    }

    /// Block until something happens.
    ///
    /// Returns `None` only when every sender has been dropped, which means
    /// there is nothing left that could ever wake the loop.
    pub fn next(&self) -> Option<Input> {
        self.rx.recv().ok()
    }

    /// Drain anything already queued, without blocking.
    ///
    /// Used to coalesce a burst — a held-down arrow key, or a torrent of
    /// terminal output — into a single redraw instead of one per event.
    pub fn drain(&self) -> Vec<Input> {
        self.rx.try_iter().collect()
    }
}

impl Default for Inputs {
    fn default() -> Self {
        Self::new()
    }
}

fn spawn_terminal_reader(tx: Sender<Input>) {
    thread::spawn(move || {
        loop {
            // A long poll rather than a blocking read: `event::read` gives no
            // way to notice the channel has closed, so this wakes occasionally
            // to check whether anyone is still listening.
            match event::poll(Duration::from_millis(250)) {
                Ok(true) => match event::read() {
                    Ok(ev) => {
                        if tx.send(Input::Terminal(ev)).is_err() {
                            return;
                        }
                    }
                    Err(e) => {
                        let _ = tx.send(Input::TerminalGone(e.to_string()));
                        return;
                    }
                },
                // Nothing arrived within the poll window. Loop round; the
                // thread ends when a send fails, which is how it learns the
                // receiver is gone. Nothing is sent on this path, so an idle
                // grove produces no events and never redraws.
                Ok(false) => {}
                Err(e) => {
                    let _ = tx.send(Input::TerminalGone(e.to_string()));
                    return;
                }
            }
        }
    });
}

/// Read framed events off a connection and forward them.
///
/// Errors end the source rather than being retried: a framing error means the
/// stream is no longer trustworthy, and silently continuing would render
/// whatever garbage parses next.
pub fn spawn_daemon_reader<R>(mut read: R, tx: Sender<Input>)
where
    R: io::Read + Send + 'static,
{
    thread::spawn(move || {
        loop {
            match grove_proto::read_frame::<_, DaemonEvent>(&mut read) {
                Ok(ev) => {
                    if tx.send(Input::Daemon(ev)).is_err() {
                        return;
                    }
                }
                Err(e) => {
                    let _ = tx.send(Input::DaemonGone(e.to_string()));
                    return;
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use grove_proto::{PROTOCOL_VERSION, write_frame};

    #[test]
    fn daemon_events_reach_the_loop() {
        let mut buf = Vec::new();
        write_frame(
            &mut buf,
            &DaemonEvent::Welcome {
                version: PROTOCOL_VERSION,
            },
        )
        .unwrap();
        write_frame(
            &mut buf,
            &DaemonEvent::SessionEnded(grove_domain::SessionId("s".into())),
        )
        .unwrap();

        let (tx, rx) = channel();
        spawn_daemon_reader(io::Cursor::new(buf), tx);

        assert!(matches!(
            rx.recv().unwrap(),
            Input::Daemon(DaemonEvent::Welcome { .. })
        ));
        assert!(matches!(
            rx.recv().unwrap(),
            Input::Daemon(DaemonEvent::SessionEnded(_))
        ));
    }

    #[test]
    fn a_closed_connection_is_reported_not_silent() {
        // The user's terminals keep running inside a daemon the TUI can no
        // longer see, so vanishing without a word is the wrong failure.
        let (tx, rx) = channel();
        spawn_daemon_reader(io::Cursor::new(Vec::new()), tx);
        assert!(matches!(rx.recv().unwrap(), Input::DaemonGone(_)));
    }

    #[test]
    fn a_corrupt_stream_ends_the_source_rather_than_parsing_on() {
        // Past a framing error the stream cannot be trusted; continuing would
        // render whatever happens to parse next.
        let mut buf = 8u32.to_be_bytes().to_vec();
        buf.extend_from_slice(b"not json");
        let (tx, rx) = channel();
        spawn_daemon_reader(io::Cursor::new(buf), tx);
        assert!(matches!(rx.recv().unwrap(), Input::DaemonGone(_)));
        assert!(rx.recv().is_err(), "source must not keep sending");
    }
}
