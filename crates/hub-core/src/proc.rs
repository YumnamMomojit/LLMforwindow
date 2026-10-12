//! Owned process trees. Windows: Job Object with KILL_ON_JOB_CLOSE (children and
//! grandchildren die with the job, only ours are affected). Unix: a dedicated process group.
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
    pub fn spawn(cmd: &mut Command) -> std::io::Result<Self> {
        cmd.kill_on_drop(true);
        #[cfg(unix)]
        cmd.process_group(0);
        #[cfg(windows)]
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        let child = cmd.spawn()?;
        #[cfg(windows)]
        {
            let job = win::Job::new()?;
            job.assign(&child)?;
            return Ok(Self { child, job });
        }
        #[cfg(not(windows))]
        Ok(Self { child })
    }

    pub fn id(&self) -> Option<u32> {
        self.child.id()
    }

    /// Terminate the whole tree and reap the direct child.
    pub async fn kill_tree(&mut self) {
        #[cfg(unix)]
        if let Some(pid) = self.child.id() {
            unsafe {
                libc::killpg(pid as i32, libc::SIGTERM);
            }
            if tokio::time::timeout(std::time::Duration::from_secs(5), self.child.wait())
                .await
                .is_err()
            {
                unsafe {
                    libc::killpg(pid as i32, libc::SIGKILL);
                }
            }
        }
        #[cfg(windows)]
        self.job.terminate();
        let _ = self.child.start_kill();
        let _ = self.child.wait().await;
    }
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
    }
    impl Drop for Job {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.0); // KILL_ON_JOB_CLOSE ends any survivors
            }
        }
    }
}
