//! The Win32 half of [`crate`]. Every `unsafe` block in the workspace's
//! Windows write confinement lives here or in the `leveler-confine` binary.

use std::ffi::{OsStr, OsString};
use std::io;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::Path;
use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::{ERROR_SUCCESS, LocalFree};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSidToSidW, GetNamedSecurityInfoW, SE_FILE_OBJECT,
    SetNamedSecurityInfoW,
};
use windows_sys::Win32::Security::{
    ACE_HEADER, ACL, ACL_REVISION, AddMandatoryAce, CONTAINER_INHERIT_ACE, GetAce, GetLengthSid,
    InitializeAcl, LABEL_SECURITY_INFORMATION, OBJECT_INHERIT_ACE, PSID,
    SYSTEM_MANDATORY_LABEL_ACE,
};

use crate::{IntegrityLabel, LOW_INTEGRITY_SID};

/// `SYSTEM_MANDATORY_LABEL_ACE_TYPE`. windows-sys exposes it only as part of
/// the ACE union constants, so name it once here.
const SYSTEM_MANDATORY_LABEL_ACE_TYPE: u8 = 0x11;

/// `SYSTEM_MANDATORY_LABEL_NO_WRITE_UP` — the mandatory policy that denies a
/// lower-integrity subject write access. Reads are never denied by it.
const SYSTEM_MANDATORY_LABEL_NO_WRITE_UP: u32 = 0x1;

fn wide(path: &Path) -> Vec<u16> {
    OsStr::new(path)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

fn win32_error(context: &str, code: u32) -> io::Error {
    io::Error::new(
        io::Error::from_raw_os_error(code as i32).kind(),
        format!("{context}: {}", io::Error::from_raw_os_error(code as i32)),
    )
}

/// An owned `PSID` from `ConvertStringSidToSidW`, freed on drop.
struct OwnedSid(PSID);

impl OwnedSid {
    fn parse(text: &str) -> io::Result<Self> {
        let wide: Vec<u16> = OsStr::new(text)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let mut sid: PSID = null_mut();
        // SAFETY: `wide` is a NUL-terminated UTF-16 buffer that outlives the
        // call; `sid` receives a LocalAlloc'd SID this type owns and frees.
        let ok = unsafe { ConvertStringSidToSidW(wide.as_ptr(), &mut sid) };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self(sid))
    }
}

impl Drop for OwnedSid {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: the pointer came from ConvertStringSidToSidW (LocalAlloc).
            unsafe { LocalFree(self.0) };
        }
    }
}

/// A security descriptor from `GetNamedSecurityInfoW`, freed on drop. The SACL
/// it hands back points INTO this allocation, so the descriptor must outlive
/// every read of it.
struct OwnedSecurityDescriptor(*mut std::ffi::c_void);

impl Drop for OwnedSecurityDescriptor {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: the pointer came from GetNamedSecurityInfoW (LocalAlloc).
            unsafe { LocalFree(self.0) };
        }
    }
}

fn sid_to_string(sid: PSID) -> io::Result<String> {
    let mut text: *mut u16 = null_mut();
    // SAFETY: `sid` points into a live security descriptor; `text` receives a
    // LocalAlloc'd string this function copies and frees before returning.
    let ok = unsafe { ConvertSidToStringSidW(sid, &mut text) };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: ConvertSidToStringSidW returns a NUL-terminated UTF-16 string.
    let len = unsafe {
        let mut len = 0usize;
        while *text.add(len) != 0 {
            len += 1;
        }
        len
    };
    // SAFETY: `text[..len]` is the string body, still allocated.
    let owned = OsString::from_wide(unsafe { std::slice::from_raw_parts(text, len) });
    // SAFETY: `text` came from ConvertSidToStringSidW (LocalAlloc).
    unsafe { LocalFree(text.cast()) };
    Ok(owned.to_string_lossy().into_owned())
}

