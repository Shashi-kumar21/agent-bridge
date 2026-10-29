//! Keeps agent processes from outliving the daemon, and tears an agent's
//! process tree down on request.
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
//!   group. [`ProcessReaper::terminate`] additionally `SIGKILL`s the agent's
//!   process group and the descendants it finds in a snapshot taken first (`ps`,
//!   or `/proc` on Linux when `ps` is unavailable), which covers the jobs an
//!   interactive shell moved into their own process group - but only while the
//!   client is still alive. On Linux the PTY slave is also swept through
//!   `/proc/<pid>/fd`, which keeps reaching the tree after the client has exited
//!   (the slave number cannot be reused while the daemon holds the master). A
//!   process that daemonized and closed the terminal, and the same post-exit
//!   case on macOS, is not reached; daemon death only reaches the foreground
//!   group. All of that is documented in `SECURITY.md`.

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
    /// Slave device of the agent's PTY (for example `/dev/pts/3`), captured
    /// while the direct client is still alive.
    ///
    /// Linux only: it is the anchor for the post-exit sweep. Once the client is
    /// gone its pid, process group, session and controlling terminal can all be
    /// reused or disassociated, while the slave number cannot be reused as long
    /// as this process holds the master.
    #[cfg(target_os = "linux")]
    pty: Option<String>,
}

/// The caller's last observation of the agent's direct client.
///
/// A `bool` reads the same at both call sites; this enum makes the hazard
/// explicit: once the leader is gone its pid may already name a stranger.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LeaderState {
    /// The client was still running when the caller checked.
    Alive,
    /// The client had exited (or could no longer be reaped by this process).
    Exited,
}

impl ProcessReaper {
    /// Kill the agent and the descendants it still owns.
    ///
    /// * **Windows** - drops the job handle. Since the job was created with
    ///   `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`, the kernel terminates every
    ///   process in it, including grandchildren that opened their own console.
    /// * **Unix** - `SIGKILL` to the agent's process group, then to every
    ///   descendant found in a `ps` snapshot taken before the signals. The child
    ///   is a session leader, so the group signal reaches the agent and the
    ///   processes that stayed in its group; the snapshot adds the jobs an
    ///   interactive shell moved into their own group (and children that called
    ///   `setsid`). On Linux a third pass kills every process that still holds
    ///   the agent's PTY slave open, which is the only anchor left once the
    ///   client has exited. A process that had already left the tree is *not*
    ///   reached - see `SECURITY.md`.
    ///
    /// `leader` is the caller's last liveness observation. On Unix it narrows
    /// the pid-reuse hazard: once the leader is gone its pid may have been
    /// recycled into an unrelated process group, so the group signal is skipped
    /// when a process owns that number again, and the descendant walk is skipped
    /// entirely (its root would be a stranger). This is best-effort, not a proof
    /// (the probe and the signals are not atomic); a complete fix needs `pidfd`
    /// or process enumeration. The PTY sweep is unaffected: it is anchored on a
    /// slave number the daemon still owns.
    ///
    /// Consumes the reaper: once the tree is gone there is nothing left to
    /// guard. Returns an error only when the signal could not be delivered.
    pub fn terminate(self, leader: LeaderState) -> Result<()> {
        #[cfg(windows)]
        {
            let _ = leader;
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
            //
            // If the leader is gone the id may have been recycled. On Linux a
            // pid number stays allocated while the agent's own group references
            // it, so `ESRCH` usually means that orphaned group (or nothing).
            // That narrows the hazard but does not prove it: an unrelated group
            // can reuse the id after its own leader exits, and the probe ->
            // killpg window is not atomic. `Ok`/`EPERM` means a process owns the
            // number, so the group signal is skipped. A complete fix needs pidfd
            // or enumeration.
            let group_is_stranger = leader == LeaderState::Exited
                && !matches!(
                    rustix::process::test_kill_process(group),
                    Err(rustix::io::Errno::SRCH)
                );
            // Snapshot the tree before signalling: once the agent dies its
            // children are reparented and can no longer be found. Skipped when
            // the leader is gone - its pid may name an unrelated tree by now.
            let descendants = if leader == LeaderState::Alive && !group_is_stranger {
                descendants(pid)
            } else {
                Vec::new()
            };
            let mut first_error = None;
            if !group_is_stranger {
                if let Err(err) =
                    rustix::process::kill_process_group(group, rustix::process::Signal::KILL)
                {
                    // The agent may have exited between the liveness check and
                    // the signal; a vanishing group is success, not a failure.
                    // Any other error (`EPERM` after a credential change, a
                    // sandbox) must not skip the sweep: the descendants are
                    // already snapshotted and killing them is still the best
                    // chance to avoid a leak.
                    if err != rustix::io::Errno::SRCH {
                        first_error = Some(BridgeError::Io(std::io::Error::from_raw_os_error(
                            err.raw_os_error(),
                        )));
                    }
                }
            }
            // Job-control jobs live in their own process group, so the group
            // signal above leaves them behind. The snapshot fixed the pid list,
            // so the order is not a correctness requirement; leaves first just
            // reads like a tree teardown. One unkillable pid (`EPERM`) must not
            // stop the rest of the tree from being signalled either.
            for descendant in descendants.into_iter().rev() {
                if let Err(err) = kill_member(descendant) {
                    first_error.get_or_insert(err);
                }
            }
            // The group and the parent chain are both gone once the direct
            // client exits (the kernel reparents its children and disassociates
            // the controlling terminal), so a job-control job would survive
            // them. The PTY slave is the one anchor that outlives the client:
            // its number stays ours while we hold the master, so every process
            // that still has it open on stdin/stdout/stderr belongs to this
            // agent. Two passes, so a process forked between the scan and the
            // signals is caught as well.
            #[cfg(target_os = "linux")]
            if let Some(pty) = &self.pty {
                // Both passes always run: the second catches a process forked
                // between the first scan and its signals.
                for _ in 0..2 {
                    for member in pty_members(pty) {
                        if let Err(err) = kill_member(member) {
                            first_error.get_or_insert(err);
                        }
                    }
                }
            }
            match first_error {
                Some(err) => Err(err),
                None => Ok(()),
            }
        }
    }
}

