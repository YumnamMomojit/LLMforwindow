//! Owned process trees. Windows: Job Object with KILL_ON_JOB_CLOSE (children and
//! grandchildren die with the job, only ours are affected). Unix: a dedicated process group,
//! SIGKILLed when the `OwnedChild` is dropped unreaped; on Linux the direct child also gets
//! PR_SET_PDEATHSIG(SIGKILL), so it dies with semdok-edge even when semdok-edge is killed without running
//! its cleanup (its own children then see their pipes close). Other Unixes have no such
//! backstop: there the tree outlives a semdok-edge that dies without calling `kill_tree`.
//! Known gap (Windows): a grandchild spawned before the job assignment would escape; the
//! launcher is Python and spawns the engine seconds later, so the window is not exploitable
//! in practice, but it is not a guarantee. Not exercised on a real Windows host yet.
use tokio::process::{Child, Command};

pub struct OwnedChild {
    pub child: Child,
    #[cfg(windows)]
    job: win::Job,
}

impl OwnedChild {
    /// Spawn `cmd` as the root of an owned tree. On Linux, PR_SET_PDEATHSIG is tied to the
    /// *thread* that forks, so call this from a long-lived thread (the main thread or a
    /// runtime worker), never from a `spawn_blocking` thread that may retire.
    pub fn spawn(cmd: &mut Command) -> std::io::Result<Self> {
        cmd.kill_on_drop(true);
        #[cfg(unix)]
        cmd.process_group(0);
        #[cfg(target_os = "linux")]
        {
            let parent = std::process::id() as libc::pid_t;
            // SAFETY: the closure only makes async-signal-safe syscalls and does not allocate.
            unsafe {
                cmd.pre_exec(move || {
                    if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL as libc::c_ulong) != 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    // semdok-edge may have died between fork and prctl; then nothing would fire.
                    if libc::getppid() != parent {
                        return Err(std::io::Error::from_raw_os_error(libc::ESRCH));
                    }
                    Ok(())
                });
            }
        }
        #[cfg(windows)]
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        let child = cmd.spawn()?;
        #[cfg(windows)]
        {
            let job = win::Job::new()?;
            job.assign(&child)?;
            Ok(Self { child, job })
        }
        #[cfg(not(windows))]
        Ok(Self { child })
    }

    pub fn id(&self) -> Option<u32> {
        self.child.id()
    }

    /// Non-blocking exit check that reaps. tokio's `Child::id()` stays `Some` until something
    /// waits on the child, so this is the way to notice that the process is gone. Once the
    /// direct child has exited, whatever it left running in the tree is killed at once:
    /// nothing can reach a grandchild whose launcher is gone.
    pub fn poll_exit(&mut self) -> Option<std::process::ExitStatus> {
        #[cfg(unix)]
        let pgid = self.child.id();
        let status = self.child.try_wait().ok().flatten()?;
        #[cfg(unix)]
        if let Some(pgid) = pgid {
            // The group id stays taken while any member is alive; if none is, this is ESRCH.
            unsafe {
                libc::killpg(pgid as i32, libc::SIGKILL);
            }
        }
        #[cfg(windows)]
        self.job.terminate();
        Some(status)
    }

    /// Live processes in the tree, the direct child included: the members of its process
    /// group (Linux) or of its job (Windows). None where they cannot be counted: other Unixes,
    /// a direct child already reaped (Linux), or a failed query. A process that leaves the
    /// group or job on its own (a new session, a breakaway) is not counted.
    pub fn live_processes(&self) -> Option<usize> {
        #[cfg(target_os = "linux")]
        {
            group_members(self.child.id()?)
        }
        #[cfg(windows)]
        {
            self.job.active_processes()
        }
        #[cfg(not(any(target_os = "linux", windows)))]
        None
    }

    /// Whether the direct child has exited (Linux), including before `poll_exit` can reap it:
    /// a process whose main thread is gone shows as a zombie, and is counted as dead by
    /// `live_processes`, but it can only be waited for once its last thread has exited too.
    /// Some(true) once reaped. None where /proc cannot tell (other OSes, a failed read).
    pub fn has_exited(&self) -> Option<bool> {
        #[cfg(target_os = "linux")]
        {
            let Some(pid) = self.child.id() else {
                return Some(true);
            };
            match std::fs::read(format!("/proc/{pid}/stat")) {
                Ok(stat) => stat_fields(&stat).map(|(state, _)| !is_alive(state)),
                // Not reaped, so the entry stays while it is a zombie: gone means dead.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Some(true),
                Err(_) => None,
            }
        }
        #[cfg(not(target_os = "linux"))]
        None
    }

    /// SIGKILL the whole tree (Windows: terminate the job) without waiting. The direct child
    /// is left for `poll_exit` or `kill_tree` to reap.
    pub fn kill_tree_now(&self) {
        #[cfg(unix)]
        if let Some(pgid) = self.child.id() {
            unsafe {
                libc::killpg(pgid as i32, libc::SIGKILL);
            }
        }
        #[cfg(windows)]
        self.job.terminate();
    }

    /// Terminate the whole tree and reap the direct child.
    pub async fn kill_tree(&mut self) {
        #[cfg(unix)]
        if let Some(pid) = self.child.id() {
            unsafe {
                libc::killpg(pid as i32, libc::SIGTERM);
            }
            let _ =
                tokio::time::timeout(std::time::Duration::from_secs(5), self.child.wait()).await;
            // Whatever is left gets SIGKILL: all of it after the timeout, and a grandchild
            // that ignores SIGTERM even when the direct child exited on it. The group id
            // stays taken while any member is alive; if none is, this is ESRCH.
            unsafe {
                libc::killpg(pid as i32, libc::SIGKILL);
            }
        }
        #[cfg(windows)]
        self.job.terminate();
        let _ = self.child.start_kill();
        let _ = self.child.wait().await;
    }
}

