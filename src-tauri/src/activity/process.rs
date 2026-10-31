use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ProcessAnchor {
    pub pid: u32,
    pub started_seconds: u64,
    pub started_microseconds: u64,
    pub executable: String,
}

#[cfg(target_os = "macos")]
mod macos {
    use super::ProcessAnchor;
    use std::ffi::c_void;
    use std::mem::{size_of, MaybeUninit};
    use std::path::Path;

    // ABI from the macOS SDK's sys/proc_info.h, struct proc_bsdinfo (136 bytes).
    #[repr(C)]
    struct ProcBsdInfo {
        identifiers: [u32; 12],
        command: [u8; 16],
        name: [u8; 32],
        resource_fields: [u32; 6],
        started_seconds: u64,
        started_microseconds: u64,
    }

    #[link(name = "proc")]
    extern "C" {
        fn proc_pidinfo(pid: i32, flavor: i32, arg: u64, buffer: *mut c_void, size: i32) -> i32;
        fn proc_pidpath(pid: i32, buffer: *mut c_void, size: u32) -> i32;
    }

    fn info(pid: u32) -> Option<(ProcBsdInfo, String)> {
        let pid = i32::try_from(pid).ok()?;
        let mut data = MaybeUninit::<ProcBsdInfo>::zeroed();
        let size = size_of::<ProcBsdInfo>();
        let read = unsafe { proc_pidinfo(pid, 3, 0, data.as_mut_ptr().cast(), size as i32) };
        if read != size as i32 {
            return None;
        }
        let data = unsafe { data.assume_init() };
        if data.identifiers[1] == 5 {
            return None;
        } // SZOMB is not a living execution process.
        let mut bytes = [0_u8; 4096];
        let read = unsafe { proc_pidpath(pid, bytes.as_mut_ptr().cast(), bytes.len() as u32) };
        if read <= 0 {
            return None;
        }
        let end = bytes.iter().position(|byte| *byte == 0)?;
        let executable = std::str::from_utf8(&bytes[..end]).ok()?.to_owned();
        Some((data, executable))
    }

    pub(super) fn identity(pid: u32) -> Option<ProcessAnchor> {
        let (data, executable) = info(pid)?;
        Some(ProcessAnchor {
            pid,
            started_seconds: data.started_seconds,
            started_microseconds: data.started_microseconds,
            executable,
        })
    }

    pub(super) fn capture() -> Option<ProcessAnchor> {
        let (own, _) = info(std::process::id())?;
        let mut parent = own.identifiers[4];
        let own_executable = std::env::current_exe().ok()?;
        for _ in 0..16 {
            if parent <= 1 {
                return None;
            }
            let (data, executable) = info(parent)?;
            let path = Path::new(&executable);
            let shell = path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| {
                    matches!(
                        name,
                        "sh" | "bash" | "zsh" | "dash" | "fish" | "env" | "timeout"
                    )
                });
            if !shell && path != own_executable {
                return Some(ProcessAnchor {
                    pid: parent,
                    started_seconds: data.started_seconds,
                    started_microseconds: data.started_microseconds,
                    executable,
                });
            }
            parent = data.identifiers[4];
        }
        None
    }
}

#[cfg(target_os = "macos")]
pub(super) fn capture() -> Option<ProcessAnchor> {
    macos::capture()
}

#[cfg(not(target_os = "macos"))]
pub(super) fn capture() -> Option<ProcessAnchor> {
    None
}

#[cfg(target_os = "macos")]
pub(super) fn identity(pid: u32) -> Option<ProcessAnchor> {
    macos::identity(pid)
}

#[cfg(not(target_os = "macos"))]
pub(super) fn identity(_pid: u32) -> Option<ProcessAnchor> {
    None
}

pub(super) fn is_alive(anchor: &ProcessAnchor) -> bool {
    identity(anchor.pid).as_ref() == Some(anchor)
}
