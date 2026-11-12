//! Terminal formatting helpers (human readable output).

use std::fmt::Write as _;

use agent_bridge_core::protocol::{AgentInfo, AgentStatus, DeliveryStatus, Message};

/// Format a timestamp as a coarse age ("4s ago").
#[must_use]
pub fn human_age(at_ms: u64) -> String {
    let now = agent_bridge_core::now_ms();
    let delta = now.saturating_sub(at_ms) / 1000;
    match delta {
        0 => "now".to_string(),
        1..=59 => format!("{delta}s ago"),
        60..=3599 => format!("{}m ago", delta / 60),
        3600..=86_399 => format!("{}h ago", delta / 3600),
        _ => format!("{}d ago", delta / 86_400),
    }
}

/// Print the agent table.
pub fn print_agents(agents: &[AgentInfo]) {
    if agents.is_empty() {
        println!("no agents. create one with: agent-bridge new <name>");
        return;
    }
    let mut rows: Vec<[String; 7]> = vec![[
        "NAME".to_string(),
        "STATUS".to_string(),
        "PID".to_string(),
        "MSGS".to_string(),
        "QUEUED".to_string(),
        "LAST OUTPUT".to_string(),
        "COMMAND".to_string(),
    ]];
    for agent in agents {
        rows.push([
            agent.name.clone(),
            match agent.status {
                AgentStatus::Running => "running".to_string(),
                AgentStatus::Exited => "exited".to_string(),
            },
            agent
                .pid
                .map_or_else(|| "-".to_string(), |pid| pid.to_string()),
            agent.message_count.to_string(),
            agent.queued.to_string(),
            agent
                .last_output_at
                .map_or_else(|| "-".to_string(), human_age),
            agent.command.clone(),
        ]);
    }
    print_table(&rows);
}

/// Print a single agent in detail.
pub fn print_agent(agent: &AgentInfo) {
    println!("name       : {}", agent.name);
    println!("id         : {}", agent.id);
    println!(
        "status     : {}",
        match agent.status {
            AgentStatus::Running => "running",
            AgentStatus::Exited => "exited",
        }
    );
    println!(
        "pid        : {}",
        agent
            .pid
            .map_or_else(|| "-".to_string(), |pid| pid.to_string())
    );
    println!("command    : {}", agent.command);
    println!("cwd        : {}", agent.cwd);
    println!("size       : {}x{}", agent.cols, agent.rows);
    println!("created    : {}", human_age(agent.created_at));
    if let Some(code) = agent.exit_code {
        println!("exit code  : {code}");
    }
    println!("messages   : {}", agent.message_count);
    println!("queued     : {}", agent.queued);
    println!(
        "paste      : {}",
        if agent.bracketed_paste {
            "bracketed"
        } else {
            "plain"
        }
    );
    println!("subscribers: {}", agent.subscribers);
}

/// Print buffered output lines.
pub fn print_output(agent: &str, lines: &[String], truncated: bool) {
    if truncated {
        println!("--- {agent} (older output rotated out) ---");
    }
    for line in lines {
        println!("{line}");
    }
}

/// Print one message record and its delivery status.
pub fn print_message(message: &Message) {
    let from = message.from.as_deref().unwrap_or("owner");
    println!("id       : {}", message.id);
    println!("target   : {}", message.target);
    println!("from     : {from}");
    println!("status   : {}", status_line(&message.status));
    println!("bytes    : {}", message.bytes);
    println!("queued   : {}", human_age(message.at));
    println!("updated  : {}", human_age(message.updated_at));
    if let Some(ack) = &message.ack {
        println!("acked    : {}", human_age(ack.at));
        if let Some(note) = &ack.note {
            println!("ack note : {note}");
        }
    }
    println!("text     : {}", message.text);
}

/// `label (detail)` for a delivery status.
#[must_use]
pub fn status_line(status: &DeliveryStatus) -> String {
    match status {
        DeliveryStatus::Failed { code, detail } => {
            format!("failed ({}): {detail}", code.label())
        }
        other => other.label().to_string(),
    }
}

/// Print the global message log.
pub fn print_messages(messages: &[Message], empty: &str) {
    if messages.is_empty() {
        println!("{empty}");
        return;
    }
    for message in messages {
        let from = message.from.as_deref().unwrap_or("owner");
        let status = match &message.status {
            DeliveryStatus::Failed { code, .. } => format!("failed ({})", code.label()),
            other => other.label().to_string(),
        };
        println!(
            "#{} {:<8} {from} -> {}: {}",
            message.id, status, message.target, message.text
        );
    }
}

fn print_table<const N: usize>(rows: &[[String; N]]) {
    let mut widths = [0_usize; N];
    for row in rows {
        for (index, cell) in row.iter().enumerate() {
            widths[index] = widths[index].max(cell.chars().count());
        }
    }
    for (row_index, row) in rows.iter().enumerate() {
        let mut line = String::new();
        for (index, cell) in row.iter().enumerate() {
            if index + 1 == row.len() {
                line.push_str(cell);
            } else {
                let _ = write!(line, "{cell:<width$}  ", width = widths[index]);
            }
        }
        println!("{}", line.trim_end());
        if row_index == 0 {
            let rule: usize = widths.iter().sum::<usize>() + widths.len() * 2;
            println!("{}", "-".repeat(rule.min(120)));
        }
    }
}
