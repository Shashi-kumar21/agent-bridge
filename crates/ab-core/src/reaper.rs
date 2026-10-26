//! Keeps agent processes from outliving the daemon, and takes a whole agent
//! tree down on request.
//!
//! A daemon that is killed with `SIGKILL` / `TerminateProcess` / Task Manager
//! never runs its shutdown path, so anything the daemon owns has to be reaped
//! by the OS:
//!
//! * **Windows** - every agent is assigned to an anonymous job object with
//!   `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`. When the last handle to that job
//!   closes (daemon exits, crashes, is terminated, or the handle is dropped by
//!   [`ProcessReaper::terminate`]), the kernel terminates every process in the
//!   job, including descendants the agent spawned.
//! * **Unix** - `portable-pty` makes the child a session leader with the PTY
//!   slave as its controlling terminal, so when the daemon dies the kernel
//!   closes the master and sends `SIGHUP` to the terminal's foreground process
//!   group. A background job that left that group can still survive a daemon
//!   crash; [`ProcessReaper::terminate`] reaches it by signalling the whole
//!   process group. A descendant that created its own session (`setsid`,
//!   `nohup`) is out of reach for both and is documented in `SECURITY.md`.

use portable_pty::Child;

#[cfg(unix)]
use crate::error::BridgeError;
use crate::Result;

/// Owner of the OS-level process lifetime link.
///
/// Keep it alive for as long as the agent should be allowed to run: dropping it
/// closes the job handle and (on Windows) kills the agent immediately.
#[derive(Debug)]
pub struct ProcessReaper {
    #[cfg(windows)]
    _job: win32job::Job,
    #[cfg(unix)]
    pid: Option<u32>,
}

impl ProcessReaper {
    /// Kill the agent and everything it spawned.
    ///
    /// * **Windows** - drops the job handle. Since the job was created with
    ///   `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`, the kernel terminates every
    ///   process in it, including grandchildren that opened their own console.
    /// * **Unix** - `SIGKILL` to the agent's process group. The child is a
    ///   session leader, so the group holds the shell plus everything it
    ///   started, foreground or background. Processes that created their own
    ///   session are out of reach; nothing portable can follow those.
    ///
    /// Consumes the reaper: once the tree is gone there is nothing left to
    /// guard. Returns an error only when the signal could not be delivered.
    pub fn terminate(self) -> Result<()> {
        #[cfg(windows)]
        {
            drop(self);
            Ok(())
        }
        #[cfg(unix)]
        {
            let Some(pid) = self.pid else {
                return Ok(());
            };
            let raw = i32::try_from(pid)
                .map_err(|_| BridgeError::Daemon(format!("agent pid {pid} does not fit in i32")))?;
            let Some(group) = rustix::process::Pid::from_raw(raw) else {
                return Ok(());
            };
            // `portable-pty` calls `setsid()` before exec, so the child's pid is
            // also its process group id.
            if let Err(err) =
                rustix::process::kill_process_group(group, rustix::process::Signal::KILL)
            {
                // The agent may have exited between the liveness check and the
                // signal; a vanishing group is success, not a failure.
                if err != rustix::io::Errno::SRCH {
                    return Err(BridgeError::Io(std::io::Error::from_raw_os_error(
                        err.raw_os_error(),
                    )));
                }
            }
            Ok(())
        }
    }
}

/// Tie `child` to the daemon's lifetime.
///
/// Returns `None` when the OS cannot provide the guarantee. That is a
/// degradation, not a failure: refusing to spawn the agent would be worse than
/// spawning it without the safety net, so the caller logs a warning instead.
#[cfg(windows)]
pub fn install(child: &dyn Child) -> Option<ProcessReaper> {
    use win32job::{ExtendedLimitInfo, Job};

    let Some(handle) = child.as_raw_handle() else {
        tracing::warn!("child process has no handle; it may outlive the daemon");
        return None;
    };

    let mut limits = ExtendedLimitInfo::new();
    limits.limit_kill_on_job_close();

    let job = match Job::create_with_limit_info(&limits) {
        Ok(job) => job,
        Err(err) => {
            tracing::warn!(error = %err, "could not create a job object; agent may outlive the daemon");
            return None;
        }
    };

    if let Err(err) = job.assign_process(handle as isize) {
        tracing::warn!(error = %err, "could not assign the agent to a job object; it may outlive the daemon");
        return None;
    }

    Some(ProcessReaper { _job: job })
}

/// Unix keeps the daemon-death guarantee through the PTY (see the module
/// docs); the pid is recorded so [`ProcessReaper::terminate`] can signal the
/// process group.
#[cfg(unix)]
pub fn install(child: &dyn Child) -> Option<ProcessReaper> {
    Some(ProcessReaper {
        pid: child.process_id(),
    })
}

/// Human readable description of the platform mechanism, printed by
/// `agent-bridge version` and quoted in `SECURITY.md`.
#[must_use]
pub const fn mechanism() -> &'static str {
    if cfg!(windows) {
        "windows job object (kill on job close)"
    } else {
        "pty hangup (SIGHUP on master close)"
    }
}
