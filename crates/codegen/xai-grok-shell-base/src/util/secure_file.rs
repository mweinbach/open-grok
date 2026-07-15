//! Cross-platform secure file operations.
//!
//! This module provides utilities for creating files with restrictive permissions
//! that limit access to the current user only. This is critical for storing
//! sensitive data like authentication tokens.
//!
//! ## Security Model
//!
//! - **Unix**: Files are created with mode 0o600 (owner read/write only)
//! - **Windows**: Files are created with ACLs that grant access only to the current user
//!
//! ## Encryption Consideration
//!
//! While this module restricts file access at the OS level, the token is stored in
//! plaintext. For additional security in high-risk environments, consider:
//! - Using the operating system's keychain/credential manager (e.g., macOS Keychain,
//!   Windows Credential Manager, Linux Secret Service)
//! - Encrypting the token with a key derived from system-specific entropy
//!
//! The current approach balances security with simplicity - OS file permissions
//! provide reasonable protection for most use cases, and the token is already
//! short-lived (7-30 days TTL with automatic refresh).

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::Path;

#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;

/// Creates or opens a file with secure permissions (owner read/write only).
///
/// On Unix, this sets mode 0o600. On Windows, this restricts the file's ACL
/// to grant access only to the current user.
///
/// # Arguments
/// * `path` - The path to the file to create/open
/// * `contents` - The data to write to the file
///
/// # Returns
/// An `io::Result<()>` indicating success or failure.
///
/// # Example
/// ```ignore
/// use xai_grok_shell_base::util::secure_file::write_secure_file;
///
/// let token = "secret_token";
/// write_secure_file("/path/to/auth.json", token.as_bytes())?;
/// ```
pub fn write_secure_file(path: &Path, contents: &[u8]) -> io::Result<()> {
    // Ensure parent directory exists
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    // Create the file with secure permissions
    let mut file = open_secure_file(path)?;
    file.write_all(contents)?;
    file.flush()?;

    // On Windows, we need to set permissions after file creation
    #[cfg(windows)]
    {
        set_windows_secure_permissions(path)?;
    }

    Ok(())
}

/// Opens a file for writing with secure permissions set during creation (Unix)
/// or prepares it for permission setting after creation (Windows).
pub fn open_secure_file(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.truncate(true).write(true).create(true);

    #[cfg(unix)]
    {
        // Set file mode to 0o600 (owner read/write only) during creation
        options.mode(0o600);
    }

    options.open(path)
}

/// Sets Windows-specific secure permissions on a file.
///
/// This function modifies the file's ACL to:
/// 1. Remove inherited permissions
/// 2. Grant full control only to the current user
///
/// This is equivalent to Unix mode 0o600.
#[cfg(windows)]
pub fn set_windows_secure_permissions(path: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows::Win32::Foundation::{CloseHandle, HLOCAL, LocalFree};
    use windows::Win32::Security::Authorization::{
        EXPLICIT_ACCESS_W, SE_FILE_OBJECT, SET_ACCESS, SetEntriesInAclW, SetNamedSecurityInfoW,
        TRUSTEE_IS_SID, TRUSTEE_IS_USER, TRUSTEE_W,
    };
    use windows::Win32::Security::{
        ACE_FLAGS, ACL, DACL_SECURITY_INFORMATION, GetTokenInformation,
        PROTECTED_DACL_SECURITY_INFORMATION, TOKEN_QUERY, TOKEN_USER, TokenUser,
    };
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    use windows::core::PCWSTR;

    unsafe {
        // Get current process token
        let mut token_handle = windows::Win32::Foundation::HANDLE::default();
        OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token_handle)
            .map_err(|e| io::Error::new(io::ErrorKind::PermissionDenied, e))?;

        // Get token user size
        let mut return_length = 0u32;
        let _ = GetTokenInformation(token_handle, TokenUser, None, 0, &mut return_length);

        // Get token user (current user's SID)
        let mut token_user_buffer = vec![0u8; return_length as usize];
        GetTokenInformation(
            token_handle,
            TokenUser,
            Some(token_user_buffer.as_mut_ptr() as *mut _),
            return_length,
            &mut return_length,
        )
        .map_err(|e| {
            let _ = CloseHandle(token_handle);
            io::Error::new(io::ErrorKind::PermissionDenied, e)
        })?;

        // The TOKEN_USER structure starts with a SID_AND_ATTRIBUTES which has PSID as first field
        let token_user = &*(token_user_buffer.as_ptr() as *const TOKEN_USER);
        let user_sid = token_user.User.Sid;

        // Create explicit access entry for current user only
        // GENERIC_ALL = 0x10000000
        let explicit_access = EXPLICIT_ACCESS_W {
            grfAccessPermissions: 0x10000000, // GENERIC_ALL
            grfAccessMode: SET_ACCESS,
            grfInheritance: ACE_FLAGS(0), // No inheritance for files
            Trustee: TRUSTEE_W {
                pMultipleTrustee: std::ptr::null_mut(),
                MultipleTrusteeOperation:
                    windows::Win32::Security::Authorization::NO_MULTIPLE_TRUSTEE,
                TrusteeForm: TRUSTEE_IS_SID,
                TrusteeType: TRUSTEE_IS_USER,
                ptstrName: windows::core::PWSTR(user_sid.0 as *mut u16),
            },
        };

        // Create new ACL with only this entry
        let mut new_acl: *mut ACL = std::ptr::null_mut();
        let result = SetEntriesInAclW(Some(&[explicit_access]), None, &mut new_acl);
        if result.0 != 0 {
            let _ = CloseHandle(token_handle);
            return Err(io::Error::from_raw_os_error(result.0 as i32));
        }

        // Convert path to wide string for Windows API
        let wide_path: Vec<u16> = path
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();

        // Set the new DACL on the file, removing inherited permissions
        let result = SetNamedSecurityInfoW(
            PCWSTR::from_raw(wide_path.as_ptr()),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            None, // psidOwner: not changing the owner
            None, // psidGroup: not changing the primary group
            Some(new_acl),
            None,
        );

        // Clean up
        let _ = LocalFree(Some(HLOCAL(new_acl as *mut _)));
        let _ = CloseHandle(token_handle);

        if result.0 != 0 {
            return Err(io::Error::from_raw_os_error(result.0 as i32));
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn test_write_secure_file_creates_file() {
        let temp_dir = tempfile::tempdir().unwrap();
        let file_path = temp_dir.path().join("test_secure.txt");

        write_secure_file(&file_path, b"test content").unwrap();

        assert!(file_path.exists());
        let content = fs::read_to_string(&file_path).unwrap();
        assert_eq!(content, "test content");
    }

    #[test]
    fn test_write_secure_file_creates_parent_dirs() {
        let temp_dir = tempfile::tempdir().unwrap();
        let file_path = temp_dir.path().join("nested").join("dir").join("test.txt");

        write_secure_file(&file_path, b"nested content").unwrap();

        assert!(file_path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn test_unix_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let temp_dir = tempfile::tempdir().unwrap();
        let file_path = temp_dir.path().join("test_perms.txt");

        write_secure_file(&file_path, b"secure content").unwrap();

        let metadata = fs::metadata(&file_path).unwrap();
        let mode = metadata.permissions().mode();
        // Check that only owner has read/write (0o600), ignoring file type bits
        assert_eq!(mode & 0o777, 0o600);
    }
}
