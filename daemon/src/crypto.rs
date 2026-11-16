use ring::aead::{self, Aad, LessSafeKey, Nonce, UnboundKey, AES_256_GCM};
use ring::hkdf;
use ring::hmac;
use std::sync::atomic::{AtomicU32, Ordering};

// Never recycle namespaces, even when a sender is recreated under the same key.
static NEXT_SERVER_GENERATION: AtomicU32 = AtomicU32::new(1);

pub struct ServerPacketSequence {
    pub generation: u32,
    next_frame: Option<u32>,
}

impl ServerPacketSequence {
    pub fn new() -> Self {
        let generation = NEXT_SERVER_GENERATION
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1))
            .expect("server packet namespace exhausted");
        Self {
            generation,
            next_frame: Some(0),
        }
    }

    /// Reserve before encryption/sending: a partial send or error must not reuse a nonce.
    pub fn take_frame(&mut self) -> anyhow::Result<u32> {
        let frame = self
            .next_frame
            .ok_or_else(|| anyhow::anyhow!("server frame counter exhausted"))?;
        self.next_frame = frame.checked_add(1);
        Ok(frame)
    }
}

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
/// Layout: 0x00 | frame_id(4 BE) | chunk_idx(2 BE) | flags(1) | generation(4 BE)
pub fn nonce_server(frame_id: u32, chunk_idx: u16, flags: u8, generation: u32) -> [u8; 12] {
    let mut n = [0u8; 12];
    n[0] = 0x00; // direction: server
    n[1..5].copy_from_slice(&frame_id.to_be_bytes());
    n[5..7].copy_from_slice(&chunk_idx.to_be_bytes());
    n[7] = flags;
    n[8..12].copy_from_slice(&generation.to_be_bytes());
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn recreated_and_parallel_senders_never_reuse_a_namespace() {
        let handles: Vec<_> = (0..8)
            .map(|_| {
                std::thread::spawn(|| {
                    (0..64)
                        .map(|_| ServerPacketSequence::new().generation)
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        let generations: Vec<_> = handles
            .into_iter()
            .flat_map(|h| h.join().unwrap())
            .collect();
        assert_eq!(
            generations.len(),
            generations.iter().collect::<HashSet<_>>().len()
        );
        assert!(generations.iter().all(|g| *g > 0));
    }

    #[test]
    fn frame_reservations_are_consumed_and_never_wrap() {
        let mut sequence = ServerPacketSequence::new();
        assert_eq!(sequence.take_frame().unwrap(), 0);
        assert_eq!(sequence.take_frame().unwrap(), 1);
        sequence.next_frame = Some(u32::MAX);
        assert_eq!(sequence.take_frame().unwrap(), u32::MAX);
        assert!(sequence.take_frame().is_err());
        assert!(sequence.take_frame().is_err());
    }

    #[test]
    fn media_nonce_and_aead_match_swift_fixture() {
        let nonce = nonce_server(0x01020304, 2, 0x11, 0x090a0b0c);
        assert_eq!(nonce, [0, 1, 2, 3, 4, 0, 2, 0x11, 9, 10, 11, 12]);
        let header = [
            1, 2, 3, 4, 0, 2, 0, 3, 0, 1, 0x11, 1, 0, 4, 5, 6, 7, 8, 9, 10, 11, 12,
        ];
        let cipher = SessionCipher::new(&[0x42; 32]);
        let mut payload = [0u8; 20];
        payload[..4].copy_from_slice(&[0xde, 0xad, 0xbe, 0xef]);
        cipher.encrypt_slice(&nonce, &header, &mut payload, 4);
        assert_eq!(
            payload,
            [
                0x8f, 0x85, 0x5c, 0x3b, 0xad, 0x35, 0x03, 0x12, 0xbe, 0x50, 0x87, 0xc8, 0xfa, 0xe9,
                0xdd, 0x7e, 0xdb, 0x52, 0xe6, 0x77
            ]
        );
        let mut bad_header = header;
        bad_header[18] ^= 1;
        let mut tampered = payload;
        assert!(cipher.decrypt(&nonce, &bad_header, &mut tampered).is_none());
        assert_eq!(
            cipher.decrypt(&nonce, &header, &mut payload).unwrap(),
            &[0xde, 0xad, 0xbe, 0xef]
        );
    }
}
