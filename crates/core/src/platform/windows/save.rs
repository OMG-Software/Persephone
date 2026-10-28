//! Saving a file without widening who can read it (ADR-0029). The partial
//! file is created with a protected DACL that lets only its owner in (the
//! Owner Rights SID), and no other process may open it while it is written,
//! so nothing inherited from the folder exposes the decrypted bytes. It then
//! replaces a file already there with `ReplaceFileW`, which keeps that
//! file's DACL, or is moved into place with its own owner-only DACL.

use std::{
    fs::File,
    io,
    os::windows::{ffi::OsStrExt, io::FromRawHandle},
    path::Path,
    ptr,
};
use windows_sys::{
    Win32::{
        Foundation::{ERROR_FILE_NOT_FOUND, GENERIC_WRITE, INVALID_HANDLE_VALUE},
        Security::{
            ACL, ACL_REVISION, AddAccessAllowedAce, CreateWellKnownSid, InitializeAcl,
            InitializeSecurityDescriptor, PSECURITY_DESCRIPTOR, SE_DACL_PROTECTED,
            SECURITY_ATTRIBUTES, SECURITY_DESCRIPTOR, SECURITY_MAX_SID_SIZE,
            SetSecurityDescriptorControl, SetSecurityDescriptorDacl, WELL_KNOWN_SID_TYPE,
            WinCreatorOwnerRightsSid,
        },
        Storage::FileSystem::{
            CREATE_NEW, CreateFileW, FILE_ALL_ACCESS, FILE_ATTRIBUTE_NORMAL,
            MOVEFILE_WRITE_THROUGH, MoveFileExW, ReplaceFileW,
        },
        System::SystemServices::SECURITY_DESCRIPTOR_REVISION,
    },
    core::BOOL,
};

fn wide(path: &Path) -> Vec<u16> {
    path.as_os_str().encode_wide().chain(Some(0)).collect()
}

