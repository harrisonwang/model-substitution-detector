//! 采集进程的生命周期：退出或取消时一并回收后代进程。

use anyhow::{Context, Result};
use std::process::{Child, Command};

pub(crate) struct ChildProcess {
    pub child: Child,
    terminated: bool,
    #[cfg(windows)]
    job: Job,
}

impl ChildProcess {
    pub fn spawn(mut command: Command) -> Result<Self> {
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        #[cfg(windows)]
        let job = {
            use std::os::windows::process::CommandExt;
            use windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;
            command.creation_flags(CREATE_NO_WINDOW);
            Job::new()?
        };
        let child = command.spawn().context("failed to start CLI")?;
        let process = Self {
            child,
            terminated: false,
            #[cfg(windows)]
            job,
        };
        #[cfg(windows)]
        process.job.assign(&process.child)?;
        // 所有平台均在传递提示词前建立进程组。
        Ok(process)
    }

    pub fn kill(&mut self) {
        if self.terminated {
            return;
        }
        self.terminated = true;
        #[cfg(unix)]
        {
            unsafe extern "C" {
                fn kill(pid: i32, signal: i32) -> i32;
            }
            if let Ok(pid) = i32::try_from(self.child.id()) {
                // 仅指向由 process_group(0) 创建的独立进程组。
                unsafe { kill(-pid, 9) };
            }
        }
        #[cfg(windows)]
        self.job.terminate();
        let _ = self.child.kill();
    }
}

impl Drop for ChildProcess {
    fn drop(&mut self) {
        self.kill();
        let _ = self.child.wait();
    }
}

#[cfg(windows)]
struct Job(std::os::windows::io::OwnedHandle);

#[cfg(windows)]
impl Job {
    fn new() -> Result<Self> {
        use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
        use windows_sys::Win32::System::JobObjects::{
            CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
            SetInformationJobObject,
        };
        let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if handle.is_null() {
            return Err(std::io::Error::last_os_error()).context("cannot create CLI job");
        }
        // 此句之后的所有失败路径均由 OwnedHandle 关闭句柄。
        let handle = unsafe { OwnedHandle::from_raw_handle(handle) };
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let configured = unsafe {
            SetInformationJobObject(
                handle.as_raw_handle(),
                JobObjectExtendedLimitInformation,
                (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                std::mem::size_of_val(&limits) as u32,
            )
        };
        if configured == 0 {
            return Err(std::io::Error::last_os_error()).context("cannot configure CLI job");
        }
        Ok(Self(handle))
    }

    fn assign(&self, child: &Child) -> Result<()> {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::System::JobObjects::AssignProcessToJobObject;
        if unsafe { AssignProcessToJobObject(self.0.as_raw_handle(), child.as_raw_handle()) } == 0 {
            return Err(std::io::Error::last_os_error()).context("cannot assign CLI to job");
        }
        Ok(())
    }

    fn terminate(&self) {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::System::JobObjects::TerminateJobObject;
        unsafe { TerminateJobObject(self.0.as_raw_handle(), 1) };
    }
}
