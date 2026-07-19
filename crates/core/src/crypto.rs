use std::fs;
use std::path::{Path, PathBuf};

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use anyhow::{Context, Result, bail};
use rand::RngCore;
use windows::Win32::Foundation::{HLOCAL, LocalFree};
use windows::Win32::Security::Cryptography::{
    CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData, CryptUnprotectData,
};
use windows::core::PCWSTR;

const KEY_FILE: &str = "session.key";

pub struct SessionCrypto {
    key: [u8; 32],
    key_path: PathBuf,
}

impl SessionCrypto {
    pub fn create(session_dir: &Path) -> Result<Self> {
        let key_path = session_dir.join(KEY_FILE);
        let mut key = [0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut key);
        let protected = protect_for_current_user(&key)?;
        fs::write(&key_path, protected).with_context(|| {
            format!(
                "failed to write protected session key to {}",
                key_path.display()
            )
        })?;
        Ok(Self { key, key_path })
    }

    pub fn open(session_dir: &Path) -> Result<Self> {
        let key_path = session_dir.join(KEY_FILE);
        let protected = fs::read(&key_path).with_context(|| {
            format!(
                "failed to read protected session key from {}",
                key_path.display()
            )
        })?;
        let clear = unprotect_for_current_user(&protected)?;
        if clear.len() != 32 {
            bail!("protected session key had an unexpected length");
        }
        let mut key = [0u8; 32];
        key.copy_from_slice(&clear);
        Ok(Self { key, key_path })
    }

    pub fn encrypt(&self, plaintext: &[u8]) -> Result<Vec<u8>> {
        let cipher = Aes256Gcm::new_from_slice(&self.key).expect("AES-256 key length");
        let mut nonce = [0u8; 12];
        rand::rngs::OsRng.fill_bytes(&mut nonce);
        let encrypted = cipher
            .encrypt(Nonce::from_slice(&nonce), plaintext)
            .map_err(|_| anyhow::anyhow!("AES-GCM encryption failed"))?;
        let mut result = Vec::with_capacity(nonce.len() + encrypted.len());
        result.extend_from_slice(&nonce);
        result.extend_from_slice(&encrypted);
        Ok(result)
    }

    pub fn decrypt(&self, payload: &[u8]) -> Result<Vec<u8>> {
        if payload.len() < 13 {
            bail!("encrypted payload is truncated");
        }
        let (nonce, ciphertext) = payload.split_at(12);
        let cipher = Aes256Gcm::new_from_slice(&self.key).expect("AES-256 key length");
        cipher
            .decrypt(Nonce::from_slice(nonce), ciphertext)
            .map_err(|_| anyhow::anyhow!("AES-GCM authentication failed"))
    }

    pub fn key_path(&self) -> &Path {
        &self.key_path
    }
}

fn protect_for_current_user(cleartext: &[u8]) -> Result<Vec<u8>> {
    crypt_data(cleartext, true)
}

fn unprotect_for_current_user(ciphertext: &[u8]) -> Result<Vec<u8>> {
    crypt_data(ciphertext, false)
}

fn crypt_data(input: &[u8], protect: bool) -> Result<Vec<u8>> {
    let input_len = u32::try_from(input.len()).context("DPAPI input is too large")?;
    let input_blob = CRYPT_INTEGER_BLOB {
        cbData: input_len,
        pbData: input.as_ptr().cast_mut(),
    };
    let mut output_blob = CRYPT_INTEGER_BLOB::default();

    if protect {
        unsafe {
            CryptProtectData(
                &input_blob,
                PCWSTR::null(),
                None,
                None,
                None,
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output_blob,
            )
        }
        .context("CryptProtectData failed")?;
    } else {
        unsafe {
            CryptUnprotectData(
                &input_blob,
                None,
                None,
                None,
                None,
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output_blob,
            )
        }
        .context("CryptUnprotectData failed")?;
    }

    let output =
        unsafe { std::slice::from_raw_parts(output_blob.pbData, output_blob.cbData as usize) }
            .to_vec();
    unsafe {
        let _ = LocalFree(Some(HLOCAL(output_blob.pbData.cast())));
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dpapi_and_aes_round_trip_on_the_real_windows_user() {
        let dir = std::env::temp_dir().join(format!("cdxvidext-crypto-{}", uuid::Uuid::now_v7()));
        fs::create_dir_all(&dir).unwrap();
        let crypto = SessionCrypto::create(&dir).unwrap();
        let payload = b"sensitive keyboard payload";
        let encrypted = crypto.encrypt(payload).unwrap();
        assert!(
            !encrypted
                .windows(payload.len())
                .any(|window| window == payload)
        );
        let reopened = SessionCrypto::open(&dir).unwrap();
        assert_eq!(reopened.decrypt(&encrypted).unwrap(), payload);
        fs::remove_dir_all(dir).unwrap();
    }
}
