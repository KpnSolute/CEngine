use hmac::{Hmac, Mac};
use sha2::Sha256;
use subtle::ConstantTimeEq;

/// A rotated-out key stays valid this long after rotation.
pub const KEY_OVERLAP_SECS: u64 = 600;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignError {
    ShortKey,
    BadSignature,
}

struct Key {
    id: String,
    secret: Vec<u8>,
}

pub struct Signer {
    current: Key,
    /// Every rotated-out key with the time it was rotated out. Each stays valid for
    /// [`KEY_OVERLAP_SECS`] after its own rotation, however many rotations followed.
    previous: Vec<(Key, u64)>,
}

fn mac(secret: &[u8], payload: &[u8]) -> Vec<u8> {
    let mut m = Hmac::<Sha256>::new_from_slice(secret).expect("any key length");
    m.update(payload);
    m.finalize().into_bytes().to_vec()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

impl Signer {
    /// Keys under 32 bytes are refused.
    pub fn new(key_id: &str, secret: &[u8]) -> Result<Signer, SignError> {
        if secret.len() < 32 {
            return Err(SignError::ShortKey);
        }
        Ok(Signer {
            current: Key {
                id: key_id.into(),
                secret: secret.to_vec(),
            },
            previous: Vec::new(),
        })
    }

    /// Makes `secret` the signing key. The old key keeps verifying for [`KEY_OVERLAP_SECS`].
    pub fn rotate(&mut self, key_id: &str, secret: &[u8], now: u64) -> Result<(), SignError> {
        if secret.len() < 32 {
            return Err(SignError::ShortKey);
        }
        let old = std::mem::replace(
            &mut self.current,
            Key {
                id: key_id.into(),
                secret: secret.to_vec(),
            },
        );
        self.previous
            .retain(|(_, rotated)| now < rotated + KEY_OVERLAP_SECS);
        self.previous.push((old, now));
        Ok(())
    }

    /// Returns `(key id, hex signature)`.
    pub fn sign(&self, payload: &[u8]) -> (String, String) {
        (
            self.current.id.clone(),
            hex(&mac(&self.current.secret, payload)),
        )
    }

    /// Checks a signature made by the key named `key_id`.
    pub fn verify(
        &self,
        key_id: &str,
        payload: &[u8],
        signature_hex: &str,
        now: u64,
    ) -> Result<(), SignError> {
        let key = if key_id == self.current.id {
            &self.current
        } else {
            match self
                .previous
                .iter()
                .find(|(k, rotated)| k.id == key_id && now < rotated + KEY_OVERLAP_SECS)
            {
                Some((k, _)) => k,
                None => return Err(SignError::BadSignature),
            }
        };
        let expected = hex(&mac(&key.secret, payload));
        if bool::from(expected.as_bytes().ct_eq(signature_hex.as_bytes())) {
            Ok(())
        } else {
            Err(SignError::BadSignature)
        }
    }
}
