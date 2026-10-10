//! Spawn a supervisor without retaining a caller's stdio handles.

#[cfg(not(windows))]
pub(crate) type SupervisorChild = tokio::process::Child;

#[cfg(not(windows))]
pub(crate) fn spawn(command: &mut tokio::process::Command) -> std::io::Result<SupervisorChild> {
    command.spawn()
}

#[cfg(windows)]
pub(crate) struct SupervisorChild(std::os::windows::io::OwnedHandle);

#[cfg(windows)]
impl SupervisorChild {
    pub(crate) fn try_wait(&mut self) -> std::io::Result<Option<std::process::ExitStatus>> {
        use std::os::windows::{io::AsRawHandle, process::ExitStatusExt};
        use windows_sys::Win32::Foundation::{WAIT_FAILED, WAIT_TIMEOUT};
        use windows_sys::Win32::System::Threading::{GetExitCodeProcess, WaitForSingleObject};
        unsafe {
            let handle = self.0.as_raw_handle();
            match WaitForSingleObject(handle, 0) {
                WAIT_TIMEOUT => Ok(None),
                WAIT_FAILED => Err(std::io::Error::last_os_error()),
                _ => {
                    let mut status = 0;
                    if GetExitCodeProcess(handle, &mut status) == 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(Some(std::process::ExitStatus::from_raw(status)))
                }
            }
        }
    }

    pub(crate) async fn wait(&mut self) -> std::io::Result<std::process::ExitStatus> {
        loop {
            if let Some(status) = self.try_wait()? {
                return Ok(status);
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }
}

#[cfg(windows)]
fn quoted(value: &std::ffi::OsStr) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    let mut result = vec![b'"' as u16];
    let mut slashes = 0;
    for character in value.encode_wide() {
        if character == b'\\' as u16 {
            slashes += 1;
        } else {
            let count = if character == b'"' as u16 {
                slashes * 2 + 1
            } else {
                slashes
            };
            result.extend(std::iter::repeat_n(b'\\' as u16, count));
            result.push(character);
            slashes = 0;
        }
    }
    result.extend(std::iter::repeat_n(b'\\' as u16, slashes * 2));
    result.push(b'"' as u16);
    result
}

#[cfg(windows)]
pub(crate) fn spawn(command: &mut tokio::process::Command) -> std::io::Result<SupervisorChild> {
    use std::os::windows::{
        ffi::OsStrExt,
        io::{FromRawHandle, OwnedHandle},
    };
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{
        CREATE_UNICODE_ENVIRONMENT, CreateProcessW, DETACHED_PROCESS, PROCESS_INFORMATION,
        STARTUPINFOW,
    };
    let command = command.as_std();
    let executable = command
        .get_program()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let mut arguments = quoted(command.get_program());
    for argument in command.get_args() {
        arguments.push(b' ' as u16);
        arguments.extend(quoted(argument));
    }
    if arguments.contains(&0) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "NUL in supervisor arguments",
        ));
    }
    arguments.push(0);
    let mut variables = std::env::vars_os().collect::<Vec<_>>();
    for (key, value) in command.get_envs() {
        variables.retain(|(existing, _)| {
            !existing
                .to_string_lossy()
                .eq_ignore_ascii_case(&key.to_string_lossy())
        });
        if let Some(value) = value {
            variables.push((key.to_owned(), value.to_owned()));
        }
    }
    variables.sort_by_key(|(key, _)| key.to_string_lossy().to_uppercase());
    let mut environment = Vec::new();
    for (key, value) in variables {
        environment.extend(key.encode_wide());
        environment.push(b'=' as u16);
        environment.extend(value.encode_wide());
        environment.push(0);
    }
    environment.push(0);
    unsafe {
        let mut startup: STARTUPINFOW = std::mem::zeroed();
        startup.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
        let mut process: PROCESS_INFORMATION = std::mem::zeroed();
        // No inherited handles and no console: the supervisor outlives CLI/MCP exit
        // without holding either captured pipes or the caller's terminal open.
        if CreateProcessW(
            executable.as_ptr(),
            arguments.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            0,
            CREATE_UNICODE_ENVIRONMENT | DETACHED_PROCESS,
            environment.as_ptr().cast(),
            std::ptr::null(),
            &startup,
            &mut process,
        ) == 0
        {
            return Err(std::io::Error::last_os_error());
        }
        CloseHandle(process.hThread);
        Ok(SupervisorChild(OwnedHandle::from_raw_handle(
            process.hProcess,
        )))
    }
}
