//! Windows: who is at the other end of a named pipe, and a pipe only the current user may
//! open. Thin wrappers over Win32 (windows-sys).

use std::io;
use std::os::windows::io::AsRawHandle;
use std::ptr;

use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_INSUFFICIENT_BUFFER, ERROR_SUCCESS, HANDLE, LocalFree,
};
use windows_sys::Win32::Security::Authorization::{GetSecurityInfo, SE_KERNEL_OBJECT};
use windows_sys::Win32::Security::{
    EqualSid, GetTokenInformation, OWNER_SECURITY_INFORMATION, PSID, TOKEN_QUERY, TOKEN_USER,
    TokenUser,
};
use windows_sys::Win32::System::Pipes::GetNamedPipeServerProcessId;
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
};

/// A handle closed on drop.
struct Handle(HANDLE);

impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: the handle was opened by us and is closed once.
        unsafe { CloseHandle(self.0) };
    }
}

/// Memory from LocalAlloc (a security descriptor), freed on drop.
struct Local(*mut core::ffi::c_void);

impl Drop for Local {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: allocated by the system with LocalAlloc, freed once.
            unsafe { LocalFree(self.0) };
        }
    }
}

/// The user (SID) a token belongs to: its `TOKEN_USER`, kept in an aligned buffer.
pub(crate) struct UserSid {
    buffer: Vec<u64>,
}

impl UserSid {
    fn psid(&self) -> PSID {
        // SAFETY: `buffer` holds a TOKEN_USER written by GetTokenInformation.
        unsafe { (*(self.buffer.as_ptr() as *const TOKEN_USER)).User.Sid }
    }

    pub(crate) fn same(&self, other: &UserSid) -> bool {
        // SAFETY: both SIDs point into live TOKEN_USER buffers.
        unsafe { EqualSid(self.psid(), other.psid()) != 0 }
    }

    /// The SID in string form (`S-1-5-21-…`): for a security descriptor, and the pipe's
    /// name.
    pub(crate) fn to_string_sid(&self) -> io::Result<String> {
        use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
        let mut text: *mut u16 = ptr::null_mut();
        // SAFETY: the SID is valid; on success `text` is a NUL-terminated string allocated
        // with LocalAlloc, freed below.
        unsafe {
            if ConvertSidToStringSidW(self.psid(), &mut text) == 0 {
                return Err(io::Error::last_os_error());
            }
            let text = Local(text.cast());
            let chars = text.0 as *const u16;
            let len = (0..).take_while(|&i| *chars.add(i) != 0).count();
            Ok(String::from_utf16_lossy(std::slice::from_raw_parts(
                chars, len,
            )))
        }
    }
}

