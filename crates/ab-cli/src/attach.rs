//! `agent-bridge attach` - mirror an agent terminal in the current one.

use std::io::{IsTerminal, Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use base64::Engine as _;
use crossterm::terminal;

use agent_bridge_core::client::{self, Subscriber};
use agent_bridge_core::protocol::{Event, Request};
use agent_bridge_core::{BridgeError, Result};

/// Attach to `target`, forwarding local keystrokes and printing its output.
pub fn run(target: &str, from_seq: Option<u64>) -> Result<()> {
    let interactive = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    let mut subscriber = Subscriber::attach(target, from_seq)?;
    let name = target.to_string();
    let done = Arc::new(AtomicBool::new(false));

    if interactive {
        terminal::enable_raw_mode().map_err(BridgeError::Io)?;
    }
    let guard = RawGuard { interactive };

    let printer_done = Arc::clone(&done);
    thread::spawn(move || {
        let mut stdout = std::io::stdout();
        loop {
            if printer_done.load(Ordering::SeqCst) {
                break;
            }
            match subscriber.next_event() {
                Ok(Some(Event::Output { data, .. })) => {
                    if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(data) {
                        let _ = stdout.write_all(&bytes);
                        let _ = stdout.flush();
                    }
                }
                Ok(Some(Event::Message { from, text, .. })) => {
                    let who = from.unwrap_or_else(|| "owner".to_string());
                    let _ = write!(
                        stdout,
                        "\r\n\x1b[2m[bridge]\x1b[0m message from {who}: {text}\r\n"
                    );
                    let _ = stdout.flush();
                }
                Ok(Some(Event::Exit { code, .. })) => {
                    let code = code.map_or_else(|| "-".to_string(), |value| value.to_string());
                    let _ = write!(
                        stdout,
                        "\r\n\x1b[2m[bridge] agent exited\x1b[0m (code {code})\r\n"
                    );
                    let _ = stdout.flush();
                    break;
                }
                Ok(Some(Event::Hello { .. } | Event::Heartbeat { .. })) => {}
                Ok(None) | Err(_) => break,
            }
        }
        printer_done.store(true, Ordering::SeqCst);
    });

    if interactive {
        spawn_resize_watcher(&name, Arc::clone(&done));
    }

    let mut stdin = std::io::stdin();
    let mut buffer = [0_u8; 1024];
    let mut detached = false;
    loop {
        match stdin.read(&mut buffer) {
            Ok(0) | Err(_) => break,
            Ok(size) => {
                let chunk = &buffer[..size];
                if interactive && chunk.contains(&0x1d) {
                    detached = true;
                    break;
                }
                let data = base64::engine::general_purpose::STANDARD.encode(chunk);
                let sent = client::request(&Request::Input {
                    target: name.clone(),
                    data,
                });
                if sent.is_err() {
                    break;
                }
            }
        }
    }

    done.store(true, Ordering::SeqCst);
    drop(guard);
    if detached {
        println!("\r\ndetached from {name}");
    }
    Ok(())
}

fn spawn_resize_watcher(target: &str, done: Arc<AtomicBool>) {
    let target = target.to_string();
    thread::spawn(move || {
        let mut last = terminal::size().ok();
        while !done.load(Ordering::SeqCst) {
            thread::sleep(Duration::from_millis(400));
            if let Ok(size) = terminal::size() {
                if Some(size) != last {
                    last = Some(size);
                    let _ = client::request(&Request::Resize {
                        target: target.clone(),
                        cols: size.0,
                        rows: size.1,
                    });
                }
            }
        }
    });
}

struct RawGuard {
    interactive: bool,
}

impl Drop for RawGuard {
    fn drop(&mut self) {
        if self.interactive {
            let _ = terminal::disable_raw_mode();
        }
    }
}
