//! Debug helper: dump raw PTY bytes for a command.
//!
//! `cargo run -p agent-bridge-core --example pty_dump -- cmd.exe /c echo hi`

use std::io::Read;

use portable_pty::{native_pty_system, CommandBuilder, PtySize};

fn main() {
    let mut args = std::env::args().skip(1);
    let program = args.next().unwrap_or_else(|| "cmd.exe".to_string());
    let rest: Vec<String> = args.collect();

    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("openpty");

    let mut builder = CommandBuilder::new(&program);
    if !rest.is_empty() {
        builder.args(&rest);
    }
    let mut child = pair.slave.spawn_command(builder).expect("spawn");
    drop(pair.slave);

    let master = pair.master;
    let _writer = master.take_writer().expect("writer");
    let mut reader = master.try_clone_reader().expect("reader");

    let mut buffer = [0_u8; 4096];
    for _ in 0..6 {
        match reader.read(&mut buffer) {
            Ok(0) => {
                println!("EOF");
                break;
            }
            Ok(size) => println!(
                "CHUNK {}",
                String::from_utf8_lossy(&buffer[..size]).escape_debug()
            ),
            Err(err) => {
                println!("ERR {err}");
                break;
            }
        }
    }
    let _ = child.wait();
}
