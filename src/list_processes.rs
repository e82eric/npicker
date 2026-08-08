use anyhow::{Context, Result};
use windows::Win32::Foundation::{CloseHandle, FILETIME};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::ProcessStatus::{
    K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX,
};
use windows::Win32::System::Threading::{
    GetProcessTimes, OpenProcess, TerminateProcess, WaitForSingleObject,
    PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_TERMINATE,
};

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ProcessInfo {
    pub name: String,
    pub pid: u32,
    pub working_set_kb: u64,
    pub private_bytes_kb: u64,
    pub cpu_seconds: u64,
}

pub fn list_processes() -> Result<Vec<ProcessInfo>> {
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) }
        .context("CreateToolhelp32Snapshot failed")?;
    let mut entry = PROCESSENTRY32W {
        dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };
    let mut processes = Vec::new();
    if unsafe { Process32FirstW(snapshot, &mut entry) }.is_ok() {
        loop {
            let end = entry
                .szExeFile
                .iter()
                .position(|character| *character == 0)
                .unwrap_or(entry.szExeFile.len());
            let mut process = ProcessInfo {
                name: String::from_utf16_lossy(&entry.szExeFile[..end]),
                pid: entry.th32ProcessID,
                ..Default::default()
            };
            fill_process_stats(&mut process);
            processes.push(process);
            if unsafe { Process32NextW(snapshot, &mut entry) }.is_err() {
                break;
            }
        }
    }
    let _ = unsafe { CloseHandle(snapshot) };
    Ok(processes)
}

pub fn terminate_process(pid: u32) -> Result<()> {
    let handle = unsafe { OpenProcess(PROCESS_TERMINATE, false, pid) }
        .with_context(|| format!("OpenProcess failed for PID {pid}"))?;
    let result = unsafe { TerminateProcess(handle, 1) }
        .with_context(|| format!("TerminateProcess failed for PID {pid}"));
    if result.is_ok() {
        // TerminateProcess is asynchronous. Briefly wait so a refreshed process
        // snapshot does not normally retain the terminating process.
        let _ = unsafe { WaitForSingleObject(handle, 500) };
    }
    let _ = unsafe { CloseHandle(handle) };
    result
}

fn fill_process_stats(process: &mut ProcessInfo) {
    let Ok(handle) =
        (unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, process.pid) })
    else {
        return;
    };

    let mut memory = PROCESS_MEMORY_COUNTERS_EX {
        cb: std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32,
        ..Default::default()
    };
    if unsafe {
        K32GetProcessMemoryInfo(
            handle,
            (&raw mut memory).cast::<PROCESS_MEMORY_COUNTERS>(),
            std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32,
        )
    }
    .as_bool()
    {
        process.working_set_kb = memory.WorkingSetSize as u64 / 1024;
        process.private_bytes_kb = memory.PrivateUsage as u64 / 1024;
    }

    let mut creation = FILETIME::default();
    let mut exit = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    if unsafe { GetProcessTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user) }.is_ok()
    {
        process.cpu_seconds = (file_time(kernel) + file_time(user)) / 10_000_000;
    }
    let _ = unsafe { CloseHandle(handle) };
}

fn file_time(value: FILETIME) -> u64 {
    ((value.dwHighDateTime as u64) << 32) | value.dwLowDateTime as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enumeration_includes_the_current_process_with_statistics() {
        let pid = std::process::id();
        let processes = list_processes().expect("process enumeration");
        let current = processes
            .iter()
            .find(|process| process.pid == pid)
            .expect("current process");
        assert!(!current.name.is_empty());
        assert!(current.working_set_kb > 0);
    }
}
