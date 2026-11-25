//! The Win32 half of [`crate`]. Every `unsafe` block in the workspace's
//! Windows write confinement lives here or in the `leveler-confine` binary.

use std::ffi::{OsStr, OsString};
use std::io;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::Path;
use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::{ERROR_SUCCESS, HANDLE, INVALID_HANDLE_VALUE, LocalFree};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSidToSidW, GetNamedSecurityInfoW, SE_FILE_OBJECT,
    SetNamedSecurityInfoW,
};
use windows_sys::Win32::Security::{
    ACE_HEADER, ACL, ACL_REVISION, AddMandatoryAce, CONTAINER_INHERIT_ACE, GetAce, GetLengthSid,
    InitializeAcl, LABEL_SECURITY_INFORMATION, OBJECT_INHERIT_ACE, PSID,
    SYSTEM_MANDATORY_LABEL_ACE, TOKEN_INFORMATION_CLASS,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_BASIC_INFO, FILE_FLAG_BACKUP_SEMANTICS, FILE_ID_INFO, FILE_READ_ATTRIBUTES,
    FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FileBasicInfo, FileIdInfo, GetDriveTypeW,
    GetFileInformationByHandleEx, GetVolumeInformationByHandleW, OPEN_EXISTING,
};
use windows_sys::Win32::System::WindowsProgramming::DRIVE_REMOTE;

use crate::{FileObjectIdentity, IntegrityLabel};

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

pub(crate) type RawHandle = HANDLE;

/// A `CreateFileW` handle closed on every return path.
struct OwnedHandle(HANDLE);

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        if self.0 != INVALID_HANDLE_VALUE && !self.0.is_null() {
            // SAFETY: the handle came from CreateFileW and is owned here.
            unsafe { windows_sys::Win32::Foundation::CloseHandle(self.0) };
        }
    }
}

fn win32_error(context: &str, code: u32) -> io::Error {
    io::Error::new(
        io::Error::from_raw_os_error(code as i32).kind(),
        format!("{context}: {}", io::Error::from_raw_os_error(code as i32)),
    )
}

/// Refuse volumes whose file id is not the filesystem's persistent identity.
///
/// NTFS and ReFS keep a file id stable across a rename and allocate a new one
/// when the object is replaced. The FAT family reuses directory-entry slots,
/// and a network share's id is the server's to recycle, so neither can prove
/// the incarnation a grant was bound to.
fn require_persistent_file_ids(handle: HANDLE) -> io::Result<()> {
    let mut serial = 0u32;
    let mut maximum_component = 0u32;
    let mut flags = 0u32;
    let mut filesystem = [0u16; 64];
    // SAFETY: the handle is live; every out-parameter points at writable
    // storage, and the filesystem buffer is its own length.
    let ok = unsafe {
        GetVolumeInformationByHandleW(
            handle,
            std::ptr::null_mut(),
            0,
            &mut serial,
            &mut maximum_component,
            &mut flags,
            filesystem.as_mut_ptr(),
            filesystem.len() as u32,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    let len = filesystem
        .iter()
        .position(|unit| *unit == 0)
        .unwrap_or(filesystem.len());
    let filesystem = String::from_utf16_lossy(&filesystem[..len]);
    if filesystem.eq_ignore_ascii_case("NTFS") || filesystem.eq_ignore_ascii_case("ReFS") {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!("filesystem `{filesystem}` does not provide persistent file ids"),
        ))
    }
}

