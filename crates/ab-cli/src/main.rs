//! `agent-bridge` command line interface.

mod attach;
mod fmt;

use std::io::Write;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use agent_bridge_core::client;
use agent_bridge_core::daemon::Daemon;
use agent_bridge_core::paths;
use agent_bridge_core::protocol::{
    DeliveryStatus, PasteMode, Request, Response, DEFAULT_COLS, DEFAULT_ROWS,
};
use agent_bridge_core::Result;
use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "agent-bridge",
    version,
    about = "Cross-platform session bus: list agents and message their terminals",
    long_about = None
)]
struct Cli {
    /// Emit raw JSON instead of human readable output.
    #[arg(long, global = true)]
    json: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Manage the background daemon.
    Daemon {
        #[command(subcommand)]
        action: Option<DaemonAction>,
    },
    /// List every agent managed by the daemon.
    #[command(
        visible_alias = "list",
        alias = "ls",
        alias = "agents",
        alias = "listagent"
    )]
    Listagents,
    /// Send a message into another agent's terminal.
    Sendmessage {
        /// Target agent name.
        target: String,
        /// Message body. Options must come before it; pass `--` first if the
        /// body itself starts with a dash.
        #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
        message: Vec<String>,
        /// Sender label recorded in the message log.
        #[arg(long)]
        from: Option<String>,
        /// Do not press Enter after typing the message.
        #[arg(long)]
        no_enter: bool,
        /// Maximum wait for a free queue slot (`0` = fail fast).
        #[arg(long, default_value = "5s", value_parser = parse_timeout)]
        queue_timeout: Duration,
        /// Maximum wait for the PTY write (`0` = wait forever).
        #[arg(long, default_value = "5s", value_parser = parse_timeout)]
        write_timeout: Duration,
        /// Return after the message is queued instead of waiting for the write.
        #[arg(long)]
        no_wait: bool,
        /// How line breaks are delivered: `auto` (bracket-paste multi-line text
        /// when the target supports it), `bracketed` (force the markers), or
        /// `plain` (send raw bytes; the documented fallback).
        #[arg(long, default_value = "auto", value_parser = parse_paste)]
        paste: PasteMode,
    },
    /// Spawn a new agent inside a daemon-owned PTY.
    New {
        /// Unique agent name.
        name: String,
        /// Working directory for the child process.
        #[arg(long)]
        cwd: Option<String>,
        /// PTY width.
        #[arg(long, default_value_t = DEFAULT_COLS)]
        cols: u16,
        /// PTY height.
        #[arg(long, default_value_t = DEFAULT_ROWS)]
        rows: u16,
        /// Command to run (defaults to the platform shell).
        #[arg(last = true)]
        command: Vec<String>,
    },
    /// Print the tail of an agent's output.
    Read {
        /// Agent name.
        target: String,
        /// Number of trailing lines.
        #[arg(long, default_value_t = 40)]
        lines: usize,
    },
    /// Terminate an agent.
    Kill {
        /// Agent name.
        name: String,
        /// Skip Ctrl-C and kill the agent's process tree immediately
        /// (Windows: whole job; Unix: process group + best-effort tree
        /// snapshot, plus a PTY fd sweep on Linux).
        #[arg(long)]
        force: bool,
    },
    /// Attach to an agent's terminal (Ctrl-] detaches).
    Attach {
        /// Agent name.
        target: String,
        /// Do not replay buffered output: pass the agent's current sequence
        /// number (`seq` in `read --json`).
        #[arg(long)]
        from_seq: Option<u64>,
    },
    /// Show the global message history.
    Log {
        /// Maximum entries.
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },
    /// Show the delivery status of one message.
    #[command(visible_alias = "msg-status")]
    Messagestatus {
        /// Message id printed by `sendmessage` or `log`.
        id: u64,
    },
    /// List messages that may still change: queued, writing or unknown.
    Pending {
        /// Agent name.
        target: String,
        /// Maximum entries.
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },
    /// Check whether the daemon answers.
    Ping,
    /// Print protocol and daemon details.
    Version,
}