/// `pid -> children` map of the process table.
#[cfg(unix)]
type Children = std::collections::HashMap<u32, Vec<u32>>;

/// Every descendant of `root`, parents before children.
///
/// The tree comes from [`ps_children`] and, if `ps` is missing or failing, from
/// [`proc_children`] on Linux. When neither source works the snapshot is
/// skipped with a warning: only the process-group signal and the PTY sweep are
/// left. The snapshot-to-kill window is best-effort: a process forked
/// afterwards, one that had already been reparented, or a snapshotted pid that
/// exits and is recycled before the sweep is not handled - a complete fix needs
/// `pidfd` or process enumeration.
#[cfg(unix)]
fn descendants(root: u32) -> Vec<u32> {
    match ps_children() {
        Ok(children) => tree_from(&children, root),
        Err(reason) => {
            if let Some(children) = proc_children() {
                return tree_from(&children, root);
            }
            tracing::warn!(
                reason = %reason,
                "could not enumerate processes; the agent's descendants may survive kill --force"
            );
            Vec::new()
        }
    }
}

/// `(pid, ppid)` pairs from `ps`, keyed by parent.
///
/// `ps` is the portable way to enumerate processes without unsafe FFI or a
/// platform crate; it is part of POSIX and present on every target here. The
/// flags matter: `-axo` works on both procps and BSD `ps`, while the
/// equivalent `-Axo` makes procps reject `-x` ("must set personality to get -x
/// option") and the sweep silently finds nothing.
///
/// The `Err` carries why it failed so [`descendants`] can report it if the
/// Linux fallback fails too.
#[cfg(unix)]
fn ps_children() -> std::result::Result<Children, String> {
    let output = std::process::Command::new("ps")
        .args(["-axo", "pid=,ppid="])
        .output()
        .map_err(|err| format!("could not run ps: {err}"))?;
    if !output.status.success() {
        return Err(format!(
            "ps exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let mut children = Children::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let mut fields = line.split_whitespace();
        let (Some(child), Some(parent)) = (
            fields.next().and_then(|value| value.parse::<u32>().ok()),
            fields.next().and_then(|value| value.parse::<u32>().ok()),
        ) else {
            continue;
        };
        children.entry(parent).or_default().push(child);
    }
    Ok(children)
}

/// `(pid, ppid)` pairs from `/proc`, keyed by parent - the fallback when `ps`
/// cannot run.
///
/// Linux always mounts `/proc` for the daemon's own processes, so this works in
/// minimal containers that ship no procps. `None` means `/proc` itself could not
/// be read, which is the only failure worth reporting: individual entries can
/// disappear at any moment (the process exited), so those are skipped silently.
#[cfg(target_os = "linux")]
fn proc_children() -> Option<Children> {
    let entries = std::fs::read_dir("/proc").ok()?;
    let mut children = Children::new();
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        let Ok(stat) = std::fs::read_to_string(entry.path().join("stat")) else {
            continue;
        };
        // Field 2 is `comm` in parentheses and may itself contain spaces and
        // parentheses, so the numbers start after the *last* `)`: the first
        // token there is the state, the second the parent pid.
        let parent = stat
            .rsplit_once(')')
            .and_then(|(_, rest)| rest.split_whitespace().nth(1))
            .and_then(|value| value.parse::<u32>().ok());
        let Some(parent) = parent else {
            continue;
        };
        children.entry(parent).or_default().push(pid);
    }
    Some(children)
}

/// [`proc_children`] where the platform has no `/proc` (macOS).
#[cfg(all(unix, not(target_os = "linux")))]
fn proc_children() -> Option<Children> {
    None
}

/// Descendants of `root` in a `pid -> children` map, parents before children.
#[cfg(unix)]
fn tree_from(children: &Children, root: u32) -> Vec<u32> {
    let mut seen = std::collections::HashSet::from([root]);
    let mut found = Vec::new();
    let mut stack = vec![root];
    while let Some(pid) = stack.pop() {
        for child in children.get(&pid).into_iter().flatten().copied() {
            if seen.insert(child) {
                found.push(child);
                stack.push(child);
            }
        }
    }
    found
}

/// Slave device of the agent's PTY, read from the child's standard streams.
///
/// Must run while the direct client is alive: once it exits, its `/proc` entry
/// (and with it these fds) is gone. Only a `/dev/pts/N` slave is accepted,
/// because that is the path the sweep can match against other processes.
#[cfg(unix)]
pub fn pty_path(pid: Option<u32>) -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        let pid = pid?;
        let proc_dir = format!("/proc/{pid}");
        // `spawn_command` returns after `exec`, so the child's stdio already
        // points at the slave; the retry only covers a `/proc` that is briefly
        // unreadable. It is deliberately short: this runs while the daemon holds
        // the agent table lock.
        for _ in 0..10 {
            let mut readable = false;
            // Any of the three standard streams is enough: a program may close
            // or replace stdin, and the sweep matches all three.
            for fd in ["0", "1", "2"] {
                if let Ok(target) = std::fs::read_link(format!("{proc_dir}/fd/{fd}")) {
                    readable = true;
                    if let Some(path) = target.to_str().filter(|path| path.starts_with("/dev/pts/"))
                    {
                        return Some(path.to_string());
                    }
                }
            }
            if readable {
                tracing::warn!(
                    pid,
                    "the agent's stdio is not a pty; kill --force cannot sweep it after the client exits"
                );
                return None;
            }
            if !std::path::Path::new(&proc_dir).exists() {
                return None;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        tracing::warn!(
            pid,
            "could not read the agent's pty; kill --force cannot sweep it after the client exits"
        );
        None
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = pid;
        None
    }
}

/// Every process that still holds `pty` open on stdin, stdout or stderr.
///
/// Deliberately fd-based: the slave number cannot be reused while the daemon
/// holds the master, so a match proves the process inherited this agent's
/// terminal - unlike a pid, a process group, a session id or a controlling
/// terminal, none of which survive the client's exit reliably. A process that
/// closed or redirected its stdio no longer carries the anchor and is not found.
#[cfg(target_os = "linux")]
fn pty_members(pty: &str) -> Vec<u32> {
    let self_pid = std::process::id();
    let wanted = std::path::Path::new(pty);
    let Ok(entries) = std::fs::read_dir("/proc") else {
        tracing::warn!(
            "could not read /proc; processes holding the agent's pty may survive kill --force"
        );
        return Vec::new();
    };
    let mut found = Vec::new();
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        if pid == self_pid {
            continue;
        }
        for fd in ["0", "1", "2"] {
            let link = entry.path().join("fd").join(fd);
            if std::fs::read_link(&link).is_ok_and(|target| target == wanted) {
                found.push(pid);
                break;
            }
        }
    }
    found.sort_unstable();
    found
}