/// Dropping the owner of a tree that was not reaped kills the whole tree, as closing the job
/// does on Windows, so a cancelled `kill_tree` or a replaced child cannot leave grandchildren
/// running (tokio's kill_on_drop only kills the direct child).
#[cfg(unix)]
impl Drop for OwnedChild {
    fn drop(&mut self) {
        self.kill_tree_now();
    }
}

/// Processes of the group `pgid` that are alive (not zombies), from /proc.
///
/// Entries that cannot be ours are skipped: a process gone since the listing, and one this
/// user may not look at (/proc mounted hidepid=noaccess, as systemd's ProtectProc=noaccess
/// does: another user's process, which a tree of ours does not hold; a member that ran a
/// setuid program or made itself undumpable would be hidden too, which upstream's launcher
/// and engine do not do). Any other error (out of file descriptors or memory, say) gives
/// None: a count that is too low would read as a dead engine.
#[cfg(target_os = "linux")]
fn group_members(pgid: u32) -> Option<usize> {
    let mut n = 0;
    for e in std::fs::read_dir("/proc").ok()? {
        let name = e.ok()?.file_name();
        let Some(pid) = name
            .to_str()
            .filter(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
        else {
            continue;
        };
        // Bytes, not a String: comm is the kernel's raw, 15-byte cut of the program name and
        // can end in the middle of a UTF-8 sequence.
        let stat = match std::fs::read(format!("/proc/{pid}/stat")) {
            Ok(s) => s,
            Err(e) if not_ours(&e) => continue,
            Err(_) => return None,
        };
        if stat_fields(&stat).is_some_and(|(state, pgrp)| pgrp == pgid && is_alive(state)) {
            n += 1;
        }
    }
    Some(n)
}

/// A /proc/<pid>/stat read failure that says the process is not one of ours.
#[cfg(target_os = "linux")]
fn not_ours(e: &std::io::Error) -> bool {
    matches!(
        e.raw_os_error(),
        Some(libc::ENOENT | libc::ESRCH | libc::EACCES | libc::EPERM)
    )
}

/// (state, process group) from the bytes of /proc/<pid>/stat: "pid (comm) state ppid pgrp ...",
/// where comm may hold spaces, ')' and bytes that are not UTF-8.
#[cfg(target_os = "linux")]
fn stat_fields(stat: &[u8]) -> Option<(u8, u32)> {
    let close = stat.iter().rposition(|&b| b == b')')?;
    let mut f = stat[close + 1..]
        .split(|b| b.is_ascii_whitespace())
        .filter(|f| !f.is_empty());
    let state = match f.next()? {
        [s] => *s,
        _ => return None,
    };
    let pgrp = std::str::from_utf8(f.nth(1)?).ok()?.parse().ok()?;
    Some((state, pgrp))
}

/// Not a zombie (Z) or dead (X, or x on old kernels).
#[cfg(target_os = "linux")]
fn is_alive(state: u8) -> bool {
    !matches!(state, b'Z' | b'X' | b'x')
}

#[cfg(windows)]
mod win {
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::JobObjects::*;
    pub struct Job(HANDLE);
    unsafe impl Send for Job {}
    impl Job {
        pub fn new() -> std::io::Result<Self> {
            unsafe {
                let h = CreateJobObjectW(std::ptr::null(), std::ptr::null());
                if h.is_null() {
                    return Err(std::io::Error::last_os_error());
                }
                let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
                info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                if SetInformationJobObject(
                    h,
                    JobObjectExtendedLimitInformation,
                    &info as *const _ as *const _,
                    std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                ) == 0
                {
                    let e = std::io::Error::last_os_error();
                    CloseHandle(h);
                    return Err(e);
                }
                Ok(Job(h))
            }
        }
        pub fn assign(&self, child: &tokio::process::Child) -> std::io::Result<()> {
            let h = child
                .raw_handle()
                .ok_or_else(|| std::io::Error::other("child already exited"))?;
            if unsafe { AssignProcessToJobObject(self.0, h as HANDLE) } == 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        }
        pub fn terminate(&self) {
            unsafe {
                TerminateJobObject(self.0, 1);
            }
        }
        /// Processes in the job now (nested jobs included).
        pub fn active_processes(&self) -> Option<usize> {
            unsafe {
                let mut info: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION = std::mem::zeroed();
                let ok = QueryInformationJobObject(
                    self.0,
                    JobObjectBasicAccountingInformation,
                    &mut info as *mut _ as *mut _,
                    std::mem::size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
                    std::ptr::null_mut(),
                );
                (ok != 0).then_some(info.ActiveProcesses as usize)
            }
        }
    }
    impl Drop for Job {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.0); // KILL_ON_JOB_CLOSE ends any survivors
            }
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn stat_with_a_comm_cut_inside_a_utf8_sequence_parses() {
        // 'ééééééééé' cut at 15 bytes by the kernel ends in a lone 0xC3.
        let mut stat = b"19605 (".to_vec();
        stat.extend_from_slice(&"é".repeat(7).into_bytes());
        stat.extend_from_slice(b"\xc3) Z 1 19604 19601 0 -1 4228108\n");
        assert!(std::str::from_utf8(&stat).is_err());
        assert_eq!(stat_fields(&stat), Some((b'Z', 19604)));
    }

    #[test]
    fn stat_with_spaces_and_parens_in_comm_parses() {
        let stat = b"42 (a) b (c)) S 1 4242 4242 0 -1 0\n";
        assert_eq!(stat_fields(stat), Some((b'S', 4242)));
        assert!(is_alive(b'S') && is_alive(b'D') && !is_alive(b'Z') && !is_alive(b'X'));
        assert_eq!(stat_fields(b"42 (x)"), None);
        assert_eq!(stat_fields(b"42 (x) S 1"), None);
        assert_eq!(stat_fields(b"42 x S 1 2"), None);
    }

    #[test]
    fn only_gone_or_hidden_entries_are_skipped() {
        use std::io::Error;
        for e in [libc::ENOENT, libc::ESRCH, libc::EACCES, libc::EPERM] {
            assert!(not_ours(&Error::from_raw_os_error(e)), "{e}");
        }
        for e in [libc::EMFILE, libc::ENFILE, libc::ENOMEM, libc::EIO] {
            assert!(!not_ours(&Error::from_raw_os_error(e)), "{e}");
        }
        assert!(!not_ours(&Error::from(std::io::ErrorKind::InvalidData)));
    }
}
