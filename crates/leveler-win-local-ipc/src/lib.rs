//! Narrow safe boundary around Windows named-pipe security APIs.
#![cfg(windows)]
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::{io, mem, ptr};
use tokio::net::windows::named_pipe::{NamedPipeClient, NamedPipeServer, ServerOptions};
use windows_sys::Win32::{
    Foundation::*, Security::Authorization::*, Security::*, System::Pipes::*, System::Threading::*,
};

fn process_sid(pid: u32) -> io::Result<String> {
    // SAFETY: process and token handles are owned below; API output buffers
    // remain allocated through each call and SID conversion.
    unsafe {
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if process.is_null() {
            return Err(io::Error::last_os_error());
        }
        let process = OwnedHandle::from_raw_handle(process);
        let mut token = ptr::null_mut();
        if OpenProcessToken(process.as_raw_handle(), TOKEN_QUERY, &mut token) == 0 {
            return Err(io::Error::last_os_error());
        }
        let token = OwnedHandle::from_raw_handle(token);
        let mut len = 0;
        GetTokenInformation(
            token.as_raw_handle(),
            TokenUser,
            ptr::null_mut(),
            0,
            &mut len,
        );
        if len == 0 {
            return Err(io::Error::last_os_error());
        }
        // usize ensures TOKEN_USER and its embedded pointers are aligned.
        let mut buffer = vec![0usize; (len as usize).div_ceil(mem::size_of::<usize>())];
        if GetTokenInformation(
            token.as_raw_handle(),
            TokenUser,
            buffer.as_mut_ptr().cast(),
            len,
            &mut len,
        ) == 0
        {
            return Err(io::Error::last_os_error());
        }
        let user = &*buffer.as_ptr().cast::<TOKEN_USER>();
        let mut text = ptr::null_mut();
        if ConvertSidToStringSidW(user.User.Sid, &mut text) == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut count = 0;
        while *text.add(count) != 0 {
            count += 1;
        }
        let sid = String::from_utf16(std::slice::from_raw_parts(text, count))
            .map_err(|_| io::Error::other("invalid SID encoding"));
        LocalFree(text.cast());
        sid
    }
}
pub fn current_sid() -> io::Result<String> {
    // SAFETY: no pointers or handles are passed.
    process_sid(unsafe { GetCurrentProcessId() })
}
pub fn create_server(name: &str, first: bool) -> io::Result<NamedPipeServer> {
    let sid = current_sid()?;
    // Protected DACL grants only this account; other processes under that
    // account are within the same trust boundary as a Unix owner-only socket.
    let sddl: Vec<u16> = format!("O:{sid}D:P(A;;GA;;;{sid})")
        .encode_utf16()
        .chain(Some(0))
        .collect();
    unsafe {
        let mut descriptor = ptr::null_mut();
        if ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            SDDL_REVISION_1,
            &mut descriptor,
            ptr::null_mut(),
        ) == 0
        {
            return Err(io::Error::last_os_error());
        }
        let mut attrs = SECURITY_ATTRIBUTES {
            nLength: mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor,
            bInheritHandle: 0,
        };
        let result = ServerOptions::new()
            .first_pipe_instance(first)
            .reject_remote_clients(true)
            .create_with_security_attributes_raw(
                name,
                (&mut attrs as *mut SECURITY_ATTRIBUTES).cast(),
            );
        LocalFree(descriptor);
        result
    }
}
pub fn retain_instance(server: &NamedPipeServer) -> io::Result<OwnedHandle> {
    // SAFETY: DuplicateHandle returns a distinct owned handle; it is never
    // registered for I/O and only keeps the initial pipe instance alive.
    unsafe {
        let mut duplicate = ptr::null_mut();
        let process = GetCurrentProcess();
        if DuplicateHandle(
            process,
            server.as_raw_handle(),
            process,
            &mut duplicate,
            0,
            0,
            DUPLICATE_SAME_ACCESS,
        ) == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(OwnedHandle::from_raw_handle(duplicate))
    }
}
pub fn verify_server(client: &NamedPipeClient) -> io::Result<()> {
    let mut pid = 0;
    // SAFETY: client handle stays alive and pid is writable.
    if unsafe { GetNamedPipeServerProcessId(client.as_raw_handle(), &mut pid) } == 0 {
        return Err(io::Error::last_os_error());
    }
    verify_sid(pid)?;
    // Bind identity to the connected kernel object as well as its process PID.
    // A terminated server's PID can be reused while a duplicated pipe handle
    // remains open elsewhere; a SID lookup by PID alone would be insufficient.
    verify_pipe_owner(client)
}
fn verify_pipe_owner(client: &NamedPipeClient) -> io::Result<()> {
    let expected = current_sid()?;
    // SAFETY: security-info pointers are backed by the returned allocation,
    // which stays live through SID conversion and is then freed exactly once.
    unsafe {
        let mut descriptor = ptr::null_mut();
        let mut owner = ptr::null_mut();
        let status = GetSecurityInfo(
            client.as_raw_handle(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION,
            &mut owner,
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
            &mut descriptor,
        );
        if status != 0 {
            return Err(io::Error::from_raw_os_error(status as i32));
        }
        let mut text = ptr::null_mut();
        if ConvertSidToStringSidW(owner, &mut text) == 0 {
            let error = io::Error::last_os_error();
            LocalFree(descriptor);
            return Err(error);
        }
        let mut len = 0;
        while *text.add(len) != 0 {
            len += 1;
        }
        let actual = String::from_utf16(std::slice::from_raw_parts(text, len));
        LocalFree(text.cast());
        LocalFree(descriptor);
        if actual.is_ok_and(|actual| actual == expected) {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "named pipe object is owned by another account",
            ))
        }
    }
}
pub fn verify_client(server: &NamedPipeServer) -> io::Result<()> {
    let mut pid = 0;
    // SAFETY: server handle stays alive and pid is writable.
    if unsafe { GetNamedPipeClientProcessId(server.as_raw_handle(), &mut pid) } == 0 {
        return Err(io::Error::last_os_error());
    }
    verify_sid(pid)
}
fn verify_sid(pid: u32) -> io::Result<()> {
    if process_sid(pid)? == current_sid()? {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "named pipe peer is owned by another account",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn pipe_dacl_is_protected_and_has_only_current_account() {
        let sid = current_sid().unwrap();
        let name = format!(r"\\.\pipe\CodeLeveler-security-test-{}", std::process::id());
        let server = create_server(&name, true).unwrap();
        let client = tokio::net::windows::named_pipe::ClientOptions::new()
            .open(&name)
            .unwrap();
        server.connect().await.unwrap();
        verify_server(&client).unwrap();
        verify_client(&server).unwrap();
        // SAFETY: GetSecurityInfo returns an allocated descriptor; all pointers
        // into it are inspected before freeing it. The ACE is OS validated.
        unsafe {
            let mut descriptor = ptr::null_mut();
            let mut dacl = ptr::null_mut();
            let status = GetSecurityInfo(
                server.as_raw_handle(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                ptr::null_mut(),
                ptr::null_mut(),
                &mut dacl,
                ptr::null_mut(),
                &mut descriptor,
            );
            assert_eq!(status, 0);
            assert!(!dacl.is_null());
            assert_eq!((*dacl).AceCount, 1);
            let mut control = 0;
            let mut revision = 0;
            assert_ne!(
                GetSecurityDescriptorControl(descriptor, &mut control, &mut revision),
                0
            );
            assert_ne!(control & SE_DACL_PROTECTED, 0);
            let mut ace = ptr::null_mut();
            assert_ne!(GetAce(dacl, 0, &mut ace), 0);
            let ace = &*ace.cast::<ACCESS_ALLOWED_ACE>();
            assert_eq!(ace.Header.AceType, 0 /* ACCESS_ALLOWED_ACE_TYPE */);
            let mut text = ptr::null_mut();
            assert_ne!(
                ConvertSidToStringSidW((&ace.SidStart as *const u32).cast_mut().cast(), &mut text),
                0
            );
            let mut len = 0;
            while *text.add(len) != 0 {
                len += 1;
            }
            let actual = String::from_utf16(std::slice::from_raw_parts(text, len)).unwrap();
            LocalFree(text.cast());
            LocalFree(descriptor);
            assert_eq!(actual, sid);
        }
    }
}