/// SIGKILL one pid, treating "already gone" as success.
///
/// The pid comes from a snapshot, so a process that exited and had its pid
/// recycled before this call is signalled too - the same best-effort window as
/// the group signal, not a proof of identity.
#[cfg(unix)]
fn kill_member(pid: u32) -> Result<()> {
    let Ok(raw) = i32::try_from(pid) else {
        return Ok(());
    };
    let Some(pid) = rustix::process::Pid::from_raw(raw) else {
        return Ok(());
    };
    match rustix::process::kill_process(pid, rustix::process::Signal::KILL) {
        Ok(()) | Err(rustix::io::Errno::SRCH) => Ok(()),
        Err(err) => Err(BridgeError::Io(std::io::Error::from_raw_os_error(
            err.raw_os_error(),
        ))),
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
/// process group, and on Linux the PTY slave so the tree can be swept after the
/// client exits.
#[cfg(unix)]
pub fn install(child: &dyn Child, pty: Option<&str>) -> Option<ProcessReaper> {
    #[cfg(target_os = "linux")]
    {
        Some(ProcessReaper {
            pid: child.process_id(),
            pty: pty.map(str::to_string),
        })
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = pty;
        Some(ProcessReaper {
            pid: child.process_id(),
        })
    }
}

/// Human readable description of the platform mechanism, printed by
/// `agent-bridge version` and quoted in `SECURITY.md`.
#[must_use]
pub const fn mechanism() -> &'static str {
    if cfg!(windows) {
        "windows job object (kill on job close)"
    } else if cfg!(target_os = "linux") {
        "pty hangup (SIGHUP) + SIGKILL to the agent group + best-effort tree snapshot + pty fd sweep"
    } else {
        "pty hangup (SIGHUP) + SIGKILL to the agent group + best-effort tree snapshot"
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::{descendants, kill_member, proc_children, ps_children};
    use std::sync::{Mutex, PoisonError};
    use std::time::{Duration, Instant};

    /// `PATH` is process-wide, so the test that empties it must not overlap with
    /// any other test in this binary that spawns a command.
    static PATH_LOCK: Mutex<()> = Mutex::new(());

    /// Clears `PATH` for [`descendants`] and restores it on drop, including when
    /// an assertion panics.
    struct EmptyPath(Option<std::ffi::OsString>);

    impl EmptyPath {
        fn new() -> Self {
            let previous = std::env::var_os("PATH");
            std::env::set_var("PATH", "");
            Self(previous)
        }
    }

    impl Drop for EmptyPath {
        fn drop(&mut self) {
            match self.0.take() {
                Some(path) => std::env::set_var("PATH", path),
                None => std::env::remove_var("PATH"),
            }
        }
    }

    /// Pid of the single child `parent` forked, waiting briefly for the fork.
    fn child_of(parent: u32) -> u32 {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if let Some(children) = proc_children() {
                if let Some(pid) = children.get(&parent).and_then(|pids| pids.first()) {
                    return *pid;
                }
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("{parent} did not fork a child within 5s");
    }

    /// The snapshot must not go empty in a container that ships no procps: with
    /// `ps` unreachable, `/proc` has to produce the same child.
    #[test]
    fn proc_fallback_finds_the_tree_when_ps_is_missing() {
        let _lock = PATH_LOCK.lock().unwrap_or_else(PoisonError::into_inner);

        // Spawn while `PATH` still works: the shell needs it to resolve `sleep`.
        let mut client = std::process::Command::new("/bin/sh")
            .args(["-c", "sleep 30 & wait"])
            .spawn()
            .expect("spawn /bin/sh");
        let root = client.id();
        let child = child_of(root);

        // Both sources agree while `ps` is reachable, so the fallback is the only
        // difference in the second half of the test.
        assert_eq!(
            ps_children().ok().and_then(|map| map.get(&root).cloned()),
            Some(vec![child]),
            "ps should see the same child"
        );

        let found = {
            let _empty = EmptyPath::new();
            assert!(
                ps_children().is_err(),
                "ps must be unreachable with an empty PATH"
            );
            descendants(root)
        };

        assert!(
            found.contains(&child),
            "the /proc fallback missed {child}: got {found:?}"
        );
        assert!(
            found
                .iter()
                .all(|pid| std::path::Path::new(&format!("/proc/{pid}")).exists()),
            "the snapshot contains a pid that is not alive: {found:?}"
        );

        for pid in found.iter().rev().chain(std::iter::once(&root)) {
            let _ = kill_member(*pid);
        }
        let _ = client.wait();
    }
}