#[derive(Debug, Subcommand)]
enum DaemonAction {
    /// Start the daemon in the background.
    Start,
    /// Run the daemon in the foreground.
    Run,
    /// Stop the background daemon.
    Stop,
    /// Restart the background daemon.
    Restart,
    /// Report whether the daemon is running.
    Status,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err}");
            ExitCode::FAILURE
        }
    }
}

#[allow(clippy::too_many_lines)]
fn run(cli: Cli) -> Result<()> {
    let json = cli.json;
    match cli.command {
        Command::Daemon { action } => daemon_action(action.unwrap_or(DaemonAction::Status), json),
        Command::Ping => {
            let response = client::request(&Request::Ping)?;
            emit(response, json)
        }
        Command::Version => {
            ensure_daemon(true)?;
            let response = client::request(&Request::Version)?;
            emit(response, json)
        }
        Command::Listagents => {
            ensure_daemon(true)?;
            let response = client::request(&Request::List)?;
            match (&response, json) {
                (Response::Agents { agents }, false) => {
                    fmt::print_agents(agents);
                    Ok(())
                }
                _ => emit(response, json),
            }
        }
        Command::Sendmessage {
            target,
            message,
            from,
            no_enter,
            queue_timeout,
            write_timeout,
            no_wait,
            paste,
        } => {
            ensure_daemon(true)?;
            let text = message.join(" ");
            let response = client::request(&Request::Send {
                target: target.clone(),
                message: text,
                enter: Some(!no_enter),
                from,
                queue_timeout_ms: Some(duration_millis(queue_timeout)),
                write_timeout_ms: Some(duration_millis(write_timeout)),
                wait: Some(!no_wait),
                paste: Some(paste),
            })?;
            let failure = match &response {
                Response::Sent {
                    message_id,
                    status: DeliveryStatus::Failed { detail, .. },
                    ..
                } => Some((*message_id, detail.clone())),
                _ => None,
            };
            if json {
                emit(response, json)?;
            } else if let Response::Sent {
                target,
                bytes,
                message_id,
                status,
            } = &response
            {
                if failure.is_none() {
                    if matches!(status, DeliveryStatus::Written) {
                        println!(
                            "sent msg {message_id} to {target} ({}, {bytes} bytes)",
                            fmt::status_line(status)
                        );
                    } else {
                        // `queued`/`unknown` have no confirmed byte count.
                        println!(
                            "sent msg {message_id} to {target} ({})",
                            fmt::status_line(status)
                        );
                    }
                }
            } else {
                emit(response, json)?;
            }
            match failure {
                Some((id, detail)) => {
                    Err(agent_bridge_core::BridgeError::DeliveryFailed { id, detail })
                }
                None => Ok(()),
            }
        }
        Command::New {
            name,
            cwd,
            cols,
            rows,
            command,
        } => {
            ensure_daemon(true)?;
            let argv = if command.is_empty() {
                None
            } else {
                Some(command)
            };
            let response = client::request(&Request::New {
                name,
                command: argv,
                cwd,
                cols: Some(cols),
                rows: Some(rows),
            })?;
            match (&response, json) {
                (Response::Agent { agent }, false) => {
                    fmt::print_agent(agent);
                    Ok(())
                }
                _ => emit(response, json),
            }
        }
        Command::Read { target, lines } => {
            ensure_daemon(true)?;
            let response = client::request(&Request::Read {
                target,
                lines: Some(lines),
            })?;
            match (&response, json) {
                (
                    Response::Output {
                        agent,
                        lines,
                        truncated,
                        ..
                    },
                    false,
                ) => {
                    fmt::print_output(agent, lines, *truncated);
                    Ok(())
                }
                _ => emit(response, json),
            }
        }
        Command::Kill { name, force } => {
            ensure_daemon(true)?;
            let response = client::request(&Request::Kill {
                name: name.clone(),
                force: Some(force),
            })?;
            match (&response, json) {
                (Response::Ok, false) => {
                    println!("killed {name}");
                    Ok(())
                }
                _ => emit(response, json),
            }
        }
        Command::Log { limit } => {
            ensure_daemon(true)?;
            let response = client::request(&Request::Log { limit: Some(limit) })?;
            match (&response, json) {
                (Response::Messages { messages }, false) => {
                    fmt::print_messages(messages, "no messages yet");
                    Ok(())
                }
                _ => emit(response, json),
            }
        }
        Command::Messagestatus { id } => {
            ensure_daemon(true)?;
            let response = client::request(&Request::MessageStatus { id })?;
            match (&response, json) {
                (Response::Message { message }, false) => {
                    fmt::print_message(message);
                    Ok(())
                }
                _ => emit(response, json),
            }
        }
        Command::Pending { target, limit } => {
            ensure_daemon(true)?;
            let response = client::request(&Request::Pending {
                target,
                limit: Some(limit),
            })?;
            match (&response, json) {
                (Response::Messages { messages }, false) => {
                    fmt::print_messages(messages, "no pending messages");
                    Ok(())
                }
                _ => emit(response, json),
            }
        }
        Command::Attach { target, from_seq } => {
            ensure_daemon(true)?;
            attach::run(&target, from_seq)
        }
    }
}

