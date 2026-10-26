//! `agent-bridge` command line interface.

mod attach;
mod fmt;

use std::io::Write;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use agent_bridge_core::client;
use agent_bridge_core::daemon::Daemon;
use agent_bridge_core::paths;
use agent_bridge_core::protocol::{Request, Response, DEFAULT_COLS, DEFAULT_ROWS};
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
        /// Message body. Pass `--` first if it starts with a dash.
        #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
        message: Vec<String>,
        /// Sender label recorded in the message log.
        #[arg(long)]
        from: Option<String>,
        /// Do not press Enter after typing the message.
        #[arg(long)]
        no_enter: bool,
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
        /// Skip Ctrl-C and kill immediately.
        #[arg(long)]
        force: bool,
    },
    /// Attach to an agent's terminal (Ctrl-] detaches).
    Attach {
        /// Agent name.
        target: String,
        /// Skip output produced before this sequence number.
        #[arg(long)]
        from_seq: Option<u64>,
    },
    /// Show the global message history.
    Log {
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
        } => {
            ensure_daemon(true)?;
            let text = message.join(" ");
            let response = client::request(&Request::Send {
                target: target.clone(),
                message: text,
                enter: Some(!no_enter),
                from,
            })?;
            match (&response, json) {
                (Response::Sent { bytes, .. }, false) => {
                    println!("sent {bytes} bytes to {target}");
                    Ok(())
                }
                _ => emit(response, json),
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
                    fmt::print_messages(messages);
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
        Response::Sent { target, bytes } => println!("sent {bytes} bytes to {target}"),
        Response::Messages { messages } => fmt::print_messages(&messages),
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

#[cfg(test)]
mod tests {
    use super::skew_restart_is_safe;

    #[test]
    fn skew_restart_requires_idle_daemon() {
        assert!(skew_restart_is_safe(0));
        assert!(!skew_restart_is_safe(1));
        assert!(!skew_restart_is_safe(7));
    }
}
