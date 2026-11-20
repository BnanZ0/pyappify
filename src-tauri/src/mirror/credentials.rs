//! CDK storage encrypted for the current Windows account, inside installation data.
#[cfg(debug_assertions)]
use super::download::local_zip_release;
use anyhow::{bail, Result};
use std::path::PathBuf;

fn cdk_path() -> PathBuf {
    crate::utils::path::get_config_dir().join("mirrorchyan.bin")
}

#[cfg(windows)]
fn protect(bytes: &[u8], encrypt: bool) -> Result<Vec<u8>> {
    use windows_sys::Win32::{Foundation::LocalFree, Security::Cryptography::*};
    let input = CRYPT_INTEGER_BLOB {
        cbData: bytes.len().try_into()?,
        pbData: bytes.as_ptr() as *mut u8,
    };
    let mut output: CRYPT_INTEGER_BLOB = unsafe { std::mem::zeroed() };
    let ok = unsafe {
        if encrypt {
            CryptProtectData(
                &input,
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            )
        } else {
            CryptUnprotectData(
                &input,
                std::ptr::null_mut(),
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            )
        }
    };
    if ok == 0 {
        bail!("Unable to access the encrypted MirrorChyan CDK. Save the CDK again.");
    }
    let result =
        unsafe { std::slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec() };
    unsafe {
        LocalFree(output.pbData as _);
    }
    Ok(result)
}
#[cfg(not(windows))]
fn protect(_: &[u8], _: bool) -> Result<Vec<u8>> {
    bail!("MirrorChyan CDK storage requires Windows")
}

pub(super) fn read_cdk() -> Result<Option<String>> {
    let path = cdk_path();
    if !path.exists() {
        return Ok(None);
    }
    Ok(Some(String::from_utf8(protect(
        &std::fs::read(path)?,
        false,
    )?)?))
}

#[tauri::command]
pub async fn mirrorchyan_has_cdk() -> Result<bool, String> {
    read_cdk()
        .map(|cdk| cdk.is_some_and(|key| !key.trim().is_empty()))
        .map_err(|e| e.to_string())
}

pub fn require_cdk() -> Result<()> {
    #[cfg(debug_assertions)]
    if local_zip_release()?.is_some() {
        return Ok(());
    }
    check_cdk(read_cdk()?.as_deref())
}

fn check_cdk(cdk: Option<&str>) -> Result<()> {
    if cdk.is_none_or(|key| key.trim().is_empty()) {
        bail!("Configure a MirrorChyan CDK in Settings before installing or updating.");
    }
    Ok(())
}
#[tauri::command]
pub async fn mirrorchyan_set_cdk(cdk: String) -> Result<(), String> {
    (|| -> Result<()> {
        let path = cdk_path();
        if cdk.trim().is_empty() {
            if path.exists() {
                crate::extensions::file_operations::remove_file(
                    &path,
                    "Mirror clear saved CDK",
                    &|| false,
                )?;
            }
        } else {
            std::fs::create_dir_all(crate::utils::path::get_config_dir())?;
            crate::extensions::file_operations::atomic_write(
                &path,
                &protect(cdk.trim().as_bytes(), true)?,
            )?;
        }
        Ok(())
    })()
    .map_err(|e| e.to_string())
}
