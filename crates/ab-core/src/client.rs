//! Blocking client used by the CLI and (later) the Tauri app.
//!
//! Each request opens a short-lived connection; `attach` keeps a single
//! connection open and consumes the event stream.

use std::io::{BufRead, BufReader, Write};

use interprocess::local_socket::Stream;

use crate::error::{BridgeError, Result};
use crate::paths;
use crate::protocol::{Event, Request, Response};
use crate::transport;

/// Send one request and wait for one response.
pub fn request(request: &Request) -> Result<Response> {
    let mut stream = transport::connect().map_err(|err| {
        if err.kind() == std::io::ErrorKind::NotFound
            || err.kind() == std::io::ErrorKind::ConnectionRefused
        {
            BridgeError::DaemonNotRunning(paths::socket_display())
        } else {
            BridgeError::Io(err)
        }
    })?;
    write_frame(&mut stream, request)?;
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    if reader.read_line(&mut line)? == 0 {
        return Err(BridgeError::Protocol(
            "daemon closed the connection".to_string(),
        ));
    }
    Ok(serde_json::from_str(line.trim())?)
}

/// `true` when a daemon answers a ping.
#[must_use]
pub fn is_running() -> bool {
    matches!(request(&Request::Ping), Ok(Response::Pong))
}

/// Streaming handle produced by [`Subscriber::attach`].
pub struct Subscriber {
    reader: BufReader<Stream>,
    last_seq: u64,
}

impl Subscriber {
    /// Subscribe to an agent's output.
    pub fn attach(target: &str, from_seq: Option<u64>) -> Result<Self> {
        let mut stream = transport::connect()
            .map_err(|_| BridgeError::DaemonNotRunning(paths::socket_display()))?;
        write_frame(
            &mut stream,
            &Request::Attach {
                target: target.to_string(),
                from_seq,
            },
        )?;
        Ok(Self {
            reader: BufReader::new(stream),
            last_seq: from_seq.unwrap_or(0),
        })
    }

    /// Next event, or `None` when the daemon closed the stream.
    ///
    /// Replayed and live output can carry the same `seq`, so duplicate frames
    /// are dropped here: callers always receive strictly increasing sequences.
    pub fn next_event(&mut self) -> Result<Option<Event>> {
        loop {
            let mut line = String::new();
            if self.reader.read_line(&mut line)? == 0 {
                return Ok(None);
            }
            let trimmed = line.trim();
            if let Ok(event) = serde_json::from_str::<Event>(trimmed) {
                if let Event::Output { seq, .. } = &event {
                    if *seq <= self.last_seq {
                        continue;
                    }
                    self.last_seq = *seq;
                }
                return Ok(Some(event));
            }
            if let Ok(Response::Error { code, message }) = serde_json::from_str::<Response>(trimmed)
            {
                return Err(BridgeError::Daemon(format!("{code:?}: {message}")));
            }
            return Err(BridgeError::Protocol(format!(
                "unexpected frame: {trimmed}"
            )));
        }
    }
}

/// Write one NDJSON frame.
pub fn write_frame<W: Write, T: serde::Serialize>(writer: &mut W, value: &T) -> Result<()> {
    let mut frame = serde_json::to_vec(value)?;
    frame.push(b'\n');
    writer.write_all(&frame)?;
    writer.flush()?;
    Ok(())
}
