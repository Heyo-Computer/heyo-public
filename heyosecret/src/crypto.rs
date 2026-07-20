use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use anyhow::{anyhow, Context, Result};
use rand::RngCore;
use sha2::{Digest, Sha256};

const NONCE_LEN: usize = 12;

#[derive(Clone)]
pub struct SecretCrypto {
    cipher: Aes256Gcm,
}

impl SecretCrypto {
    pub fn new(key: [u8; 32]) -> Self {
        Self {
            cipher: Aes256Gcm::new((&key).into()),
        }
    }

    pub fn encrypt(&self, plaintext: &[u8]) -> Result<(Vec<u8>, Vec<u8>, String)> {
        let mut nonce = [0u8; NONCE_LEN];
        rand::thread_rng().fill_bytes(&mut nonce);
        let ciphertext = self
            .cipher
            .encrypt(Nonce::from_slice(&nonce), plaintext)
            .map_err(|_| anyhow!("failed to encrypt secret value"))?;
        let digest = Sha256::digest(plaintext);
        Ok((nonce.to_vec(), ciphertext, hex::encode(digest)))
    }

    pub fn decrypt(&self, nonce: &[u8], ciphertext: &[u8]) -> Result<Vec<u8>> {
        if nonce.len() != NONCE_LEN {
            return Err(anyhow!("invalid secret nonce length"));
        }
        self.cipher
            .decrypt(Nonce::from_slice(nonce), ciphertext)
            .map_err(|_| anyhow!("failed to decrypt secret value"))
            .context("decrypt secret")
    }
}

pub fn random_secret_bytes(len: usize) -> Vec<u8> {
    let mut value = vec![0u8; len];
    rand::thread_rng().fill_bytes(&mut value);
    value
}