/// The user a token (opened with `TOKEN_QUERY`) belongs to.
fn token_user(token: HANDLE) -> io::Result<UserSid> {
    let mut needed = 0u32;
    // SAFETY: a size query (null buffer, length 0).
    unsafe { GetTokenInformation(token, TokenUser, ptr::null_mut(), 0, &mut needed) };
    let error = io::Error::last_os_error();
    if error.raw_os_error() != Some(ERROR_INSUFFICIENT_BUFFER as i32) {
        return Err(error);
    }
    let mut buffer = vec![0u64; (needed as usize).div_ceil(8)];
    // SAFETY: `buffer` has room for `needed` bytes and is aligned for TOKEN_USER.
    let ok = unsafe {
        GetTokenInformation(
            token,
            TokenUser,
            buffer.as_mut_ptr().cast(),
            (buffer.len() * 8) as u32,
            &mut needed,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(UserSid { buffer })
}

/// The user of the process whose handle `process` is.
fn process_user(process: HANDLE) -> io::Result<UserSid> {
    let mut token: HANDLE = ptr::null_mut();
    // SAFETY: `process` is a valid process handle; `token` receives a new handle.
    if unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let token = Handle(token);
    token_user(token.0)
}

pub(crate) fn current_user() -> io::Result<UserSid> {
    // SAFETY: the pseudo handle of this process needs no closing.
    process_user(unsafe { GetCurrentProcess() })
}

fn user_of_process(pid: u32) -> io::Result<UserSid> {
    // SAFETY: plain call; a null handle is an error.
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if process.is_null() {
        return Err(io::Error::last_os_error());
    }
    let process = Handle(process);
    process_user(process.0)
}

/// Undoes `ImpersonateNamedPipeClient` when dropped, on every path.
#[cfg(feature = "server")]
struct Impersonating;

#[cfg(feature = "server")]
impl Drop for Impersonating {
    fn drop(&mut self) {
        use windows_sys::Win32::Security::RevertToSelf;
        // SAFETY: plain call; this thread impersonates the pipe's client.
        if unsafe { RevertToSelf() } == 0 {
            // This thread (one of the adapter's tokio workers) would go on as the client:
            // stop rather than run anything else that way.
            let error = io::Error::last_os_error();
            tracing::error!("cannot stop impersonating an agent's client: {error}");
            eprintln!("agent-mcp: cannot stop impersonating an agent's client: {error}");
            std::process::abort();
        }
    }
}

/// Whether the client connected to this pipe instance (a raw pipe handle, valid during the
/// call) runs as the current user: by its own token, through identification-level
/// impersonation (`ImpersonateNamedPipeClient`, `OpenThreadToken`, `RevertToSelf`; clients
/// open the pipe with `SECURITY_IDENTIFICATION`, enough to read the token, and nothing is
/// done as the client). Unlike the client's process id, this cannot be faked by the client
/// and needs no access to its process (an elevated client too). Windows lets a server do
/// this only once it has read something from the pipe (`ERROR_CANNOT_IMPERSONATE` before).
/// All on this thread, with no await in between: nothing else runs as the client.
#[cfg(feature = "server")]
pub(crate) fn client_is_same_user(pipe: HANDLE) -> io::Result<bool> {
    use windows_sys::Win32::System::Pipes::ImpersonateNamedPipeClient;
    use windows_sys::Win32::System::Threading::{GetCurrentThread, OpenThreadToken};

    let token = {
        // SAFETY: `pipe` is a connected server pipe handle.
        if unsafe { ImpersonateNamedPipeClient(pipe) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let _revert = Impersonating;
        let mut token: HANDLE = ptr::null_mut();
        // Checked as the adapter itself (`OpenAsSelf`): an identification-level token
        // cannot open anything, its own thread token included.
        // SAFETY: the pseudo handle of this thread needs no closing; `token` receives a
        // new handle.
        let ok = unsafe { OpenThreadToken(GetCurrentThread(), TOKEN_QUERY, 1, &mut token) };
        if ok == 0 {
            // Taken before `_revert` runs RevertToSelf, which may set its own.
            return Err(io::Error::last_os_error());
        }
        Handle(token)
        // `_revert`: back to the adapter's own identity here.
    };
    Ok(token_user(token.0)?.same(&current_user()?))
}

/// Whether the server of this pipe runs as the current user: the server process's user
/// (`GetNamedPipeServerProcessId`), and the pipe's owner (`GetSecurityInfo`): both must be
/// the current user. A process id can be reused once its process has ended, but another
/// user cannot make a pipe owned by this user (that takes `SeRestorePrivilege`).
pub(crate) fn server_is_same_user(
    pipe: &tokio::net::windows::named_pipe::NamedPipeClient,
) -> io::Result<bool> {
    let handle = pipe.as_raw_handle() as HANDLE;
    let mut pid = 0u32;
    // SAFETY: the pipe handle is valid while `pipe` lives.
    if unsafe { GetNamedPipeServerProcessId(handle, &mut pid) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let me = current_user()?;
    if !user_of_process(pid)?.same(&me) {
        return Ok(false);
    }
    let mut owner: PSID = ptr::null_mut();
    let mut descriptor: *mut core::ffi::c_void = ptr::null_mut();
    // SAFETY: the pipe handle is valid (opened with read access, which includes
    // READ_CONTROL); on success `owner` points into `descriptor`, freed below.
    let status = unsafe {
        GetSecurityInfo(
            handle,
            SE_KERNEL_OBJECT,
            OWNER_SECURITY_INFORMATION,
            &mut owner,
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
            &mut descriptor,
        )
    };
    if status != ERROR_SUCCESS {
        return Err(io::Error::from_raw_os_error(status as i32));
    }
    let _descriptor = Local(descriptor);
    // SAFETY: `owner` points into the live descriptor; `me` is a live TOKEN_USER.
    Ok(!owner.is_null() && unsafe { EqualSid(owner, me.psid()) } != 0)
}

/// Security attributes that let only the current user open the pipe (a protected DACL
/// with one entry: generic all for the user's SID; no inherited entries), and make the user
/// its owner (not the Administrators group, the default owner of an elevated process's
/// objects): the bridge checks the owner.
#[cfg(feature = "server")]
pub(crate) struct OnlyCurrentUser {
    descriptor: *mut core::ffi::c_void,
    attributes: windows_sys::Win32::Security::SECURITY_ATTRIBUTES,
}

#[cfg(feature = "server")]
impl OnlyCurrentUser {
    pub(crate) fn new() -> io::Result<Self> {
        use windows_sys::Win32::Security::Authorization::{
            ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
        };
        let sid = current_user()?.to_string_sid()?;
        let sddl: Vec<u16> = format!("O:{sid}D:P(A;;GA;;;{sid})")
            .encode_utf16()
            .chain([0])
            .collect();
        let mut descriptor: *mut core::ffi::c_void = ptr::null_mut();
        // SAFETY: `sddl` is NUL-terminated; on success `descriptor` is allocated with
        // LocalAlloc and freed on drop.
        let ok = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                SDDL_REVISION_1,
                &mut descriptor,
                ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            descriptor,
            attributes: windows_sys::Win32::Security::SECURITY_ATTRIBUTES {
                nLength: size_of::<windows_sys::Win32::Security::SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: descriptor,
                bInheritHandle: 0,
            },
        })
    }

    /// For `ServerOptions::create_with_security_attributes_raw`: valid while `self` lives.
    pub(crate) fn as_ptr(&mut self) -> *mut core::ffi::c_void {
        (&mut self.attributes as *mut windows_sys::Win32::Security::SECURITY_ATTRIBUTES).cast()
    }
}

#[cfg(feature = "server")]
impl Drop for OnlyCurrentUser {
    fn drop(&mut self) {
        // SAFETY: allocated by ConvertStringSecurityDescriptorToSecurityDescriptorW.
        unsafe { LocalFree(self.descriptor) };
    }
}
