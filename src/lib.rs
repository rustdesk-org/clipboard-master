//! Clipboard master
//!
//! Provides simple way to track updates of clipboard.
//!
//! ## Example:
//!
//! ```rust
//! extern crate clipboard_master;
//!
//! use clipboard_master::{Master, ClipboardHandler, CallbackResult};
//!
//! use std::io;
//!
//! struct Handler;
//!
//! impl ClipboardHandler for Handler {
//!     fn on_clipboard_change(&mut self) -> CallbackResult {
//!         println!("Clipboard change happened!");
//!         CallbackResult::Next
//!     }
//!
//!     fn on_clipboard_error(&mut self, error: io::Error) -> CallbackResult {
//!         eprintln!("Error: {}", error);
//!         CallbackResult::Next
//!     }
//! }
//!
//! fn main() {
//!     let mut master = Master::new(Handler).expect("create new monitor");
//!
//!     let shutdown = master.shutdown_channel();
//!     std::thread::spawn(move || {
//!         std::thread::sleep(core::time::Duration::from_secs(1));
//!         println!("I did some work so time to finish...");
//!         shutdown.signal();
//!     });
//!     //Working until shutdown
//!     master.run().expect("Success");
//! }
//! ```

#![cfg_attr(feature = "cargo-clippy", allow(clippy::style))]
#![cfg_attr(rustfmt, rustfmt_skip)]

use std::io;

mod master;
pub use master::{Master, Shutdown};

///Describes Clipboard handler
pub trait ClipboardHandler {
    ///Callback to call on clipboard change.
    fn on_clipboard_change(&mut self) -> CallbackResult;

    ///Called once after the X11 or Wayland backend subscribes to clipboard changes,
    ///even when the clipboard is empty, and before the first change callback.
    ///Other backends do not emit this notification.
    fn on_clipboard_ready(&mut self) {}

    ///Called when Wayland announces an existing selection at listener startup.
    ///Defaults to a normal change notification for backwards compatibility.
    ///This callback is not guaranteed on startup and is not a readiness signal.
    ///An empty initial selection produces no callback. An initial selection batched
    ///with a later change is reported through `on_clipboard_change()` instead.
    fn on_clipboard_initial_selection(&mut self) -> CallbackResult {
        self.on_clipboard_change()
    }

    ///Callback to call on when error happens in master.
    fn on_clipboard_error(&mut self, error: io::Error) -> CallbackResult {
        CallbackResult::StopWithError(error)
    }

    #[inline(always)]
    ///Returns sleep interval for polling implementations (e.g. Mac).
    ///
    ///Default value is 500ms
    fn sleep_interval(&self) -> core::time::Duration {
        core::time::Duration::from_millis(500)
    }
}

///Possible return values of callback.
pub enum CallbackResult {
    ///Wait for next clipboard change.
    Next,
    ///Stop handling messages.
    Stop,
    ///Special variant to propagate IO Error from callback.
    StopWithError(io::Error)
}

impl Shutdown {
    ///Signals shutdown
    pub fn signal(self) {
        drop(self);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initial_selection_defaults_to_clipboard_change() {
        struct Handler {
            called: bool,
        }
        impl ClipboardHandler for Handler {
            fn on_clipboard_change(&mut self) -> CallbackResult {
                self.called = true;
                CallbackResult::Stop
            }
        }
        let mut handler = Handler { called: false };
        assert!(matches!(
            handler.on_clipboard_initial_selection(),
            CallbackResult::Stop
        ));
        assert!(handler.called);
    }
}
