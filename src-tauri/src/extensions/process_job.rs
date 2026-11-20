//! Own only an installation command and its descendants; never application processes.
use std::{
    io,
    mem::{size_of, zeroed},
    ptr::null,
};
use tokio::process::Child;
use windows_sys::Win32::{
    Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE},
    System::{
        Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD, THREADENTRY32,
        },
        JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JobObjectBasicAccountingInformation,
            JobObjectExtendedLimitInformation, QueryInformationJobObject, SetInformationJobObject,
            TerminateJobObject, JOBOBJECT_BASIC_ACCOUNTING_INFORMATION,
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        },
        Threading::{OpenThread, ResumeThread, THREAD_SUSPEND_RESUME},
    },
};

struct Handle(HANDLE);
unsafe impl Send for Handle {}
unsafe impl Sync for Handle {}
impl Drop for Handle {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}

pub(crate) struct ProcessJob(Handle);
impl ProcessJob {
    pub(crate) fn new() -> io::Result<Self> {
        unsafe {
            let raw = CreateJobObjectW(null(), null());
            if raw.is_null() {
                return Err(io::Error::last_os_error());
            }
            let job = Self(Handle(raw));
            let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = zeroed();
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            if SetInformationJobObject(
                raw,
                JobObjectExtendedLimitInformation,
                &limits as *const _ as _,
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            ) == 0
            {
                return Err(io::Error::last_os_error());
            }
            Ok(job)
        }
    }

    /// The child was spawned suspended: assign it before any descendant can escape.
    pub(crate) fn attach_and_resume(&self, child: &Child) -> io::Result<()> {
        let pid = child
            .id()
            .ok_or_else(|| io::Error::other("Command already exited"))?;
        let process = child
            .raw_handle()
            .ok_or_else(|| io::Error::other("Missing command handle"))?;
        unsafe {
            if AssignProcessToJobObject(self.0 .0, process as _) == 0 {
                return Err(io::Error::last_os_error());
            }
            let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
            if snapshot == INVALID_HANDLE_VALUE {
                return Err(io::Error::last_os_error());
            }
            let snapshot = Handle(snapshot);
            let mut thread: THREADENTRY32 = zeroed();
            thread.dwSize = size_of::<THREADENTRY32>() as u32;
            let mut found = Thread32First(snapshot.0, &mut thread);
            while found != 0 {
                if thread.th32OwnerProcessID == pid {
                    let raw = OpenThread(THREAD_SUSPEND_RESUME, 0, thread.th32ThreadID);
                    if raw.is_null() {
                        return Err(io::Error::last_os_error());
                    }
                    let handle = Handle(raw);
                    if ResumeThread(handle.0) == u32::MAX {
                        return Err(io::Error::last_os_error());
                    }
                    return Ok(());
                }
                found = Thread32Next(snapshot.0, &mut thread);
            }
        }
        Err(io::Error::other("Suspended command thread was not found"))
    }

    pub(crate) fn terminate(&self) -> io::Result<()> {
        if unsafe { TerminateJobObject(self.0 .0, 1) } == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    pub(crate) async fn wait_empty(&self) -> io::Result<()> {
        loop {
            let mut accounting: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION = unsafe { zeroed() };
            if unsafe {
                QueryInformationJobObject(
                    self.0 .0,
                    JobObjectBasicAccountingInformation,
                    &mut accounting as *mut _ as _,
                    size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
                    std::ptr::null_mut(),
                )
            } == 0
            {
                return Err(io::Error::last_os_error());
            }
            if accounting.ActiveProcesses == 0 {
                return Ok(());
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }
}