fn emit(response: Response, json: bool) -> Result<()> {
    if let Response::Error { message, .. } = &response {
        return Err(agent_bridge_core::BridgeError::Daemon(message.clone()));
    }
    if json {
        println!("{}", serde_json::to_string_pretty(&response)?);
        return Ok(());
    }
    match response {
        Response::Pong => println!("pong"),
        Response::Ok => println!("ok"),
        Response::Version {
            protocol,
            daemon,
            pid,
            socket,
            started_at,
        } => {
            println!("protocol : {protocol}");
            println!("daemon   : {daemon}");
            if pid == 0 {
                println!("pid      : unknown (daemon predates this field)");
            } else {
                println!("pid      : {pid}");
                println!("teardown : {}", agent_bridge_core::reaper::mechanism());
            }
            println!("socket   : {socket}");
            println!("uptime   : {}", fmt::human_age(started_at));
        }
        Response::Agents { agents } => fmt::print_agents(&agents),
        Response::Agent { agent } => fmt::print_agent(&agent),
        Response::Output {
            agent,
            lines,
            truncated,
            ..
        } => fmt::print_output(&agent, &lines, truncated),
        Response::Sent {
            target,
            bytes,
            message_id,
            status,
        } => println!(
            "sent msg {message_id} to {target} ({}, {bytes} bytes)",
            fmt::status_line(&status)
        ),
        Response::Message { message } => fmt::print_message(&message),
        Response::Messages { messages } => fmt::print_messages(&messages, "no messages yet"),
        Response::Error { .. } => unreachable!("handled above"),
    }
    Ok(())
}

#[allow(clippy::needless_pass_by_value)]
fn daemon_action(action: DaemonAction, json: bool) -> Result<()> {
    match action {
        DaemonAction::Run => {
            init_tracing();
            let socket = agent_bridge_core::transport::endpoint();
            println!("agent-bridge daemon listening on {socket}");
            let _ = std::io::stdout().flush();
            Daemon::new().run(|| {})?;
            Ok(())
        }
        DaemonAction::Start => {
            start_daemon()?;
            let response = client::request(&Request::Version)?;
            emit(response, json)
        }
        DaemonAction::Restart => {
            stop_daemon();
            start_daemon()?;
            let response = client::request(&Request::Version)?;
            emit(response, json)
        }
        DaemonAction::Stop => {
            stop_daemon();
            println!("daemon stopped");
            Ok(())
        }
        DaemonAction::Status => {
            if client::is_running() {
                let response = client::request(&Request::Version)?;
                emit(response, json)
            } else {
                println!("daemon is not running");
                Ok(())
            }
        }
    }
}

