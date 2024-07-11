use crate::sia::blake2b_internal::hash_blake2b_single;
use crate::sia::{PublicKey, Signature};
use rpc::v1::types::H256;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::convert::From;
use std::fmt;
use std::str::FromStr;

fn deserialize_hex_to_array<'de, D>(deserializer: D) -> Result<[u8; 64], D::Error>
where
    D: Deserializer<'de>,
{
    let s: &str = Deserialize::deserialize(deserializer)?;
    let bytes = hex::decode(s).map_err(serde::de::Error::custom)?;

    if bytes.len() != 64 {
        return Err(serde::de::Error::custom("invalid length for [u8; 64]"));
    }

    let mut array = [0u8; 64];
    array.copy_from_slice(&bytes);
    Ok(array)
}

// https://github.com/SiaFoundation/core/blob/092850cc52d3d981b19c66cd327b5d945b3c18d3/types/encoding.go#L16
// TODO go implementation limits this to 1024 bytes, should we?
#[derive(Default)]
pub struct Encoder {
    pub buffer: Vec<u8>,
}

pub trait Encodable {
    fn encode(&self, encoder: &mut Encoder);
}

// This wrapper allows us to use Signature internally but still serde as "sig:" prefixed string
#[derive(Debug)]
pub struct PrefixedSignature(pub Signature);

impl<'de> Deserialize<'de> for PrefixedSignature {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct PrefixedSignatureVisitor;

        impl<'de> serde::de::Visitor<'de> for PrefixedSignatureVisitor {
            type Value = PrefixedSignature;

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("a string prefixed with 'sig:' and followed by a 128-character hex string")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                if let Some(hex_str) = value.strip_prefix("sig:") {
                    Signature::from_str(hex_str)
                        .map(PrefixedSignature)
                        .map_err(|_| E::invalid_value(serde::de::Unexpected::Str(value), &self))
                } else {
                    Err(E::invalid_value(serde::de::Unexpected::Str(value), &self))
                }
            }
        }

        deserializer.deserialize_str(PrefixedSignatureVisitor)
    }
}

impl Serialize for PrefixedSignature {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&format!("{}", self))
    }
}

impl fmt::Display for PrefixedSignature {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { write!(f, "h:{}", self.0) }
}

impl From<PrefixedSignature> for Signature {
    fn from(sia_hash: PrefixedSignature) -> Self { sia_hash.0 }
}

impl From<Signature> for PrefixedSignature {
    fn from(signature: Signature) -> Self { PrefixedSignature(signature) }
}

// This wrapper allows us to use PublicKey internally but still serde as "ed25519:" prefixed string
#[derive(Debug)]
pub struct PrefixedPublicKey(pub PublicKey);

impl<'de> Deserialize<'de> for PrefixedPublicKey {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct PrefixedPublicKeyVisitor;

        impl<'de> serde::de::Visitor<'de> for PrefixedPublicKeyVisitor {
            type Value = PrefixedPublicKey;

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("a string prefixed with 'ed25519:' and followed by a 64-character hex string")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                if let Some(hex_str) = value.strip_prefix("ed25519:") {
                    let bytes = hex::decode(hex_str).map_err(|_| E::invalid_value(serde::de::Unexpected::Str(value), &self))?;
                    PublicKey::from_bytes(&bytes)
                        .map(PrefixedPublicKey)
                        .map_err(|_| E::invalid_value(serde::de::Unexpected::Str(value), &self))
                } else {
                    Err(E::invalid_value(serde::de::Unexpected::Str(value), &self))
                }
            }
        }

        deserializer.deserialize_str(PrefixedPublicKeyVisitor)
    }
}

impl Serialize for PrefixedPublicKey {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&format!("ed25519:{}", hex::encode(self.0.as_bytes())))
    }
}

impl From<PrefixedPublicKey> for PublicKey {
    fn from(sia_public_key: PrefixedPublicKey) -> Self {
        sia_public_key.0
    }
}

impl From<PublicKey> for PrefixedPublicKey {
    fn from(public_key: PublicKey) -> Self {
        PrefixedPublicKey(public_key)
    }
}

// This wrapper allows us to use H256 internally but still serde as "h:" prefixed string
#[derive(Debug)]
pub struct PrefixedH256(pub H256);

// FIXME this code pattern is reoccuring in many places and should be generalized with helpers or macros
impl<'de> Deserialize<'de> for PrefixedH256 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct PrefixedH256Visitor;

        impl<'de> serde::de::Visitor<'de> for PrefixedH256Visitor {
            type Value = PrefixedH256;

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("a string prefixed with 'h:' and followed by a 64-character hex string")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                if let Some(hex_str) = value.strip_prefix("h:") {
                    H256::from_str(hex_str)
                        .map(PrefixedH256)
                        .map_err(|_| E::invalid_value(serde::de::Unexpected::Str(value), &self))
                } else {
                    Err(E::invalid_value(serde::de::Unexpected::Str(value), &self))
                }
            }
        }

        deserializer.deserialize_str(PrefixedH256Visitor)
    }
}

