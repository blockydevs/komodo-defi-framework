use std::str::FromStr;
use crate::hd_wallet::{HDAccount, HDAddress, HDWallet};
use bip32::{ExtendedPublicKey, PrivateKeyBytes, PublicKey as bip32PublicKey, PublicKeyBytes, Result as bip32Result};
use web3::types::H256;
use ed25519_dalek::{PublicKey as Ed25519PublicKey};
use ed25519_dalek::SECRET_KEY_LENGTH;
use ed25519_dalek_bip32::{ChildIndex, ExtendedSecretKey, SecretKey, DerivationPath};

pub struct Address(pub H256);

pub struct SolanaPublicKey(pub Ed25519PublicKey);

pub type SolanaHDAddress = HDAddress<Address, SolanaPublicKey>;
pub type SolanaHDAccount = HDAccount<SolanaHDAddress, Ed25519ExtendedPublicKey>;
pub type SolanaHDWallet = HDWallet<SolanaHDAccount>;
pub type Ed25519ExtendedPublicKey = ExtendedPublicKey<SolanaPublicKey>;

impl SolanaPublicKey {
    fn derive_child(&self, _other: PrivateKeyBytes) -> bip32Result<Self> {
        todo!()
        // derive_ed25519_key_impl()
    }
}

const SEED_BYTES: [u8; SECRET_KEY_LENGTH] = [
    157, 097, 177, 157, 239, 253, 090, 096,
    186, 132, 074, 244, 146, 236, 044, 196,
    068, 073, 197, 105, 123, 050, 105, 025,
    112, 059, 172, 003, 028, 174, 127, 096, ];

fn derive_ed25519_key_impl(seed: [u8; SECRET_KEY_LENGTH], index: u32) -> ExtendedSecretKey {
    let xprv = ExtendedSecretKey::from_seed(&seed).unwrap();
    let derivation_path: DerivationPath = format!("m/44'/501'/{index}'/0'").parse().unwrap();
    xprv.derive(&derivation_path).unwrap()
}

#[test]
fn key_derivation() {
    let key_0 = derive_ed25519_key_impl(SEED_BYTES, 0);
    assert_eq!(key_0.public_key().to_bytes(), [185, 72, 208, 205, 67, 189, 80, 35, 221, 53, 202, 150, 255, 22, 196, 170, 5, 206, 243, 76, 158, 120, 156, 107, 141, 88, 246, 31, 168, 83, 93, 129]);

    let key_5 = derive_ed25519_key_impl(SEED_BYTES, 5);
    assert_eq!(key_5.public_key().to_bytes(), [126, 99, 126, 54, 130, 205, 203, 86, 182, 109, 13, 40, 54, 190, 38, 100, 105, 43, 48, 247, 162, 35, 7, 32, 48, 146, 131, 155, 182, 245, 87, 163]);

    let key_8 = derive_ed25519_key_impl(SEED_BYTES, 8);
    assert_eq!(key_8.public_key().to_bytes(), [185, 199, 244, 67, 196, 210, 20, 107, 189, 37, 133, 150, 248, 200, 167, 183, 88, 240, 178, 220, 41, 203, 19, 240, 198, 143, 175, 56, 216, 17, 64, 144]);
}