fn ensure_daemon(auto_start: bool) -> Result<()> {
    match client::request(&Request::Version) {
        Ok(Response::Version { protocol, .. }) => {
            if protocol == agent_bridge_core::protocol::PROTOCOL_VERSION {
                return Ok(());
            }
            // Never destroy live sessions implicitly: a version skew restart is
            // only allowed when the daemon has no agent sessions at all.
            let live = match client::request(&Request::List) {
                Ok(Response::Agents { agents }) => agents.len(),
                _ => 0,
            };
            if !skew_restart_is_safe(live) {
                return Err(agent_bridge_core::BridgeError::Daemon(format!(
                    "daemon protocol {protocol} does not match client {}, and {live} agent \
                     session(s) are still managed by it; run `agent-bridge daemon restart` \
                     when they are no longer needed",
                    agent_bridge_core::protocol::PROTOCOL_VERSION
                )));
            }
            eprintln!(
                "warning: daemon protocol {protocol} does not match client {}; restarting idle daemon",
                agent_bridge_core::protocol::PROTOCOL_VERSION
            );
            stop_daemon();
        }
        _ if !auto_start => {
            return Err(agent_bridge_core::BridgeError::DaemonNotRunning(
                paths::socket_display(),
            ));
        }
        _ => {}
    }
    start_daemon()
}

fn start_daemon() -> Result<()> {
    if client::is_running() {
        return Ok(());
    }
    paths::ensure_base_dir()?;
    let log = std::fs::File::create(paths::log_path())?;
    let stderr = log.try_clone()?;
    let exe = std::env::current_exe()?;

    let mut command = std::process::Command::new(exe);
    command
        .arg("daemon")
        .arg("run")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::from(log))
        .stderr(std::process::Stdio::from(stderr));

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP
        command.creation_flags(0x0800_0000 | 0x0000_0200);
    }

    command.spawn()?;

    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if client::is_running() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Err(agent_bridge_core::BridgeError::Daemon(format!(
        "daemon did not become ready; see {}",
        paths::log_path().display()
    )))
}

fn stop_daemon() {
    if client::is_running() {
        let _ = client::request(&Request::Shutdown);
        wait_until_down(Duration::from_secs(5));
    }
}

fn wait_until_down(timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if !client::is_running() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn init_tracing() {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init();
}

/// A version-skew restart is only safe when the daemon manages no sessions:
/// stopping the daemon kills every agent it owns.
fn skew_restart_is_safe(live_agents: usize) -> bool {
    live_agents == 0
}

/// Parse a CLI timeout: `500` / `500ms`, `5s`, `2m`; `0` means "no wait".
fn parse_paste(raw: &str) -> std::result::Result<PasteMode, String> {
    match raw.trim() {
        "auto" => Ok(PasteMode::Auto),
        "bracketed" => Ok(PasteMode::Bracketed),
        "plain" => Ok(PasteMode::Plain),
        other => Err(format!(
            "invalid paste mode '{other}' (use auto, bracketed or plain)"
        )),
    }
}

fn parse_timeout(raw: &str) -> std::result::Result<Duration, String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err("empty timeout (use e.g. 5s, 500ms, 0)".to_string());
    }
    let (number, multiplier) = if let Some(value) = raw.strip_suffix("ms") {
        (value, 1_u64)
    } else if let Some(value) = raw.strip_suffix('s') {
        (value, 1000)
    } else if let Some(value) = raw.strip_suffix('m') {
        (value, 60_000)
    } else {
        (raw, 1)
    };
    let value: u64 = number
        .trim()
        .parse()
        .map_err(|_| format!("invalid timeout '{raw}' (use e.g. 5s, 500ms, 0)"))?;
    Ok(Duration::from_millis(value.saturating_mul(multiplier))
        .min(agent_bridge_core::delivery::MAX_TIMEOUT))
}

