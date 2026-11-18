//! CPACE-X25519-SHA512, initiator/responder transcript, draft-irtf-cfrg-cpace-21.
//! The PAKE authenticates a fresh pairing; QUIC/TLS encrypts all transport traffic.
use anyhow::{ensure, Result};
use ring::{
    digest, hmac,
    rand::{SecureRandom, SystemRandom},
};
use x25519_dalek::{PublicKey, StaticSecret};

unsafe extern "C" {
    fn screx_cpace_map(out: *mut u8, representative: *const u8);
}

pub fn random<const N: usize>() -> [u8; N] {
    let mut bytes = [0; N];
    SystemRandom::new().fill(&mut bytes).expect("system RNG");
    bytes
}

/// Unbiased eight-digit code, including leading zeroes.
pub fn pin() -> String {
    const RANGE: u32 = 100_000_000;
    const LIMIT: u32 = u32::MAX - u32::MAX % RANGE;
    loop {
        let n = u32::from_le_bytes(random());
        if n < LIMIT {
            return format!("{:08}", n % RANGE);
        }
    }
}

pub fn lv(fields: &[&[u8]]) -> Vec<u8> {
    let mut result = Vec::new();
    for field in fields {
        let mut n = field.len();
        loop {
            result.push((n as u8 & 127) | if n >= 128 { 128 } else { 0 });
            n >>= 7;
            if n == 0 {
                break;
            }
        }
        result.extend_from_slice(field);
    }
    result
}

pub fn generator(password: &[u8], ci: &[u8], sid: &[u8]) -> [u8; 32] {
    let prefix = lv(&[b"CPace255", password]);
    let padding = vec![0; 128usize.saturating_sub(1 + prefix.len())];
    let input = lv(&[b"CPace255", password, &padding, ci, sid]);
    let hash = digest::digest(&digest::SHA512, &input);
    let mut out = [0; 32];
    // SAFETY: both buffers provide at least 32 bytes; the C routine masks bit 255.
    unsafe {
        screx_cpace_map(out.as_mut_ptr(), hash.as_ref().as_ptr());
    }
    out
}

pub struct Exchange {
    secret: StaticSecret,
    pub share: [u8; 32],
}
impl Exchange {
    pub fn new(password: &[u8], ci: &[u8], sid: &[u8]) -> Self {
        Self::from_scalar(password, ci, sid, random())
    }
    fn from_scalar(password: &[u8], ci: &[u8], sid: &[u8], scalar: [u8; 32]) -> Self {
        let secret = StaticSecret::from(scalar);
        let share = secret
            .diffie_hellman(&PublicKey::from(generator(password, ci, sid)))
            .to_bytes();
        Self { secret, share }
    }
    pub fn finish(
        self,
        peer: [u8; 32],
        sid: &[u8],
        ya: &[u8],
        ada: &[u8],
        yb: &[u8],
        adb: &[u8],
    ) -> Result<Vec<u8>> {
        let shared = self.secret.diffie_hellman(&PublicKey::from(peer));
        ensure!(
            shared.was_contributory(),
            "CPace rejected a low-order peer share"
        );
        let mut input = lv(&[b"CPace255_ISK", sid, shared.as_bytes()]);
        input.extend(lv(&[ya, ada, yb, adb]));
        Ok(digest::digest(&digest::SHA512, &input).as_ref().to_vec())
    }
}

pub fn confirm(key: &[u8], role: &[u8], binding: &[u8]) -> Vec<u8> {
    hmac::sign(
        &hmac::Key::new(hmac::HMAC_SHA256, key),
        &lv(&[b"screx-quic-pair-v1", role, binding]),
    )
    .as_ref()
    .to_vec()
}
pub fn verify(key: &[u8], role: &[u8], binding: &[u8], tag: &[u8]) -> Result<()> {
    hmac::verify(
        &hmac::Key::new(hmac::HMAC_SHA256, key),
        &lv(&[b"screx-quic-pair-v1", role, binding]),
        tag,
    )
    .map_err(|_| anyhow::anyhow!("pairing confirmation failed"))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn hex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }
    #[test]
    fn draft21_x25519_sha512_vectors() {
        let sid = hex("7e4b4791d6a8ef019b936c79fb7f2c57");
        let ci = lv(&[b"A_initiator", b"B_responder"]);
        assert_eq!(
            generator(b"Password", &ci, &sid).as_slice(),
            hex("d04bf6d41f6a289632a2e929fa29bebd51092512a7829fdde7d314b62f05a73f")
        );
        let a = Exchange::from_scalar(
            b"Password",
            &ci,
            &sid,
            hex("21b4f4bd9e64ed355c3eb676a28ebedaf6d8f17bdc365995b319097153044080")
                .try_into()
                .unwrap(),
        );
        let b = Exchange::from_scalar(
            b"Password",
            &ci,
            &sid,
            hex("848b0779ff415f0af4ea14df9dd1d3c29ac41d836c7808896c4eba19c51ac40a")
                .try_into()
                .unwrap(),
        );
        let ya = a.share;
        let yb = b.share;
        assert_eq!(
            ya.as_slice(),
            hex("1d13c89278cdadd826f6d8d7f887701430f8380ddc17611cdd6dc989ce0c9f32")
        );
        assert_eq!(
            yb.as_slice(),
            hex("248cccf6d5cdc3646f0ad593f9e6cef4e69d4945f8372e623512ecea32185623")
        );
        let ka = a.finish(yb, &sid, &ya, b"ADa", &yb, b"ADb").unwrap();
        let kb = b.finish(ya, &sid, &ya, b"ADa", &yb, b"ADb").unwrap();
        assert_eq!(ka, kb);
        assert_eq!(ka, hex("6e19b875f7a561d6b3ca3dbb9ef42ac55de3e717881018204b8922b4d5e53bb2aa82c300bea7b65d2b671da71922ddf6472301b79bc270adfa8bf413285f2263"));
    }
    #[test]
    fn wrong_pin_fails_confirmation_and_low_order_aborts() {
        let sid = random::<32>();
        let a = Exchange::new(b"12345678", b"context", &sid);
        let b = Exchange::new(b"12345679", b"context", &sid);
        let ya = a.share;
        let yb = b.share;
        let ka = a.finish(yb, &sid, &ya, b"a", &yb, b"b").unwrap();
        let kb = b.finish(ya, &sid, &ya, b"a", &yb, b"b").unwrap();
        assert!(verify(
            &ka,
            b"client",
            b"binding",
            &confirm(&kb, b"client", b"binding")
        )
        .is_err());
        assert!(Exchange::new(b"12345678", b"", &sid)
            .finish([0; 32], &sid, b"", b"", b"", b"")
            .is_err());
    }
    #[test]
    fn pin_has_eight_decimal_digits() {
        for _ in 0..100 {
            let code = pin();
            assert!(code.len() == 8 && code.bytes().all(|b| b.is_ascii_digit()));
        }
    }

    #[test]
    fn confirmations_are_role_and_channel_bound() {
        let key = random::<64>();
        let tag = confirm(&key, b"client", b"TLS connection A");
        assert!(verify(&key, b"client", b"TLS connection A", &tag).is_ok());
        assert!(verify(&key, b"server", b"TLS connection A", &tag).is_err());
        assert!(verify(&key, b"client", b"TLS connection B", &tag).is_err());
    }
}
