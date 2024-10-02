pub const HARDENED_PATH: u32 = 2147483648;

pub use bip32::{ChildNumber, DerivationPath, Error as Bip32Error, ExtendedPublicKey};
use ed25519_dalek::PublicKey;
use ed25519_dalek_bip32::{ChildIndex, ExtendedSecretKey};
use std::str::FromStr;

pub type Secp256k1ExtendedPublicKey = ExtendedPublicKey<secp256k1::PublicKey>;
pub type XPub = String;

pub struct Ed25519ExtendedPublicKey {
    pub pubkey: PublicKey,
    xpriv: Option<ExtendedSecretKey>,
}
impl Ed25519ExtendedPublicKey {
    pub fn derive_child(&self, child_number: ChildNumber) -> Result<Self, Bip32Error> {
        let xpriv = self.xpriv.as_ref().ok_or(Bip32Error::Depth)?;
        // Todo: this can be removed
        if !child_number.is_hardened() {
            return Err(Bip32Error::Depth);
        }
        let xpriv = xpriv
            .derive_child(ChildIndex::Hardened(child_number.index()))
            .map_err(|_| Bip32Error::Depth)?;
        let pubkey = xpriv.public_key();
        Ok(Ed25519ExtendedPublicKey {
            pubkey,
            xpriv: Some(xpriv),
        })
    }
}

impl FromStr for Ed25519ExtendedPublicKey {
    type Err = Bip32Error;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let bytes = hex::decode(s).map_err(|_| Bip32Error::Decode)?;
        let pubkey = PublicKey::from_bytes(&bytes).map_err(|_| Bip32Error::Decode)?;
        Ok(Ed25519ExtendedPublicKey { pubkey, xpriv: None })
    }
}

#[derive(Clone, Copy)]
pub enum EcdsaCurve {
    Secp256k1,
}
