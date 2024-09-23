use crate::hd_wallet::{HDAccount, HDAddress, HDWallet};
use bip32::{ExtendedPublicKey, PrivateKeyBytes, PublicKey as bip32PublicKey, PublicKeyBytes, Result as bip32Result};
use web3::types::H256;
use ed25519_dalek::{PublicKey as Ed25519PublicKey};
use ed25519_dalek::SECRET_KEY_LENGTH;
use ed25519_dalek_bip32::{ChildIndex, ExtendedSecretKey, SecretKey};

pub struct Address(pub H256);

pub struct SolanaPublicKey(pub Ed25519PublicKey);

pub type SolanaHDAddress = HDAddress<Address, SolanaPublicKey>;
pub type SolanaHDAccount = HDAccount<SolanaHDAddress, Ed25519ExtendedPublicKey>;
pub type SolanaHDWallet = HDWallet<SolanaHDAccount>;
pub type Ed25519ExtendedPublicKey = ExtendedPublicKey<SolanaPublicKey>;

impl bip32PublicKey for SolanaPublicKey {
    fn from_bytes(_bytes: PublicKeyBytes) -> bip32Result<Self> {
        todo!()
    }

    fn to_bytes(&self) -> PublicKeyBytes {
        todo!()
    }

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
   xprv.derive_child(ChildIndex::Hardened(index)).unwrap()
}

#[test]
fn key_derivation() {
    let key_0 = derive_ed25519_key_impl(SEED_BYTES, 0);
    assert_eq!(key_0.public_key().to_bytes(), [180, 84, 70, 20, 219, 166, 136, 38, 57, 179, 37, 251, 94, 32, 84, 64, 188, 87, 96, 252, 5, 86, 155, 111, 71, 97, 94, 6, 138, 166, 233, 199]);

    let key_5 = derive_ed25519_key_impl(SEED_BYTES, 5);
    assert_eq!(key_5.public_key().to_bytes(), [218, 76, 229, 83, 128, 102, 49, 238, 89, 71, 118, 37, 190, 119, 112, 19, 203, 182, 255, 143, 241, 152, 222, 45, 4, 229, 94, 2, 141, 46, 21, 13]);

    let key_8 = derive_ed25519_key_impl(SEED_BYTES, 8);
    assert_eq!(key_8.public_key().to_bytes(), [62, 24, 165, 95, 240, 247, 73, 76, 135, 225, 82, 53, 10, 111, 218, 250, 93, 28, 203, 83, 219, 46, 228, 173, 230, 238, 84, 119, 249, 153, 29, 99]);
}