/// A local volume reachable through a drive letter. UNC shares and device
/// namespaces are refused: their ids are not the local disk's to prove.
fn require_local_drive(path: &Path) -> io::Result<()> {
    use std::path::{Component, Prefix};
    let prefix = path.components().find_map(|component| match component {
        Component::Prefix(prefix) => Some(prefix.kind()),
        _ => None,
    });
    let root = match prefix {
        Some(Prefix::Disk(letter)) | Some(Prefix::VerbatimDisk(letter)) => {
            wide(Path::new(&format!("{}:\\", char::from(letter))))
        }
        Some(Prefix::UNC(_, _))
        | Some(Prefix::VerbatimUNC(_, _))
        | Some(Prefix::DeviceNS(_))
        | Some(Prefix::Verbatim(_)) => {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "network and device-namespace paths have no local persistent file id",
            ));
        }
        None => {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "a relative path cannot prove which volume holds the object",
            ));
        }
    };
    // SAFETY: `root` is a NUL-terminated drive-root string.
    let drive_type = unsafe { GetDriveTypeW(root.as_ptr()) };
    if drive_type == DRIVE_REMOTE {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "network shares have no local persistent file id",
        ));
    }
    Ok(())
}

pub(crate) fn file_identity_at(path: &Path) -> io::Result<FileObjectIdentity> {
    require_local_drive(path)?;
    let path_w = wide(path);
    // SAFETY: `path_w` is NUL-terminated; the returned handle is owned by
    // `owned` and closed on every path. FILE_FLAG_BACKUP_SEMANTICS is what
    // lets the same call open a directory; reparse points are followed so the
    // identity is the object the path reaches.
    let handle = unsafe {
        CreateFileW(
            path_w.as_ptr(),
            FILE_READ_ATTRIBUTES,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    let owned = OwnedHandle(handle);
    file_identity_of_handle(owned.0)
}

pub(crate) fn file_identity_of_handle(handle: HANDLE) -> io::Result<FileObjectIdentity> {
    require_persistent_file_ids(handle)?;
    let mut id = FILE_ID_INFO {
        VolumeSerialNumber: 0,
        FileId: windows_sys::Win32::Storage::FileSystem::FILE_ID_128 {
            Identifier: [0u8; 16],
        },
    };
    // SAFETY: the handle is live and `id` is exactly the structure the class
    // expects for FileIdInfo.
    let ok = unsafe {
        GetFileInformationByHandleEx(
            handle,
            FileIdInfo,
            (&mut id as *mut FILE_ID_INFO).cast(),
            std::mem::size_of::<FILE_ID_INFO>() as u32,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut basic: FILE_BASIC_INFO = unsafe { std::mem::zeroed() };
    // SAFETY: same live handle; `basic` is FILE_BASIC_INFO for FileBasicInfo.
    let ok = unsafe {
        GetFileInformationByHandleEx(
            handle,
            FileBasicInfo,
            (&mut basic as *mut FILE_BASIC_INFO).cast(),
            std::mem::size_of::<FILE_BASIC_INFO>() as u32,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(FileObjectIdentity {
        volume_serial: id.VolumeSerialNumber,
        file_id: id.FileId.Identifier,
        creation_time: basic.CreationTime as u64,
    })
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
    apply(path, crate::LOW_INTEGRITY_SID)
}

pub(crate) fn apply_medium(path: &Path) -> io::Result<IntegrityLabel> {
    apply(path, crate::MEDIUM_INTEGRITY_SID)
}

/// Apply `sid` with `NO_WRITE_UP` and return the label `path` had. Stale
/// inheritance cannot be skipped the way the Low root path can: a protected
/// file may already carry a Low label inherited from its parent, and the whole
/// point of this call is to override exactly that.
fn apply(path: &Path, sid: &str) -> io::Result<IntegrityLabel> {
    // Inheritable so files and directories created inside the object inherit
    // the same level. A file does not use the inherit flags: they say what new
    // children get, and a file has none.
    let flags = if path.is_dir() {
        (OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE) as u8
    } else {
        0
    };
    let desired = (sid, flags, SYSTEM_MANDATORY_LABEL_NO_WRITE_UP);
    let previous = read_label(path)?;
    let already = matches!(
        &previous,
        IntegrityLabel::Explicit { sid: current, ace_flags, mask }
            if current == desired.0 && *ace_flags == desired.1 && *mask == desired.2
    );
    if !already {
        write_label(path, Some(desired))?;
    }
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

// ---------------------------------------------------------------------------
// Owner-only state directories and object validation.
//
// The persistent Execution Host keeps its durable records (spawn intents, task
// archive, ready endpoint) in a state directory. Unix requires a 0700
// directory owned by the effective user and refuses symlinks. Windows has no
// mode bits, so the equivalent invariant is an owner-only DACL: the directory
// and the files in it are accessible only to the current user, SYSTEM and the
// Administrators group, with inheritance protected (no inherited Everyone /
// Users / Authenticated Users ACEs), and no reparse point stands where the
// directory should be.
// ---------------------------------------------------------------------------

/// `FILE_ALL_ACCESS` — every file right, used for the owner-only DACL.
const FILE_ALL_ACCESS: u32 = 0x001F_01FF;

/// A SID taken from the current process token, copied into its own allocation.
struct TokenSid(PSID);

impl Drop for TokenSid {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: the SID was allocated with GetTokenInformation into a
            // Vec, then copied here with CopySid; LocalAlloc owns it.
            unsafe { LocalFree(self.0) };
        }
    }
}

/// Copy one SID out of the current process token.
///
/// `sid_of` reads the SID pointer out of the information class's structure:
/// `TOKEN_USER` and `TOKEN_OWNER` place it at different offsets. `None` means
/// the token carries no SID for that class (Windows permits a null owner).
fn token_sid(
    class: TOKEN_INFORMATION_CLASS,
    sid_of: impl Fn(*const u8) -> PSID,
) -> io::Result<Option<TokenSid>> {
    use windows_sys::Win32::Security::{CopySid, GetTokenInformation, TOKEN_QUERY};
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    let mut token: HANDLE = INVALID_HANDLE_VALUE;
    // SAFETY: the pseudo-handle is valid; `token` is a live out-parameter.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let token = OwnedHandle(token);

    let mut needed = 0u32;
    // SAFETY: a zero-length query only reports the required size and fails
    // with ERROR_INSUFFICIENT_BUFFER, which is the documented probe.
    unsafe {
        GetTokenInformation(token.0, class, null_mut(), 0, &mut needed);
    }
    if needed == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut buffer = vec![0u8; needed as usize];
    // SAFETY: `buffer` is `needed` bytes, the exact size the API asked for.
    if unsafe {
        GetTokenInformation(
            token.0,
            class,
            buffer.as_mut_ptr().cast(),
            needed,
            &mut needed,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a successful query wrote the class's structure at the start of
    // `buffer`; `sid_of` reads the SID it points at inside the same buffer,
    // which outlives the copy below.
    let sid = sid_of(buffer.as_ptr());
    if sid.is_null() {
        return Ok(None);
    }
    // SAFETY: `sid` is a valid SID inside `buffer`.
    let sid_len = unsafe { GetLengthSid(sid) } as usize;
    // SAFETY: LocalAlloc for the copy; freed by TokenSid.
    let owned: PSID = unsafe {
        windows_sys::Win32::System::Memory::LocalAlloc(
            windows_sys::Win32::System::Memory::LMEM_FIXED,
            sid_len,
        )
    } as PSID;
    if owned.is_null() {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: both SIDs are valid; `owned` has exactly `sid_len` bytes.
    if unsafe { CopySid(sid_len as u32, owned, sid) } == 0 {
        // SAFETY: `owned` came from LocalAlloc above.
        unsafe { LocalFree(owned) };
        return Err(io::Error::last_os_error());
    }
    Ok(Some(TokenSid(owned)))
}

fn current_user_sid() -> io::Result<TokenSid> {
    use windows_sys::Win32::Security::{TOKEN_USER, TokenUser};

    // SAFETY: a successful TokenUser query wrote a TOKEN_USER at the start of
    // the buffer passed to `sid_of`.
    token_sid(TokenUser, |buffer| unsafe {
        (*(buffer as *const TOKEN_USER)).User.Sid
    })?
    .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "process token has no user SID"))
}

/// The SID Windows assigns as owner to objects this process creates.
///
/// A UAC-elevated process keeps its user SID, but the token's default owner is
/// `BUILTIN\Administrators`, so Windows stamps that group on the objects it
/// creates. The state directory stays private to the interposed user through
/// its DACL, which names the user SID; both identities are therefore valid
/// owners of an object this principal created.
fn current_owner_sid() -> io::Result<Option<TokenSid>> {
    use windows_sys::Win32::Security::{TOKEN_OWNER, TokenOwner};

    // SAFETY: a successful TokenOwner query wrote a TOKEN_OWNER at the start
    // of the buffer passed to `sid_of`.
    token_sid(TokenOwner, |buffer| unsafe {
        (*(buffer as *const TOKEN_OWNER)).Owner
    })
}

fn sid_equals(a: PSID, b: PSID) -> bool {
    // SAFETY: both pointers reference valid SIDs for the caller's lifetime.
    !a.is_null() && !b.is_null() && unsafe { windows_sys::Win32::Security::EqualSid(a, b) } != 0
}

/// Whether `attributes` name a reparse point (symlink, junction, mount point).
fn is_reparse_point(attributes: u32) -> bool {
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
    attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

fn file_attributes(path: &Path) -> io::Result<u32> {
    use windows_sys::Win32::Storage::FileSystem::GetFileAttributesW;
    const INVALID_FILE_ATTRIBUTES: u32 = u32::MAX;
    let wide = wide(path);
    // SAFETY: `wide` is NUL-terminated and outlives the call.
    let attributes = unsafe { GetFileAttributesW(wide.as_ptr()) };
    if attributes == INVALID_FILE_ATTRIBUTES {
        return Err(io::Error::last_os_error());
    }
    Ok(attributes)
}

/// Build and install an owner-only, inheritance-protected DACL on `path`.
fn apply_private_dacl(path: &Path) -> io::Result<()> {
    use windows_sys::Win32::Security::Authorization::{SE_FILE_OBJECT, SetNamedSecurityInfoW};
    use windows_sys::Win32::Security::{
        ACL_REVISION, AddAccessAllowedAceEx, CONTAINER_INHERIT_ACE, InitializeAcl,
        OBJECT_INHERIT_ACE,
    };
    use windows_sys::Win32::Security::{
        DACL_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION,
    };

    let user = current_user_sid()?;
    let system = OwnedSid::parse("S-1-5-18")?;
    let administrators = OwnedSid::parse("S-1-5-32-544")?;
    let sids = [user.0, system.0, administrators.0];
    let acl_size: usize = std::mem::size_of::<ACL>()
        + sids
            .iter()
            .map(|sid| {
                8 + std::mem::size_of::<u32>() * 2
                    // SAFETY: every `sid` is a live SID.
                    + unsafe { GetLengthSid(*sid) } as usize
            })
            .sum::<usize>()
        + 32;
    let mut storage = vec![0u8; acl_size];
    let acl = storage.as_mut_ptr() as *mut ACL;
    // SAFETY: `storage` is `acl_size` writable bytes.
    if unsafe { InitializeAcl(acl, acl_size as u32, ACL_REVISION) } == 0 {
        return Err(io::Error::last_os_error());
    }
    for sid in sids {
        // SAFETY: the ACL has room for every ACE (sized above); `sid` is live.
        if unsafe {
            AddAccessAllowedAceEx(
                acl,
                ACL_REVISION,
                CONTAINER_INHERIT_ACE | OBJECT_INHERIT_ACE,
                FILE_ALL_ACCESS,
                sid,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
    }
    let mut path_w = wide(path);
    // SAFETY: `path_w` is NUL-terminated; `acl` outlives the call.
    let status = unsafe {
        SetNamedSecurityInfoW(
            path_w.as_mut_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            null_mut(),
            null_mut(),
            acl,
            null(),
        )
    };
    if status != ERROR_SUCCESS {
        return Err(win32_error(
            &format!("set owner-only DACL on {}", path.display()),
            status,
        ));
    }
    Ok(())
}

/// `ACCESS_ALLOWED_ACE_TYPE`. windows-sys exports ACE structs, but not this
/// discriminator constant on every feature set.
const ACCESS_ALLOWED_ACE_TYPE: u8 = 0x00;

/// Refuse any security descriptor that is not owner-only.
fn validate_private_security(path: &Path) -> io::Result<()> {
    use windows_sys::Win32::Security::Authorization::{GetNamedSecurityInfoW, SE_FILE_OBJECT};
    use windows_sys::Win32::Security::{
        ACCESS_ALLOWED_ACE, DACL_SECURITY_INFORMATION, GetSecurityDescriptorOwner,
        OWNER_SECURITY_INFORMATION,
    };

    let user = current_user_sid()?;
    let token_owner = current_owner_sid()?;
    let mut dacl_ptr: *mut ACL = null_mut();
    let mut owner_ptr: PSID = null_mut();
    let mut descriptor: *mut std::ffi::c_void = null_mut();
    let mut path_w = wide(path);
    // SAFETY: `path_w` is NUL-terminated; the out-parameters are owned below.
    let status = unsafe {
        GetNamedSecurityInfoW(
            path_w.as_mut_ptr(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            &mut owner_ptr,
            null_mut(),
            &mut dacl_ptr,
            null_mut(),
            &mut descriptor,
        )
    };
    if status != ERROR_SUCCESS {
        return Err(win32_error(
            &format!("read security descriptor of {}", path.display()),
            status,
        ));
    }
    let _descriptor = OwnedSecurityDescriptor(descriptor);
    let mut defaulted = 0i32;
    // SAFETY: the descriptor is live for the duration of this function.
    if unsafe { GetSecurityDescriptorOwner(descriptor, &mut owner_ptr, &mut defaulted) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let owned_by_this_principal = sid_equals(owner_ptr, user.0)
        || token_owner
            .as_ref()
            .is_some_and(|owner| sid_equals(owner_ptr, owner.0));
    if !owned_by_this_principal {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "state directory {} is not owned by the current user or the \
                 default owner of its process token",
                path.display()
            ),
        ));
    }
    if dacl_ptr.is_null() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "state directory {} has a null DACL (every principal has access)",
                path.display()
            ),
        ));
    }
    let system = OwnedSid::parse("S-1-5-18")?;
    let administrators = OwnedSid::parse("S-1-5-32-544")?;
    let creator_owner = OwnedSid::parse("S-1-3-0")?;
    let owner_rights = OwnedSid::parse("S-1-3-4")?;
    // SAFETY: `dacl_ptr` points into the live descriptor.
    let count = unsafe { (*dacl_ptr).AceCount } as u32;
    let mut saw_user = false;
    for index in 0..count {
        let mut ace: *mut std::ffi::c_void = null_mut();
        // SAFETY: `index` is below AceCount for this live ACL.
        if unsafe { GetAce(dacl_ptr, index, &mut ace) } == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unreadable DACL entry on {}", path.display()),
            ));
        }
        // SAFETY: GetAce returned a live ACE inside the descriptor.
        let header = unsafe { *(ace as *const ACE_HEADER) };
        if header.AceType != ACCESS_ALLOWED_ACE_TYPE {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "state directory {} has a non-allow ACE (type {})",
                    path.display(),
                    header.AceType
                ),
            ));
        }
        let allowed = ace as *const ACCESS_ALLOWED_ACE;
        // SAFETY: the ACE is an ACCESS_ALLOWED_ACE; its SID starts at `SidStart`.
        let sid = unsafe { std::ptr::addr_of!((*allowed).SidStart) } as PSID;
        let permitted = sid_equals(sid, user.0)
            || sid_equals(sid, system.0)
            || sid_equals(sid, administrators.0)
            || sid_equals(sid, creator_owner.0)
            || sid_equals(sid, owner_rights.0);
        if !permitted {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "state directory {} grants access to a principal other than \
                     its owner, SYSTEM or Administrators ({})",
                    path.display(),
                    sid_to_string(sid).unwrap_or_else(|_| "unknown SID".into())
                ),
            ));
        }
        if sid_equals(sid, user.0) {
            saw_user = true;
        }
    }
    if !saw_user {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "state directory {} does not grant its owner access",
                path.display()
            ),
        ));
    }
    Ok(())
}

