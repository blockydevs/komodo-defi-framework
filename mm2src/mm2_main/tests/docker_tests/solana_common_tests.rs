use coins::solana::{ed25519_dalek,
                    ed25519_dalek_bip32::{DerivationPath, ExtendedSecretKey},
                    solana_client::rpc_client::RpcClient,
                    solana_sdk,
                    solana_sdk::{commitment_config::{CommitmentConfig, CommitmentLevel},
                                 pubkey::Pubkey,
                                 signature::{Keypair as SolKeypair, Signer}},
                    spl::{SplToken, SplTokenFields},
                    SolanaCoin};
use common::executor::abortable_queue::AbortableQueue;
use crypto::privkey::{bip39_seed_from_passphrase, key_pair_from_seed};
use mm2_core::mm_ctx::{MmArc, MmCtxBuilder};
use std::{collections::HashMap,
          str::FromStr,
          sync::{Arc, Mutex}};

pub const SOLANA_CLIENT_URL: &str = "http://localhost:8899";
pub const SOL: &str = "SOL";
pub const SOL_PASSPHRASE: &str = "federal stay trigger hour exist success game vapor become comfort action phone bright ill target wild nasty crumble dune close rare fabric hen iron";
pub const SOL_ADDITIONAL_PASSPHRASE: &str =
    "spice describe gravity federal blast come thank unfair canal monkey style afraid";
pub const PROGRAM_ID: &str = "GCJUXKH4VeKzEtr9YgwaNWC3dJonFgsM3yMiBa64CZ8m";
pub const NON_EXISTENT_PASSPHRASE: &str = "non existent passphrase";
pub const ACCOUNT_PUBKEY: &str = "FJktmyjV9aBHEShT4hfnLpr9ELywdwVtEL1w1rSWgbVf";

pub enum SolanaNet {
    Local,
}

pub fn solana_net_to_url(net_type: SolanaNet) -> String {
    match net_type {
        SolanaNet::Local => SOLANA_CLIENT_URL.to_string(),
    }
}

#[allow(dead_code)]
pub fn generate_key_pair_from_seed(seed: &str) -> SolKeypair {
    let derivation_path = DerivationPath::from_str("m/44'/501'/0'").unwrap();
    let seed = bip39_seed_from_passphrase(seed).unwrap();

    let ext = ExtendedSecretKey::from_seed(&seed.0)
        .unwrap()
        .derive(&derivation_path)
        .unwrap();
    let pub_key = ext.public_key();
    let pair = ed25519_dalek::Keypair {
        secret: ext.secret_key,
        public: pub_key,
    };

    solana_sdk::signature::keypair_from_seed(pair.to_bytes().as_ref()).unwrap()
}

pub fn generate_key_pair_from_iguana_seed(seed: String) -> SolKeypair {
    let key_pair = key_pair_from_seed(seed.as_str()).unwrap();
    let secret_key = ed25519_dalek::SecretKey::from_bytes(key_pair.private().secret.as_slice()).unwrap();
    let public_key = ed25519_dalek::PublicKey::from(&secret_key);
    let other_key_pair = ed25519_dalek::Keypair {
        secret: secret_key,
        public: public_key,
    };
    solana_sdk::signature::keypair_from_seed(other_key_pair.to_bytes().as_ref()).unwrap()
}

#[allow(dead_code)]
pub fn spl_coin_for_test(
    solana_coin: SolanaCoin,
    ticker: String,
    decimals: u8,
    token_contract_address: Pubkey,
) -> SplToken {
    SplToken {
        conf: Arc::new(SplTokenFields {
            decimals,
            ticker,
            token_contract_address,
            abortable_system: AbortableQueue::default(),
        }),
        platform_coin: solana_coin,
    }
}

pub fn solana_coin_for_test(seed: String, net_type: SolanaNet) -> (MmArc, SolanaCoin) {
    let url = solana_net_to_url(net_type);
    let client = RpcClient::new_with_commitment(url, CommitmentConfig {
        commitment: CommitmentLevel::Finalized,
    });
    let conf = json!({
        "coins":[
           {"coin":SOL,"name":"solana","protocol":{"type":SOL},"rpcport":80,"mm2":1}
        ]
    });
    let ctx = MmCtxBuilder::new().with_conf(conf).into_mm_arc();
    let (ticker, decimals) = (SOL.to_string(), 8);
    let key_pair = generate_key_pair_from_iguana_seed(seed);
    let my_address = key_pair.pubkey().to_string();
    let spl_tokens_infos = Arc::new(Mutex::new(HashMap::new()));
    let spawner = AbortableQueue::default();
    let solana_coin = SolanaCoin::new(
        ticker,
        key_pair,
        client,
        decimals,
        my_address,
        spl_tokens_infos,
        spawner,
    );
    (ctx, solana_coin)
}
