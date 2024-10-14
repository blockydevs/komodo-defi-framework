pub const HARDENED_PATH: u32 = 2147483648;

pub use bip32::{ChildNumber, DerivationPath, Error as Bip32Error, ExtendedPublicKey};

use ed25519_dalek::PublicKey;
use ed25519_dalek_bip32::ExtendedSecretKey;

pub type Secp256k1ExtendedPublicKey = ExtendedPublicKey<secp256k1::PublicKey>;
pub type XPub = String;

pub struct Ed25519ExtendedPublicKey {
    pub pubkey: PublicKey,
    pub xpriv: Option<ExtendedSecretKey>,
}

#[derive(Clone, Copy)]
pub enum EcdsaCurve {
    Secp256k1,
}