fn check(result: BOOL) -> io::Result<()> {
    if result == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Calls `with` with a security descriptor whose protected DACL allows full
/// access to each of `sids` and to nobody else. The descriptor lives only for
/// the call.
pub(crate) fn with_dacl<T>(
    sids: &[WELL_KNOWN_SID_TYPE],
    with: impl FnOnce(PSECURITY_DESCRIPTOR) -> io::Result<T>,
) -> io::Result<T> {
    const SID_WORDS: usize = (SECURITY_MAX_SID_SIZE as usize).div_ceil(4);
    let mut buffers = vec![[0u32; SID_WORDS]; sids.len()];
    // An ACL header, then per ACE a header, a mask and a SID: a whole
    // maximum-size SID per ACE leaves room to spare. u32 keeps it aligned.
    let mut acl = vec![0u32; (8 + sids.len() * (8 + SECURITY_MAX_SID_SIZE as usize)).div_ceil(4)];
    let acl_bytes = u32::try_from(acl.len() * 4).map_err(|_| io::ErrorKind::InvalidInput)?;
    let acl_ptr = acl.as_mut_ptr().cast::<ACL>();
    // SAFETY: `acl` is a writable, aligned buffer of `acl_bytes` bytes.
    check(unsafe { InitializeAcl(acl_ptr, acl_bytes, ACL_REVISION) })?;
    for (kind, buffer) in sids.iter().zip(buffers.iter_mut()) {
        let mut size = SECURITY_MAX_SID_SIZE;
        let sid = buffer.as_mut_ptr().cast();
        // SAFETY: `buffer` holds `SECURITY_MAX_SID_SIZE` writable bytes; a
        // well-known SID without a domain needs no domain SID.
        check(unsafe { CreateWellKnownSid(*kind, ptr::null_mut(), sid, &mut size) })?;
        // SAFETY: `acl` was initialized above and has room for this ACE;
        // `sid` is the valid SID just written.
        check(unsafe { AddAccessAllowedAce(acl_ptr, ACL_REVISION, FILE_ALL_ACCESS, sid) })?;
    }
    let mut descriptor = SECURITY_DESCRIPTOR::default();
    let pointer: PSECURITY_DESCRIPTOR = (&raw mut descriptor).cast();
    // SAFETY: `descriptor` is a SECURITY_DESCRIPTOR that outlives every use
    // of `pointer`, and `acl` outlives the descriptor's use in `with`.
    unsafe {
        check(InitializeSecurityDescriptor(
            pointer,
            SECURITY_DESCRIPTOR_REVISION,
        ))?;
        check(SetSecurityDescriptorDacl(pointer, 1, acl_ptr, 0))?;
        check(SetSecurityDescriptorControl(
            pointer,
            SE_DACL_PROTECTED,
            SE_DACL_PROTECTED,
        ))?;
    }
    with(pointer)
}

/// Creates `path`, which must not exist, for writing: only its owner may
/// open it, and no other handle may be opened while this one is.
pub(crate) fn create_private_file(path: &Path) -> io::Result<File> {
    let name = wide(path);
    with_dacl(&[WinCreatorOwnerRightsSid], |descriptor| {
        let attributes = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor,
            bInheritHandle: 0,
        };
        // SAFETY: `name` is NUL-terminated and `attributes` points at a
        // descriptor valid for this call.
        let handle = unsafe {
            CreateFileW(
                name.as_ptr(),
                GENERIC_WRITE,
                0,
                &attributes,
                CREATE_NEW,
                FILE_ATTRIBUTE_NORMAL,
                ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `handle` is a new, open file handle that nothing else owns.
        Ok(unsafe { File::from_raw_handle(handle) })
    })
}

/// Puts the written `partial` file at `path`. A file already there is
/// replaced with `ReplaceFileW`, which keeps its DACL; with none there,
/// `partial` is moved, never over a file that appeared meanwhile.
pub(crate) fn replace_file(partial: &Path, path: &Path) -> io::Result<()> {
    let (partial, path) = (wide(partial), wide(path));
    // SAFETY: both names are NUL-terminated; no backup file, no reserved
    // arguments.
    let replaced = unsafe {
        ReplaceFileW(
            path.as_ptr(),
            partial.as_ptr(),
            ptr::null(),
            0,
            ptr::null(),
            ptr::null(),
        )
    };
    if replaced != 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() != Some(ERROR_FILE_NOT_FOUND as i32) {
        return Err(error);
    }
    // SAFETY: both names are NUL-terminated.
    check(unsafe { MoveFileExW(partial.as_ptr(), path.as_ptr(), MOVEFILE_WRITE_THROUGH) })
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::Security::{
        ACL_SIZE_INFORMATION, AclSizeInformation, DACL_SECURITY_INFORMATION, GetAclInformation,
        GetFileSecurityW, GetSecurityDescriptorControl, GetSecurityDescriptorDacl,
        SetFileSecurityW, WinWorldSid,
    };

    /// Whether the file's DACL is protected, and how many ACEs it has.
    fn dacl(path: &Path) -> (bool, u32) {
        let name = wide(path);
        let mut buffer = vec![0u64; 512];
        let mut needed = 0;
        let descriptor: PSECURITY_DESCRIPTOR = buffer.as_mut_ptr().cast();
        // SAFETY: test-only reads into buffers that outlive each call.
        unsafe {
            check(GetFileSecurityW(
                name.as_ptr(),
                DACL_SECURITY_INFORMATION,
                descriptor,
                (buffer.len() * 8) as u32,
                &mut needed,
            ))
            .unwrap();
            let (mut control, mut revision) = (0u16, 0u32);
            check(GetSecurityDescriptorControl(
                descriptor,
                &mut control,
                &mut revision,
            ))
            .unwrap();
            let (mut present, mut defaulted, mut acl) = (0, 0, ptr::null_mut());
            check(GetSecurityDescriptorDacl(
                descriptor,
                &mut present,
                &mut acl,
                &mut defaulted,
            ))
            .unwrap();
            let mut size = ACL_SIZE_INFORMATION::default();
            check(GetAclInformation(
                acl,
                (&raw mut size).cast(),
                size_of::<ACL_SIZE_INFORMATION>() as u32,
                AclSizeInformation,
            ))
            .unwrap();
            (control & SE_DACL_PROTECTED != 0, size.AceCount)
        }
    }

    /// GIVEN a new file WHEN created private THEN its DACL is protected with
    /// one entry (its owner), and a second handle cannot be opened while it
    /// is written.
    #[test]
    fn given_a_new_private_file_then_only_its_owner_is_let_in() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("partial");
        let mut file = create_private_file(&path).unwrap();
        assert!(std::fs::File::open(&path).is_err(), "no second handle");
        std::io::Write::write_all(&mut file, b"bytes").unwrap();
        drop(file);
        assert_eq!(dacl(&path), (true, 1));
        assert!(create_private_file(&path).is_err(), "never over a file");
    }

    /// GIVEN a file already there with its own DACL (two entries) WHEN a
    /// private file replaces it THEN the bytes are new and the DACL is the
    /// old one; with no file there, the private file moves in with its own.
    #[test]
    fn given_a_replaced_file_then_its_dacl_is_kept() {
        let dir = tempfile::tempdir().unwrap();
        let existing = dir.path().join("existing.pdf");
        std::fs::write(&existing, b"old").unwrap();
        let name = wide(&existing);
        with_dacl(&[WinCreatorOwnerRightsSid, WinWorldSid], |descriptor| {
            // SAFETY: test-only; the descriptor is valid for this call.
            check(unsafe { SetFileSecurityW(name.as_ptr(), DACL_SECURITY_INFORMATION, descriptor) })
        })
        .unwrap();
        assert_eq!(dacl(&existing), (true, 2));

        let partial = dir.path().join("partial");
        std::io::Write::write_all(&mut create_private_file(&partial).unwrap(), b"new").unwrap();
        replace_file(&partial, &existing).unwrap();
        assert_eq!(std::fs::read(&existing).unwrap(), b"new");
        assert_eq!(dacl(&existing), (true, 2));
        assert!(!partial.exists());

        let fresh = dir.path().join("fresh.pdf");
        std::io::Write::write_all(&mut create_private_file(&partial).unwrap(), b"fresh").unwrap();
        replace_file(&partial, &fresh).unwrap();
        assert_eq!(std::fs::read(&fresh).unwrap(), b"fresh");
        assert_eq!(dacl(&fresh), (true, 1));
    }
}