/// Create (if needed) and validate an owner-only state directory.
///
/// The directory is created with a protected owner-only DACL when it is
/// missing. An existing directory is validated, never silently rewritten: an
/// insecure pre-existing directory is refused, exactly like the Unix 0700
/// check.
pub(crate) fn secure_private_state_dir(path: &Path) -> io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            // Rust reports a junction as a reparse-point symlink; either way,
            // an object that redirects the path is never the state directory.
            if metadata.file_type().is_symlink() {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!("state directory {} is a reparse point", path.display()),
                ));
            }
            if !metadata.is_dir() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("state directory {} is not a directory", path.display()),
                ));
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            std::fs::create_dir_all(path)?;
            let attributes = file_attributes(path)?;
            if is_reparse_point(attributes) {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!("state directory {} is a reparse point", path.display()),
                ));
            }
            apply_private_dacl(path)?;
        }
        Err(error) => return Err(error),
    }
    let attributes = file_attributes(path)?;
    if is_reparse_point(attributes) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("state directory {} is a reparse point", path.display()),
        ));
    }
    validate_private_security(path)
}

/// Validate one file inside a state directory: not a reparse point, owned by
/// the current user, and reachable only through an owner-only DACL.
pub(crate) fn verify_private_state_file(path: &Path) -> io::Result<()> {
    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{} is a reparse point", path.display()),
        ));
    }
    if !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} is not a regular file", path.display()),
        ));
    }
    let attributes = file_attributes(path)?;
    if is_reparse_point(attributes) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{} is a reparse point", path.display()),
        ));
    }
    validate_private_security(path)
}

