//! Deployment-level process cleanup, independent of agent behavior.
use anyhow::Result;
use tokio::process::{Child, Command};

pub fn prepare(command: &mut Command) {
    #[cfg(unix)]
    command.process_group(0);
    #[cfg(not(unix))]
    let _ = command;
}

pub struct ProcessTree {
    #[cfg(unix)]
    pid: i32,
    #[cfg(windows)]
    job: usize,
}
impl ProcessTree {
    pub fn attach(child: &Child) -> Result<Self> {
        #[cfg(unix)]
        {
            Ok(Self {
                pid: child
                    .id()
                    .ok_or_else(|| anyhow::anyhow!("Harness exited during launch"))?
                    as i32,
            })
        }
        #[cfg(windows)]
        {
            use std::{mem, ptr, size_of};
            use windows_sys::Win32::{Foundation::CloseHandle, System::JobObjects::*};
            let handle = child
                .raw_handle()
                .ok_or_else(|| anyhow::anyhow!("Missing process handle"))?;
            // SAFETY: Windows receives valid structure pointers; the handle is owned by this guard.
            unsafe {
                let job = CreateJobObjectW(ptr::null(), ptr::null());
                anyhow::ensure!(
                    !job.is_null(),
                    "CreateJobObjectW failed: {}",
                    std::io::Error::last_os_error()
                );
                let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = mem::zeroed();
                info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                if SetInformationJobObject(
                    job,
                    JobObjectExtendedLimitInformation,
                    &info as *const _ as _,
                    size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                ) == 0
                    || AssignProcessToJobObject(job, handle as _) == 0
                {
                    let error = std::io::Error::last_os_error();
                    CloseHandle(job);
                    return Err(error.into());
                }
                Ok(Self { job: job as usize })
            }
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = child;
            Ok(Self {})
        }
    }
}
impl Drop for ProcessTree {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            // SAFETY: positive PID names the process group created for this specific harness.
            if self.pid > 0 {
                unsafe {
                    libc::kill(-self.pid, libc::SIGKILL);
                }
            }
        }
        #[cfg(windows)]
        {
            // SAFETY: this guard owns the job handle; closing it terminates its member processes.
            unsafe {
                windows_sys::Win32::Foundation::CloseHandle(self.job as _);
            }
        }
    }
}