fn duration_millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{duration_millis, parse_paste, parse_timeout, skew_restart_is_safe, Cli, Command};
    use agent_bridge_core::protocol::PasteMode;
    use clap::Parser;

    #[test]
    fn skew_restart_requires_idle_daemon() {
        assert!(skew_restart_is_safe(0));
        assert!(!skew_restart_is_safe(1));
        assert!(!skew_restart_is_safe(7));
    }

    #[test]
    fn parses_message_status_and_alias() {
        let cli = Cli::try_parse_from(["agent-bridge", "messagestatus", "42"]).expect("parse");
        assert!(matches!(cli.command, Command::Messagestatus { id: 42 }));

        let alias = Cli::try_parse_from(["agent-bridge", "msg-status", "7"]).expect("parse alias");
        assert!(matches!(alias.command, Command::Messagestatus { id: 7 }));
    }

    #[test]
    fn parses_pending_with_limit() {
        let cli =
            Cli::try_parse_from(["agent-bridge", "pending", "dev", "--limit", "5"]).expect("parse");
        assert!(matches!(
            cli.command,
            Command::Pending { target, limit: 5 } if target == "dev"
        ));
    }

    #[test]
    fn sendmessage_keeps_its_flags() {
        let cli = Cli::try_parse_from([
            "agent-bridge",
            "sendmessage",
            "dev",
            "--from",
            "qa",
            "--no-enter",
            "hello",
            "world",
        ])
        .expect("parse");
        match cli.command {
            Command::Sendmessage {
                target,
                message,
                from,
                no_enter,
                queue_timeout,
                write_timeout,
                no_wait,
                paste,
            } => {
                assert_eq!(target, "dev");
                assert_eq!(message, ["hello", "world"]);
                assert_eq!(from.as_deref(), Some("qa"));
                assert!(no_enter);
                assert_eq!(queue_timeout, Duration::from_secs(5));
                assert_eq!(write_timeout, Duration::from_secs(5));
                assert!(!no_wait);
                assert_eq!(paste, PasteMode::Auto);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn sendmessage_parses_timeouts_and_no_wait() {
        let cli = Cli::try_parse_from([
            "agent-bridge",
            "sendmessage",
            "dev",
            "--queue-timeout",
            "0",
            "--write-timeout",
            "250ms",
            "--no-wait",
            "hi",
        ])
        .expect("parse");
        match cli.command {
            Command::Sendmessage {
                queue_timeout,
                write_timeout,
                no_wait,
                paste,
                ..
            } => {
                assert_eq!(queue_timeout, Duration::ZERO);
                assert_eq!(write_timeout, Duration::from_millis(250));
                assert!(no_wait);
                assert_eq!(paste, PasteMode::Auto);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn timeouts_reject_garbage_and_clamp() {
        assert_eq!(parse_timeout("5s").expect("5s"), Duration::from_secs(5));
        assert_eq!(parse_timeout("2m").expect("2m"), Duration::from_secs(120));
        assert_eq!(
            parse_timeout("500").expect("plain ms"),
            Duration::from_millis(500)
        );
        assert!(parse_timeout("soon").is_err());
        assert_eq!(
            parse_timeout("999999m").expect("clamp"),
            agent_bridge_core::delivery::MAX_TIMEOUT
        );
        assert_eq!(duration_millis(Duration::from_millis(250)), 250);
    }

    #[test]
    fn paste_mode_parses_and_rejects_unknown_values() {
        assert_eq!(parse_paste("auto").expect("auto"), PasteMode::Auto);
        assert_eq!(
            parse_paste("bracketed").expect("bracketed"),
            PasteMode::Bracketed
        );
        assert_eq!(parse_paste(" plain ").expect("plain"), PasteMode::Plain);
        assert!(parse_paste("paste").is_err());

        let cli = Cli::try_parse_from([
            "agent-bridge",
            "sendmessage",
            "dev",
            "--paste",
            "plain",
            "hi",
        ])
        .expect("parse");
        assert!(matches!(
            cli.command,
            Command::Sendmessage {
                paste: PasteMode::Plain,
                ..
            }
        ));
    }
}