pub(crate) fn read_label(path: &Path) -> io::Result<IntegrityLabel> {
    let path_w = wide(path);
    let mut sacl: *mut ACL = null_mut();
    let mut descriptor: *mut std::ffi::c_void = null_mut();
    // SAFETY: `path_w` is NUL-terminated; the out-params are owned below.
    let status = unsafe {
        GetNamedSecurityInfoW(
            path_w.as_ptr(),
            SE_FILE_OBJECT,
            LABEL_SECURITY_INFORMATION,
            null_mut(),
            null_mut(),
            null_mut(),
            &mut sacl,
            &mut descriptor,
        )
    };
    if status != ERROR_SUCCESS {
        return Err(win32_error(
            &format!("read integrity label of {}", path.display()),
            status,
        ));
    }
    let _descriptor = OwnedSecurityDescriptor(descriptor);
    if sacl.is_null() {
        return Ok(IntegrityLabel::Inherited);
    }
    // SAFETY: `sacl` points into the live descriptor.
    let count = unsafe { (*sacl).AceCount } as u32;
    for index in 0..count {
        let mut ace: *mut std::ffi::c_void = null_mut();
        // SAFETY: `index` is below AceCount for this live ACL.
        if unsafe { GetAce(sacl, index, &mut ace) } == 0 {
            continue;
        }
        // SAFETY: GetAce returned a live ACE inside the descriptor.
        let header = unsafe { *(ace as *const ACE_HEADER) };
        if header.AceType != SYSTEM_MANDATORY_LABEL_ACE_TYPE {
            continue;
        }
        let label = ace as *const SYSTEM_MANDATORY_LABEL_ACE;
        // SAFETY: the ACE is a SYSTEM_MANDATORY_LABEL_ACE; its SID starts at
        // `SidStart` and lives inside the same allocation.
        let (mask, sid) = unsafe { ((*label).Mask, std::ptr::addr_of!((*label).SidStart)) };
        return Ok(IntegrityLabel::Explicit {
            sid: sid_to_string(sid as PSID)?,
            ace_flags: header.AceFlags,
            mask,
        });
    }
    Ok(IntegrityLabel::Inherited)
}

/// Write `label` onto `path`. `None` clears the label (an empty SACL), which is
/// how an object goes back to having no explicit level.
fn write_label(path: &Path, label: Option<(&str, u8, u32)>) -> io::Result<()> {
    let mut path_w = wide(path);
    let mut buffer: Vec<u8>;
    let acl_ptr: *const ACL = match label {
        Some((sid_text, ace_flags, mask)) => {
            let sid = OwnedSid::parse(sid_text)?;
            // SAFETY: `sid.0` is a valid SID for the lifetime of `sid`.
            let sid_len = unsafe { GetLengthSid(sid.0) } as usize;
            // ACL header + the label ACE, whose trailing `SidStart` field
            // already accounts for the first 4 SID bytes.
            let size = std::mem::size_of::<ACL>()
                + std::mem::size_of::<SYSTEM_MANDATORY_LABEL_ACE>()
                + sid_len
                - std::mem::size_of::<u32>();
            buffer = vec![0u8; size];
            let acl = buffer.as_mut_ptr() as *mut ACL;
            // SAFETY: `buffer` is `size` bytes of writable storage for the ACL.
            if unsafe { InitializeAcl(acl, size as u32, ACL_REVISION) } == 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: `acl` was just initialized with room for this one ACE.
            let added =
                unsafe { AddMandatoryAce(acl, ACL_REVISION, ace_flags as u32, mask, sid.0) };
            if added == 0 {
                return Err(io::Error::last_os_error());
            }
            acl as *const ACL
        }
        None => {
            let size = std::mem::size_of::<ACL>();
            buffer = vec![0u8; size];
            let acl = buffer.as_mut_ptr() as *mut ACL;
            // SAFETY: `buffer` is exactly one empty ACL header.
            if unsafe { InitializeAcl(acl, size as u32, ACL_REVISION) } == 0 {
                return Err(io::Error::last_os_error());
            }
            acl as *const ACL
        }
    };

    // SAFETY: `path_w` is NUL-terminated and `acl_ptr` points at the ACL built
    // above, which outlives the call.
    let status = unsafe {
        SetNamedSecurityInfoW(
            path_w.as_mut_ptr(),
            SE_FILE_OBJECT,
            LABEL_SECURITY_INFORMATION,
            null_mut(),
            null_mut(),
            null(),
            acl_ptr,
        )
    };
    if status != ERROR_SUCCESS {
        return Err(win32_error(
            &format!("set integrity label of {}", path.display()),
            status,
        ));
    }
    Ok(())
}

pub(crate) fn apply_low(path: &Path) -> io::Result<IntegrityLabel> {
    let previous = read_label(path)?;
    // Inheritable so files and directories created inside the write root are
    // writable by the same confined child. Windows propagates an inheritable
    // SACL entry to existing children that are not protected.
    write_label(
        path,
        Some((
            LOW_INTEGRITY_SID,
            (OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE) as u8,
            SYSTEM_MANDATORY_LABEL_NO_WRITE_UP,
        )),
    )?;
    Ok(previous)
}

pub(crate) fn restore(path: &Path, previous: &IntegrityLabel) -> io::Result<()> {
    match previous {
        IntegrityLabel::Inherited => write_label(path, None),
        IntegrityLabel::Explicit {
            sid,
            ace_flags,
            mask,
        } => write_label(path, Some((sid, *ace_flags, *mask))),
    }
}