impl Serialize for PrefixedH256 {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&format!("{}", self))
    }
}

impl fmt::Display for PrefixedH256 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { write!(f, "h:{}", self.0) }
}

impl From<PrefixedH256> for H256 {
    fn from(sia_hash: PrefixedH256) -> Self { sia_hash.0 }
}

impl From<H256> for PrefixedH256 {
    fn from(h256: H256) -> Self { PrefixedH256(h256) }
}

impl Encodable for H256 {
    fn encode(&self, encoder: &mut Encoder) { encoder.write_slice(&self.0); }
}

impl Encoder {
    pub fn reset(&mut self) { self.buffer.clear(); }

    /// writes a length-prefixed []byte to the underlying stream.
    pub fn write_len_prefixed_bytes(&mut self, data: &[u8]) {
        self.buffer.extend_from_slice(&data.len().to_le_bytes());
        self.buffer.extend_from_slice(data);
    }

    pub fn write_slice(&mut self, data: &[u8]) { self.buffer.extend_from_slice(data); }

    pub fn write_u8(&mut self, u: u8) { self.buffer.extend_from_slice(&[u]) }

    pub fn write_u64(&mut self, u: u64) { self.buffer.extend_from_slice(&u.to_le_bytes()); }

    pub fn write_string(&mut self, p: &str) { self.write_len_prefixed_bytes(p.to_string().as_bytes()); }

    pub fn write_distinguisher(&mut self, p: &str) { self.buffer.extend_from_slice(format!("sia/{}|", p).as_bytes()); }

    pub fn write_bool(&mut self, b: bool) { self.buffer.push(b as u8) }

    pub fn hash(&self) -> H256 { hash_blake2b_single(&self.buffer) }

    // Utility method to create, encode, and hash
    pub fn encode_and_hash<T: Encodable>(item: &T) -> H256 {
        let mut encoder = Encoder::default();
        item.encode(&mut encoder);
        encoder.hash()
    }
}

#[test]
fn test_encoder_default_hash() {
    assert_eq!(
        Encoder::default().hash(),
        H256::from("0e5751c026e543b2e8ab2eb06099daa1d1e5df47778f7787faab45cdf12fe3a8")
    )
}

#[test]
fn test_encoder_write_bytes() {
    let mut encoder = Encoder::default();
    encoder.write_len_prefixed_bytes(&[1, 2, 3, 4]);
    assert_eq!(
        encoder.hash(),
        H256::from("d4a72b52e2e1f40e20ee40ea6d5080a1b1f76164786defbb7691a4427f3388f5")
    );
}

#[test]
fn test_encoder_write_u8() {
    let mut encoder = Encoder::default();
    encoder.write_u8(1);
    assert_eq!(
        encoder.hash(),
        H256::from("ee155ace9c40292074cb6aff8c9ccdd273c81648ff1149ef36bcea6ebb8a3e25")
    );
}

#[test]
fn test_encoder_write_u64() {
    let mut encoder = Encoder::default();
    encoder.write_u64(1);
    assert_eq!(
        encoder.hash(),
        H256::from("1dbd7d0b561a41d23c2a469ad42fbd70d5438bae826f6fd607413190c37c363b")
    );
}

#[test]
fn test_encoder_write_distiguisher() {
    let mut encoder = Encoder::default();
    encoder.write_distinguisher("test");
    assert_eq!(
        encoder.hash(),
        H256::from("25fb524721bf98a9a1233a53c40e7e198971b003bf23c24f59d547a1bb837f9c")
    );
}

#[test]
fn test_encoder_write_bool() {
    let mut encoder = Encoder::default();
    encoder.write_bool(true);
    assert_eq!(
        encoder.hash(),
        H256::from("ee155ace9c40292074cb6aff8c9ccdd273c81648ff1149ef36bcea6ebb8a3e25")
    );
}

#[test]
fn test_encoder_reset() {
    let mut encoder = Encoder::default();
    encoder.write_bool(true);
    assert_eq!(
        encoder.hash(),
        H256::from("ee155ace9c40292074cb6aff8c9ccdd273c81648ff1149ef36bcea6ebb8a3e25")
    );

    encoder.reset();
    encoder.write_bool(false);
    assert_eq!(
        encoder.hash(),
        H256::from("03170a2e7597b7b7e3d84c05391d139a62b157e78786d8c082f29dcf4c111314")
    );
}

#[test]
fn test_encoder_complex() {
    let mut encoder = Encoder::default();
    encoder.write_distinguisher("test");
    encoder.write_bool(true);
    encoder.write_u8(1);
    encoder.write_len_prefixed_bytes(&[1, 2, 3, 4]);
    assert_eq!(
        encoder.hash(),
        H256::from("b66d7a9bef9fb303fe0e41f6b5c5af410303e428c4ff9231f6eb381248693221")
    );
}
