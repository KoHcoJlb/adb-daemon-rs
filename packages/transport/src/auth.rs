use crate::error::ErrorKind;
use crate::Result;
use base64::prelude::BASE64_STANDARD;
use base64::Engine;
use bytes::BufMut;
use num_bigint_dig::ModInverse;
use num_traits::{Pow, ToPrimitive};
use rsa::traits::PublicKeyParts;
use rsa::{BigUint, RsaPublicKey};
use tracing::info;

const ANDROID_KEY_SIZE: usize = 2048 / 8;

pub(crate) fn encode_public_key(key: &RsaPublicKey) -> Result<String> {
    if key.size() != ANDROID_KEY_SIZE {
        Err((ErrorKind::InvalidKey, "wrong key size"))?;
    }

    let mut buf = Vec::<u8>::with_capacity(3 * 4 + ANDROID_KEY_SIZE * 2);
    buf.put_u32_le((ANDROID_KEY_SIZE / 4) as u32);

    let r32 = BigUint::from(2u32).pow(32u32);
    let ninv = &r32
        - (key.n() % &r32)
            .mod_inverse(&r32)
            .ok_or((ErrorKind::InvalidKey, "mod_inverse"))?
            .to_biguint()
            .ok_or((ErrorKind::InvalidKey, "to_biguint"))?;
    buf.put_u32_le(ninv.to_u32().ok_or((ErrorKind::InvalidKey, "ninv.to_u32"))?);

    buf.extend(key.n().to_bytes_le());

    let rr = BigUint::from(2u32).pow(ANDROID_KEY_SIZE * 8).modpow(&2u32.into(), key.n());
    buf.extend(rr.to_bytes_le());

    buf.put_u32_le(key.e().to_u32().ok_or((ErrorKind::InvalidKey, "e.to_u32"))?);

    info!(len = buf.len());

    Ok(BASE64_STANDARD.encode(buf))
}
