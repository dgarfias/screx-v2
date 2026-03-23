use ring::aead::{self, Aad, LessSafeKey, Nonce, UnboundKey, AES_256_GCM};
use ring::hkdf;
use ring::hmac;

pub const TAG_LEN: usize = 16;

pub struct SessionCipher {
    key: LessSafeKey,
}

impl SessionCipher {
    pub fn new(key_bytes: &[u8; 32]) -> Self {
        let unbound = UnboundKey::new(&AES_256_GCM, key_bytes).expect("AES-256-GCM key");
        Self {
            key: LessSafeKey::new(unbound),
        }
    }

    /// Encrypt `data_len` bytes of `buf` in place, writing the 16-byte tag after.
    /// `buf` must have at least `data_len + TAG_LEN` bytes available.
    /// Returns the total length (data_len + TAG_LEN).
    pub fn encrypt_slice(
        &self,
        nonce_bytes: &[u8; 12],
        aad: &[u8],
        buf: &mut [u8],
        data_len: usize,
    ) -> usize {
        let nonce = Nonce::assume_unique_for_key(*nonce_bytes);
        let tag = self
            .key
            .seal_in_place_separate_tag(nonce, Aad::from(aad), &mut buf[..data_len])
            .expect("AES-GCM encrypt");
        buf[data_len..data_len + TAG_LEN].copy_from_slice(tag.as_ref());
        data_len + TAG_LEN
    }

    /// Decrypt `ciphertext_with_tag` in-place, returning a slice to the plaintext.
    /// Returns `None` if authentication fails.
    pub fn decrypt<'a>(
        &self,
        nonce_bytes: &[u8; 12],
        aad: &[u8],
        ciphertext_with_tag: &'a mut [u8],
    ) -> Option<&'a [u8]> {
        let nonce = Nonce::assume_unique_for_key(*nonce_bytes);
        self.key
            .open_in_place(nonce, Aad::from(aad), ciphertext_with_tag)
            .ok()
            .map(|s| &*s)
    }
}

/// Derive a 32-byte key using HKDF-SHA256.
pub fn hkdf_sha256(ikm: &[u8], salt: &[u8], info: &[u8]) -> [u8; 32] {
    let salt = hkdf::Salt::new(hkdf::HKDF_SHA256, salt);
    let prk = salt.extract(ikm);
    let info_refs = [info];
    let okm = prk
        .expand(&info_refs, &aead::AES_256_GCM)
        .expect("HKDF expand");
    let mut out = [0u8; 32];
    okm.fill(&mut out).expect("HKDF fill");
    out
}

/// Compute HMAC-SHA256.
pub fn hmac_sha256(key: &[u8], data: &[u8]) -> [u8; 32] {
    let k = hmac::Key::new(hmac::HMAC_SHA256, key);
    let tag = hmac::sign(&k, data);
    let mut out = [0u8; 32];
    out.copy_from_slice(tag.as_ref());
    out
}

/// Build a 12-byte nonce for daemon→client packets from header fields.
/// Layout: 0x00 | frame_id(4 BE) | chunk_idx(2 BE) | flags(1) | 0x00(4)
pub fn nonce_server(frame_id: u32, chunk_idx: u16, flags: u8) -> [u8; 12] {
    let mut n = [0u8; 12];
    n[0] = 0x00; // direction: server
    n[1..5].copy_from_slice(&frame_id.to_be_bytes());
    n[5..7].copy_from_slice(&chunk_idx.to_be_bytes());
    n[7] = flags;
    n
}

/// Build a 12-byte nonce for client→daemon packets from a sequence number.
/// Layout: 0xFF | seq_num(4 BE) | 0x00(7)
pub fn nonce_client(seq_num: u32) -> [u8; 12] {
    let mut n = [0u8; 12];
    n[0] = 0xFF; // direction: client
    n[1..5].copy_from_slice(&seq_num.to_be_bytes());
    n
}

/// Build a 12-byte nonce for client→daemon TCP control packets from a sequence number.
/// Layout: 0x7F | seq_num(4 BE) | 0x00(7)
pub fn nonce_control_client(seq_num: u32) -> [u8; 12] {
    let mut n = [0u8; 12];
    n[0] = 0x7F; // direction: client TCP control
    n[1..5].copy_from_slice(&seq_num.to_be_bytes());
    n
}

/// Build a 12-byte nonce for daemon→client TCP control packets from a sequence number.
/// Layout: 0x80 | seq_num(4 BE) | 0x00(7)
pub fn nonce_control_server(seq_num: u32) -> [u8; 12] {
    let mut n = [0u8; 12];
    n[0] = 0x80; // direction: server TCP control
    n[1..5].copy_from_slice(&seq_num.to_be_bytes());
    n
}