/// Apply the owner-only DACL to one existing regular file and verify it.
///
/// `SetNamedSecurityInfoW` is path-based, so callers must already have
/// established that the path is not a reparse point and is the object they
/// mean to protect. The validation below repeats that check on the current
/// metadata; the caller's own write path remains the authority for identity.
pub(crate) fn secure_private_file(path: &Path) -> io::Result<()> {
    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{} is a reparse point", path.display()),
        ));
    }
    if !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} is not a regular file", path.display()),
        ));
    }
    if is_reparse_point(file_attributes(path)?) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{} is a reparse point", path.display()),
        ));
    }
    apply_private_dacl(path)?;
    validate_private_security(path)
}

/// Open `pid` and read its creation time. `ERROR_INVALID_PARAMETER` means no
/// process with that id exists; any other failure is not proof of absence.
pub(crate) fn process_creation_time(pid: u32) -> io::Result<u64> {
    use windows_sys::Win32::System::Threading::{
        GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    // SAFETY: `OpenProcess` only takes a pid and a right mask.
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle.is_null() {
        return Err(io::Error::last_os_error());
    }
    let handle = OwnedHandle(handle);
    let mut creation: windows_sys::Win32::Foundation::FILETIME =
        // SAFETY: a zeroed FILETIME is a valid "unset" value.
        unsafe { std::mem::zeroed() };
    let mut exit: windows_sys::Win32::Foundation::FILETIME = unsafe { std::mem::zeroed() };
    let mut kernel: windows_sys::Win32::Foundation::FILETIME = unsafe { std::mem::zeroed() };
    let mut user: windows_sys::Win32::Foundation::FILETIME = unsafe { std::mem::zeroed() };
    // SAFETY: the handle is live; every out-parameter is local storage.
    if unsafe { GetProcessTimes(handle.0, &mut creation, &mut exit, &mut kernel, &mut user) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(((creation.dwHighDateTime as u64) << 32) | creation.dwLowDateTime as u64)
}

/// Whether `pid` currently names a live (not terminated) process.
///
/// A terminated process object can stay queryable while a handle keeps it
/// alive; its exit code is what distinguishes "still running" from "exited,
/// not yet reaped". `ERROR_INVALID_PARAMETER` means the id names nothing.
pub(crate) fn process_is_running(pid: u32) -> io::Result<bool> {
    /// `STILL_ACTIVE` — the exit code a running process reports.
    const STILL_ACTIVE: u32 = 259;
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    // SAFETY: `OpenProcess` only takes a pid and a right mask.
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle.is_null() {
        return Err(io::Error::last_os_error());
    }
    let handle = OwnedHandle(handle);
    let mut code = 0u32;
    // SAFETY: the handle is live; `code` is local storage.
    if unsafe { GetExitCodeProcess(handle.0, &mut code) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(code == STILL_ACTIVE)
}

// ---------------------------------------------------------------------------
// Process-tree boundary.
//
// A persistent owner (the Execution Host) must not leave children behind when
// it dies abruptly. Per-task Job Objects with kill-on-drop only run their drop
// code on a graceful exit; `TerminateProcess` (taskkill, a crash, a supervisor
// hard stop) runs no destructors at all. The kernel guarantee that survives an
// abrupt death is `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`: the job itself kills
// every member when its last handle closes, and the process's handle table is
// destroyed by the kernel when the process dies.
//
// The owner therefore puts *itself* into such a job at startup. Windows assigns
// every descendant of a job member to the same job automatically, so the whole
// tree is covered without a per-spawn race. The handle is held by the owner for
// its lifetime; the runtime that launched the owner never holds it, which is
// exactly why the owner can outlive that runtime.
// ---------------------------------------------------------------------------

/// A live kill-on-close Job Object binding the current process and its tree.
///
/// The write scope is process-lifetime by construction: dropping the value
/// closes the job handle and terminates every member, including this process.
/// Owners keep it in scope for as long as they should exist.
pub struct ProcessTreeJob {
    job: OwnedHandle,
}

impl ProcessTreeJob {
    /// Create the job, opt into kill-on-close, and join this process to it.
    ///
    /// Failing here is a real inability to guarantee tree cleanup; callers
    /// that own persistent children must refuse to start instead of running
    /// with a weaker boundary.
    pub fn adopt_current_process() -> io::Result<Self> {
        Self::adopt_process(std::process::id())
    }
    /// Join `pid` to a fresh kill-on-close job instead of this process.
    pub fn adopt_process(pid: u32) -> io::Result<Self> {
        use windows_sys::Win32::System::JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
            SetInformationJobObject,
        };
        use windows_sys::Win32::System::Threading::{
            GetCurrentProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SET_QUOTA,
            PROCESS_TERMINATE,
        };

        // SAFETY: both arguments may be null; the call allocates a fresh job.
        let job = unsafe { CreateJobObjectW(null(), null()) };
        if job.is_null() {
            return Err(io::Error::last_os_error());
        }
        let job = OwnedHandle(job);
        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION =
            // SAFETY: all-zero is the documented "no limits" start state.
            unsafe { std::mem::zeroed() };
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        // SAFETY: `limits` is exactly the size the class expects.
        if unsafe {
            SetInformationJobObject(
                job.0,
                JobObjectExtendedLimitInformation,
                std::ptr::addr_of!(limits).cast(),
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        let process = if pid == std::process::id() {
            // SAFETY: the pseudo-handle names this process.
            unsafe { GetCurrentProcess() }
        } else {
            // SAFETY: `OpenProcess` only takes a pid and a right mask.
            let handle = unsafe {
                OpenProcess(
                    PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SET_QUOTA | PROCESS_TERMINATE,
                    0,
                    pid,
                )
            };
            if handle.is_null() {
                return Err(io::Error::last_os_error());
            }
            handle
        };
        let process_owned = pid != std::process::id();
        let attached = unsafe { AssignProcessToJobObject(job.0, process) };
        let error = io::Error::last_os_error();
        if process_owned {
            // SAFETY: the handle was opened above and is owned here.
            unsafe { windows_sys::Win32::Foundation::CloseHandle(process) };
        }
        if attached == 0 {
            return Err(error);
        }
        Ok(Self { job })
    }

    /// The raw job handle, for callers that need to query or terminate it.
    pub fn raw(&self) -> HANDLE {
        self.job.0
    }
}

impl Drop for ProcessTreeJob {
    fn drop(&mut self) {
        // `OwnedHandle` closes the last handle here, which is what triggers
        // KILL_ON_JOB_CLOSE. When the process dies abruptly the kernel
        // performs the same close itself, so no destructor has to run for the
        // tree to be terminated.
        let _ = &self.job;
    }
}

// ---------------------------------------------------------------------------
// Delete/rename fence.
//
// Mandatory Integrity Control denies *write-up* content changes but does not
// govern deletion: NTFS checks the parent directory's label for delete, so a
// Low-labelled workspace lets a Low child delete or replace a Medium-labelled
// file. The one unprivileged OS mechanism that prevents this is a sharing
// fence: a trusted process holds an open handle on the protected object
// without FILE_SHARE_DELETE. Any later open that asks for DELETE access — the
// first step of delete, rename, and replace — fails with a sharing violation,
// while reads and writes through other handles remain possible (the fence
// shares read and write on purpose: it must not mask the integrity label's
// content denial or block the trusted runtime's own writes).
//
// Renaming a *directory* is not blocked by a handle on the directory itself;
// NTFS refuses it while a file inside is open with the same fence, so the
// caller fences the protected files inside, not just the directory.
// ---------------------------------------------------------------------------

/// An open handle that denies delete sharing on one filesystem object.
///
/// Hold it for the lifetime of the confined command. Dropping it re-allows
/// delete/rename; the handle is process-local and cannot be closed by the
/// confined child.
pub struct PathGuard {
    /// Held for its Drop; never read after construction.
    _handle: OwnedHandle,
}

// SAFETY: a file handle is a process-wide kernel object. It may be closed from
// any thread of the owning process; no thread-affine state is stored.
unsafe impl Send for PathGuard {}

// SAFETY: sharing `&PathGuard` exposes no handle value and performs no I/O;
// the handle is closed only by the owning value's drop.
unsafe impl Sync for PathGuard {}

impl std::fmt::Debug for PathGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PathGuard").finish_non_exhaustive()
    }
}

fn open_guard(path: &Path) -> io::Result<PathGuard> {
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_READ_ATTRIBUTES, FILE_SHARE_READ,
        FILE_SHARE_WRITE, OPEN_EXISTING,
    };
    const SYNCHRONIZE: u32 = 0x0010_0000;
    let path_w = wide(path);
    // The fence must request real data-read access. A handle opened only for
    // FILE_READ_ATTRIBUTES (with or without SYNCHRONIZE) is not consulted by
    // the delete-sharing check on Windows 11 — measured: `del` and `ren`
    // succeed with such a handle open. GENERIC_READ makes every DELETE open
    // fail with a sharing violation while still allowing writes through other
    // handles (the integrity label is the content boundary).
    const GENERIC_READ: u32 = 0x8000_0000;
    let _ = (FILE_READ_ATTRIBUTES, SYNCHRONIZE);
    let access = GENERIC_READ;
    // SAFETY: `path_w` is NUL-terminated; the returned handle is owned by
    // `PathGuard` and closed on drop. No security attributes are requested.
    let handle = unsafe {
        CreateFileW(
            path_w.as_ptr(),
            access,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        let error = std::io::Error::last_os_error();
        return Err(io::Error::new(
            error.kind(),
            format!("open delete fence on {}: {error}", path.display()),
        ));
    }
    Ok(PathGuard {
        _handle: OwnedHandle(handle),
    })
}

/// Open a delete-sharing fence on one file or directory.
pub(crate) fn guard_path(path: &Path) -> io::Result<PathGuard> {
    open_guard(path)
}
