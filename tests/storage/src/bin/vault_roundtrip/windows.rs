#![cfg(windows)]

use std::{
    ffi::OsStr,
    io,
    os::windows::ffi::OsStrExt,
    ptr::null_mut,
    time::{SystemTime, UNIX_EPOCH},
};

use windows_sys::Win32::{
    Foundation::{ERROR_NOT_FOUND, GetLastError},
    Security::Credentials::{
        CRED_PERSIST_SESSION, CRED_TYPE_GENERIC, CREDENTIALW, CredDeleteW, CredFree, CredReadW,
        CredWriteW,
    },
};

const SYNTHETIC_BLOB: [u8; 32] = [0xA5; 32];

struct Credential(*mut CREDENTIALW);

impl Drop for Credential {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { CredFree(self.0.cast()) };
        }
    }
}

struct Cleanup {
    target: Vec<u16>,
    written: bool,
}

impl Cleanup {
    fn delete(&mut self) -> io::Result<()> {
        if self.written {
            if unsafe { CredDeleteW(self.target.as_ptr(), CRED_TYPE_GENERIC, 0) } == 0 {
                return Err(last_error("CredDeleteW"));
            }
            self.written = false;
        }
        Ok(())
    }
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        if self.written {
            unsafe { CredDeleteW(self.target.as_ptr(), CRED_TYPE_GENERIC, 0) };
        }
    }
}

pub fn main() {
    if let Err(error) = run() {
        eprintln!("vault probe failed: {error}");
        std::process::exit(1);
    }
}

fn run() -> io::Result<()> {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(io::Error::other)?
        .as_nanos();
    let target_name = format!(
        "Persephone.P0.VaultProbe.{}.{}",
        std::process::id(),
        timestamp
    );
    let target: Vec<u16> = OsStr::new(&target_name)
        .encode_wide()
        .chain(Some(0))
        .collect();

    if read_credential(&target)?.is_some() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "unique target unexpectedly existed before the probe; refusing to overwrite it",
        ));
    }

    let mut credential: CREDENTIALW = unsafe { std::mem::zeroed() };
    credential.Type = CRED_TYPE_GENERIC;
    credential.TargetName = target.as_ptr().cast_mut();
    credential.CredentialBlobSize = SYNTHETIC_BLOB.len() as u32;
    credential.CredentialBlob = SYNTHETIC_BLOB.as_ptr().cast_mut();
    credential.Persist = CRED_PERSIST_SESSION;
    credential.UserName = null_mut();

    if unsafe { CredWriteW(&credential, 0) } == 0 {
        return Err(last_error("CredWriteW"));
    }
    let mut cleanup = Cleanup {
        target,
        written: true,
    };

    let loaded = read_credential(&cleanup.target)?.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "credential could not be read after write",
        )
    })?;
    let persisted = unsafe {
        let record = &*loaded.0;
        record.CredentialBlobSize as usize == SYNTHETIC_BLOB.len()
            && !record.CredentialBlob.is_null()
            && std::slice::from_raw_parts(record.CredentialBlob, record.CredentialBlobSize as usize)
                == SYNTHETIC_BLOB
    };
    if !persisted {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "credential read-back did not match the synthetic 32-byte blob",
        ));
    }

    cleanup.delete()?;
    if read_credential(&cleanup.target)?.is_some() {
        return Err(io::Error::other(
            "credential was still readable after successful deletion",
        ));
    }

    println!(
        "target=unique_pid_timestamp; preexisting=absent; persist=current_logon_session; synthetic_blob_bytes=32; readback=match; cleanup=deleted; postdelete=ERROR_NOT_FOUND"
    );
    Ok(())
}

fn read_credential(target: &[u16]) -> io::Result<Option<Credential>> {
    let mut credential = null_mut();
    if unsafe { CredReadW(target.as_ptr(), CRED_TYPE_GENERIC, 0, &mut credential) } != 0 {
        return Ok(Some(Credential(credential)));
    }
    let error = unsafe { GetLastError() };
    if error == ERROR_NOT_FOUND {
        Ok(None)
    } else {
        Err(io::Error::from_raw_os_error(error as i32))
    }
}

fn last_error(api: &str) -> io::Error {
    let error = unsafe { GetLastError() };
    io::Error::new(
        io::Error::from_raw_os_error(error as i32).kind(),
        format!("{api} failed with Win32 error {error}"),
    )
}
