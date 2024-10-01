use std::str::FromStr;
use async_trait::async_trait;
use crate::hd_wallet::{ExtractExtendedPubkey, HDAccount, HDAddress, HDExtractPubkeyError, HDWallet, HDWalletCoinOps, HDXPubExtractor, TrezorCoinError};
use bip32::{ExtendedPublicKey, PrivateKeyBytes, PublicKey as bip32PublicKey, PublicKeyBytes, Result as bip32Result};
use web3::types::H256;
use ed25519_dalek::{PublicKey as Ed25519PublicKey, Keypair};
use ed25519_dalek::SECRET_KEY_LENGTH;
use ed25519_dalek_bip32::{ChildIndex, ExtendedSecretKey, SecretKey, DerivationPath};

use ethkey::public_to_address;
use crypto::{Ed25519ExtendedPublicKey};
use mm2_err_handle::mm_error::MmResult;
use crate::eth::eth_hd_wallet::{EthHDAddress};
use crate::solana::pubkey_from_extended;
use crate::{CoinWithPrivKeyPolicy, Ed25519PrivKeyPolicy, PrivKeyPolicy, SolanaCoin};
use crate::eth::EthCoin;
use crate::hd_wallet::ExtendedPublicKeyOps;

pub struct Address(pub H256);

pub type SolanaPublicKey = Ed25519PublicKey;

pub type SolanaHDAddress = HDAddress<Address, SolanaPublicKey>;
pub type SolanaHDAccount = HDAccount<SolanaHDAddress, Ed25519ExtendedPublicKey>;
pub type SolanaHDWallet = HDWallet<SolanaHDAccount>;

// impl SolanaPublicKey {
//     fn derive_child(&self, _other: PrivateKeyBytes) -> bip32Result<Self> {
//         todo!()
//         // derive_ed25519_key_impl()
//     }
// }

impl ExtractExtendedPubkey for SolanaCoin {
    type ExtendedPublicKey = Ed25519ExtendedPublicKey;

    async fn extract_extended_pubkey<XPubExtractor>(
        &self,
        xpub_extractor: Option<XPubExtractor>,
        derivation_path: bip32::DerivationPath,
    ) -> MmResult<Self::ExtendedPublicKey, HDExtractPubkeyError>
    where
        XPubExtractor: HDXPubExtractor + Send,
    {
        todo!()
        // extract_extended_pubkey_impl(self, xpub_extractor, derivation_path).await
    }
}

#[async_trait]
impl HDWalletCoinOps for SolanaCoin {
    type HDWallet = SolanaHDWallet;

    fn address_formatter(&self) -> fn(&ethereum_types::Address) -> String {
        todo!()
    }

    fn address_from_extended_pubkey(
        &self,
        extended_pubkey: &Ed25519ExtendedPublicKey,
        derivation_path: bip32::DerivationPath,
    ) -> SolanaHDAddress {
        // let pubkey = pubkey_from_extended(extended_pubkey);
        // let address = public_to_address(&pubkey);
        // SolanaHDAddress {
        //     address,
        //     pubkey,
        //     derivation_path,
        // }
        todo!()
    }

    fn trezor_coin(&self) -> MmResult<String, TrezorCoinError> {
        // self.trezor_coin.clone().or_mm_err(|| {
        //     let ticker = self.ticker();
        //     let error = format!("'{ticker}' coin has 'trezor_coin' field as `None` in the coins config");
        //     TrezorCoinError::Internal(error)
        // })
        todo!()
    }
}

impl CoinWithPrivKeyPolicy for SolanaCoin {
    type KeyPair = Keypair;
    type PrivKeyPolicy = PrivKeyPolicy<Self::KeyPair>;

    fn priv_key_policy(&self) -> &Self::PrivKeyPolicy {
        todo!()
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