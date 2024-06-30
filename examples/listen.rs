extern crate clipboard_master;

use clipboard_master::{Master, ClipboardHandler, CallbackResult};

use std::io;

struct Handler;

impl ClipboardHandler for Handler {
    fn on_clipboard_change(&mut self) -> CallbackResult {
        println!("Clipboard change happened!");
        CallbackResult::Next
    }

    fn on_clipboard_error(&mut self, error: io::Error) -> CallbackResult {
        eprintln!("Error: {}", error);
        CallbackResult::Next
    }
}

fn main() {
    let mut master = Master::new(Handler).expect("create new monitor");

    let shutdown = master.shutdown_channel();
    std::thread::spawn(move || {
        std::thread::sleep(core::time::Duration::from_secs(30));
        println!("I did some work so time to finish...");
        shutdown.signal();
    });
    //Working until shutdown
    master.run().expect("Success");
}
