//! Deployment-level process cleanup, independent of agent behavior.
use anyhow::Result;
use tokio::process::{Child, Command};

/// fs2 preserves Windows ERROR_LOCK_VIOLATION, which is not WouldBlock.
pub(crate) fn lock_busy(error: &std::io::Error) -> bool {
    error.kind() == std::io::ErrorKind::WouldBlock
        || error
            .raw_os_error()
            .is_some_and(|code| Some(code) == fs2::lock_contended_error().raw_os_error())
}

pub fn prepare(command: &mut Command) {
    #[cfg(unix)]
    command.process_group(0);
    #[cfg(windows)]
    command.creation_flags(windows_sys::Win32::System::Threading::CREATE_SUSPENDED);
    #[cfg(not(any(unix, windows)))]
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
            use std::{mem, ptr};
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
                    mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                ) == 0
                    || AssignProcessToJobObject(job, handle as _) == 0
                {
                    let error = std::io::Error::last_os_error();
                    CloseHandle(job);
                    return Err(error.into());
                }
                let tree = Self { job: job as usize };
                resume_suspended(
                    child
                        .id()
                        .ok_or_else(|| anyhow::anyhow!("Child exited before resume"))?,
                )?;
                Ok(tree)
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

#[cfg(windows)]
fn resume_suspended(pid: u32) -> Result<()> {
    use windows_sys::Win32::{
        Foundation::*,
        System::{Diagnostics::ToolHelp::*, Threading::*},
    };
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
    anyhow::ensure!(
        snapshot != INVALID_HANDLE_VALUE,
        "Could not inspect the suspended child process"
    );
    let mut entry: THREADENTRY32 = unsafe { std::mem::zeroed() };
    entry.dwSize = std::mem::size_of::<THREADENTRY32>() as u32;
    let mut found = None;
    let mut available = unsafe { Thread32First(snapshot, &mut entry) } != 0;
    while available {
        if entry.th32OwnerProcessID == pid {
            found = Some(entry.th32ThreadID);
            break;
        }
        available = unsafe { Thread32Next(snapshot, &mut entry) } != 0;
    }
    unsafe {
        CloseHandle(snapshot);
    }
    let id = found
        .ok_or_else(|| anyhow::anyhow!("The suspended child process has no initial thread"))?;
    let thread = unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, id) };
    anyhow::ensure!(
        !thread.is_null(),
        "Could not open the suspended child thread"
    );
    let resumed = unsafe { ResumeThread(thread) };
    unsafe {
        CloseHandle(thread);
    }
    anyhow::ensure!(resumed == 1, "Could not resume the suspended child process");
    Ok(())
}
