use crate::utxo::blockbook::structures::{BalanceHistoryParams, BlockBookAddress, BlockBookBalanceHistory,
                                         BlockBookBlock, BlockBookTickers, BlockBookTickersList, BlockBookTransaction,
                                         BlockBookTransactionOutput, BlockBookTransactionSpecific, BlockBookUtxo,
                                         GetAddressParams, GetBlockByHashHeight, XpubTransactions};
use crate::utxo::rpc_clients::{BlockHashOrHeight, EstimateFeeMethod, EstimateFeeMode, JsonRpcPendingRequestsShared,
                               SpentOutputInfo, UnspentInfo, UnspentMap, UtxoJsonRpcClientInfo, UtxoRpcClientOps,
                               UtxoRpcError, UtxoRpcFut};
use crate::utxo::utxo_block_header_storage::BlockHeaderStorage;
use crate::utxo::{GetBlockHeaderError, NonZeroU64, UtxoTx};
use crate::{RpcTransportEventHandler, RpcTransportEventHandlerShared};
use async_trait::async_trait;
use bitcoin::Amount;
use bitcoin::Denomination::Satoshi;
use chain::TransactionInput;
use common::executor::abortable_queue::AbortableQueue;
use common::jsonrpc_client::{JsonRpcClient, JsonRpcErrorType, JsonRpcRemoteAddr, JsonRpcRequest, JsonRpcRequestEnum,
                             JsonRpcResponseEnum, JsonRpcResponseFut, RpcRes};
use common::{block_on, APPLICATION_JSON};
use futures::FutureExt;
use futures::TryFutureExt;
use futures01::sync::mpsc;
use futures01::Future;
use http::header::{ACCEPT, AUTHORIZATION, USER_AGENT};
use http::{Request, StatusCode};
use keys::Address;
use mm2_core::mm_ctx::MmArc;
use mm2_err_handle::mm_error::MmError;
use mm2_err_handle::prelude::{MapToMmFutureExt, MapToMmResult, MmResult};
use mm2_net::transport::slurp_req_body;
use mm2_number::BigDecimal;
use rpc::v1::types::{deserialize_null_default, Bytes, CoinbaseTransactionInput, RawTransaction,
                     SignedTransactionOutput, Transaction, TransactionInputEnum, TransactionOutputScript, H256};
use serde::{Deserialize, Deserializer};
use serde_json::{self as json, Value as Json};
use serialization::CoinVariant;
use std::str::FromStr;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::Arc;
use tonic::IntoRequest;

const BTC_BLOCKBOOK_ENPOINT: &str = "https://btc1.trezor.io";

pub type BlockBookResult<T> = MmResult<T, BlockBookClientError>;

#[derive(Debug, Clone)]
pub struct BlockBookClientImpl {
    pub ticker: String,
    pub url: String,
}

#[derive(Debug, Clone)]
pub struct BlockBookClient(pub Arc<BlockBookClientImpl>);

impl BlockBookClient {
    pub fn new(url: &str, ticker: &str) -> Self {
        Self(Arc::new(BlockBookClientImpl {
            url: url.to_string(),
            ticker: ticker.to_string(),
        }))
    }

    pub async fn query(&self, path: String) -> BlockBookResult<Json> {
        use http::header::HeaderValue;

        let uri = format!("{}{path}", self.0.url);
        let request = http::Request::builder()
            .method("GET")
            .uri(uri.clone())
            .header(ACCEPT, HeaderValue::from_static(APPLICATION_JSON))
            .header(
                USER_AGENT,
                "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:90.0) Gecko/20100101 Firefox/90.0",
            )
            .body(hyper::Body::from(""))
            .map_err(|err| BlockBookClientError::Transport(err.to_string()))?;

        let (status, _header, body) = slurp_req_body(request)
            .await
            .map_err(|err| MmError::new(BlockBookClientError::Transport(err.to_string())))?;

        if !status.is_success() {
            return Err(MmError::new(BlockBookClientError::Transport(format!(
                "Response !200 from {}: {}, {}",
                uri, status, body
            ))));
        }

        Ok(body)
    }

    /// Status page returns current status of Blockbook and connected backend.
    pub async fn status(&self, _height: u64) -> BlockBookResult<H256> { todo!() }

    /// Get current block of a given height.
    pub async fn get_block_hash(&self, height: u64) -> BlockBookResult<H256> {
        let path = format!("/api/v2/block-index/{height}");
        let res = self.query(path).await?;
        Ok(serde_json::from_value(res["blockHash"].clone())
            .map_err(|err| BlockBookClientError::ResponseError(err.to_string()))?)
    }

    /// Get transaction returns "normalized" data about transaction, which has the same general structure for all
    /// supported coins. It does not return coin specific fields.
    pub async fn get_transaction(&self, txid: &str) -> BlockBookResult<BlockBookTransaction> {
        let path = format!("/api/v2/tx/{txid}");
        let res = self.query(path).await?;
        Ok(serde_json::from_value(res).map_err(|err| BlockBookClientError::ResponseError(err.to_string()))?)
    }

    /// Get transaction data in the exact format as returned by backend, including all coin specific fields:
    pub async fn get_transaction_specific(&self, _txid: &str) -> BlockBookResult<BlockBookTransactionSpecific> {
        todo!()
    }

    /// Get balances and transactions of an address. The returned transactions are sorted by block height, newest
    /// blocks first. see `[strutures::GetAddressParams]` for extra query arguments.
    pub async fn get_address(
        &self,
        _address: &str,
        _query_params: Option<GetAddressParams>,
    ) -> BlockBookResult<BlockBookAddress> {
        todo!()
    }

    /// Get balances and transactions of an xpub or output descriptor, applicable only for Bitcoin-type coins. see
    /// `[strutures::GetAddressParams]` for extra query arguments.
    pub async fn get_xpub(
        &self,
        _xpub: &str,
        _query_params: Option<GetAddressParams>,
    ) -> BlockBookResult<XpubTransactions> {
        todo!()
    }

    // Get array of unspent transaction outputs of address or xpub, applicable only for Bitcoin-type coins. By default,
    // the list contains both confirmed and unconfirmed transactions. The query parameter confirmed=true disables return of unconfirmed transactions. The returned utxos are sorted by block height, newest blocks first. For xpubs or output descriptors, the response also contains address and derivation path of the utxo.
    pub async fn get_utxo(&self, _address: &str, _confirmed: bool) -> BlockBookResult<Vec<BlockBookUtxo>> { todo!() }

    /// Get information about block with transactions, either by height or hash
    pub async fn get_block(&self, _block_by: &GetBlockByHashHeight) -> BlockBookResult<BlockBookBlock> { todo!() }

    /// Sends new transaction to backend.
    pub async fn send_transaction(&self, _hex: &RawTransaction) -> BlockBookResult<H256> { todo!() }

    /// Get a list of available currency rate tickers (secondary currencies) for the specified date, along with an
    /// actual data timestamp.
    pub async fn get_tickers_list(&self, _timestamp: Option<u32>) -> BlockBookResult<BlockBookTickersList> { todo!() }

    /// Get currency rate for the specified currency and date. If the currency is not available for that specific
    /// timestamp, the next closest rate will be returned. All responses contain an actual rate timestamp.
    pub async fn get_tickers(
        &self,
        _currency: Option<&str>,
        _timestamp: Option<u32>,
    ) -> BlockBookResult<BlockBookTickers> {
        todo!()
    }

    /// Returns a balance history for the specified XPUB or address.
    pub async fn balance_history(
        &self,
        _address: &str,
        _query_params: BalanceHistoryParams,
    ) -> BlockBookResult<BlockBookBalanceHistory> {
        todo!()
    }
}

#[async_trait]
impl UtxoRpcClientOps for BlockBookClient {
    fn list_unspent(&self, _address: &Address, _decimals: u8) -> UtxoRpcFut<Vec<UnspentInfo>> { todo!() }

    fn list_unspent_group(&self, _addresses: Vec<Address>, _decimals: u8) -> UtxoRpcFut<UnspentMap> { todo!() }

    fn send_transaction(&self, _tx: &UtxoTx) -> UtxoRpcFut<H256> { todo!() }

    fn send_raw_transaction(&self, _tx: Bytes) -> UtxoRpcFut<H256> { todo!() }

    fn get_transaction_bytes(&self, _txid: &H256) -> UtxoRpcFut<Bytes> { todo!() }

    fn get_verbose_transaction(&self, _txid: &H256) -> UtxoRpcFut<Transaction> { todo!() }

    fn get_verbose_transactions(&self, _tx_ids: &[H256]) -> UtxoRpcFut<Vec<Transaction>> { todo!() }

    fn get_block_count(&self) -> UtxoRpcFut<u64> { todo!() }

    fn display_balance(&self, _address: Address, _decimals: u8) -> RpcRes<BigDecimal> { todo!() }

    fn display_balances(&self, _addresses: Vec<Address>, _decimals: u8) -> UtxoRpcFut<Vec<(Address, BigDecimal)>> {
        todo!()
    }

    fn estimate_fee_sat(
        &self,
        _decimals: u8,
        _fee_method: &EstimateFeeMethod,
        _mode: &Option<EstimateFeeMode>,
        _n_blocks: u32,
    ) -> UtxoRpcFut<u64> {
        todo!()
    }

    fn get_relay_fee(&self) -> RpcRes<BigDecimal> { todo!() }

    fn find_output_spend(
        &self,
        _tx_hash: primitives::hash::H256,
        _script_pubkey: &[u8],
        _vout: usize,
        _from_block: BlockHashOrHeight,
    ) -> Box<dyn Future<Item = Option<SpentOutputInfo>, Error = String> + Send> {
        todo!()
    }

    fn get_median_time_past(
        &self,
        _starting_block: u64,
        _count: NonZeroU64,
        _coin_variant: CoinVariant,
    ) -> UtxoRpcFut<u32> {
        todo!()
    }

    async fn get_block_timestamp(&self, _height: u64) -> Result<u64, MmError<GetBlockHeaderError>> { todo!() }
}

#[derive(Debug, Display)]
pub enum BlockBookClientError {
    Transport(String),
    ResponseError(String),
    #[display(fmt = "'{}' asset is not yet supported by blockbook", coin)]
    NotSupported {
        coin: String,
    },
}

#[test]
fn test_block_hash() {
    let blockbook = BlockBookClient::new(BTC_BLOCKBOOK_ENPOINT, "BTC");
    let block_hash = block_on(blockbook.get_block_hash(0)).unwrap();
    let genesis_block = H256::from_str("000000000019d6689c085ae165831e934ff763ae46a2a6c172b3f1b60a8ce26f").unwrap();

    assert_eq!(genesis_block, block_hash);
}

#[test]
fn test_get_transaction() {
    let blockbook = BlockBookClient::new(BTC_BLOCKBOOK_ENPOINT, "BTC");
    let transaction =
        block_on(blockbook.get_transaction("bdb31013359ff66978e7a8bba987ba718a556c85c4051ddb1e83b1b36860734b"))
            .unwrap();

    let expected_tx = BlockBookTransaction {
        hex: Bytes(vec![
            1, 0, 0, 0, 0, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 255, 255, 255, 255, 80, 3, 32, 226, 11, 19, 98, 105, 110, 97, 110, 99, 101, 47, 56, 48, 55, 178,
            0, 48, 0, 35, 147, 150, 8, 250, 190, 109, 109, 85, 246, 160, 219, 230, 155, 23, 161, 229, 13, 137, 107, 38,
            16, 16, 102, 227, 14, 26, 226, 84, 95, 222, 114, 228, 115, 87, 134, 57, 166, 219, 20, 4, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 205, 134, 50, 142, 29, 0, 0, 0, 0, 0, 255, 255, 255, 255, 3, 210, 124, 195, 37, 0, 0, 0, 0, 23,
            169, 20, 202, 53, 177, 244, 208, 41, 7, 49, 72, 82, 240, 153, 53, 185, 96, 69, 7, 248, 215, 0, 135, 0, 0,
            0, 0, 0, 0, 0, 0, 38, 106, 36, 170, 33, 169, 237, 221, 103, 104, 188, 22, 61, 130, 222, 19, 18, 194, 188,
            165, 199, 134, 34, 252, 224, 38, 69, 251, 200, 242, 113, 177, 22, 24, 212, 103, 41, 103, 117, 0, 0, 0, 0,
            0, 0, 0, 0, 43, 106, 41, 82, 83, 75, 66, 76, 79, 67, 75, 58, 67, 117, 154, 168, 149, 76, 65, 156, 1, 144,
            79, 219, 132, 54, 75, 40, 241, 84, 76, 217, 31, 158, 196, 177, 90, 39, 117, 36, 0, 77, 184, 95, 1, 32, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        ]),
        txid: H256::from_str("bdb31013359ff66978e7a8bba987ba718a556c85c4051ddb1e83b1b36860734b").unwrap(),
        version: 1,
        block_hash: H256::from_str("00000000000000000003bde76adcbc25d0be3020ab630336cabb4ccf75c18555").unwrap(),
        block_time: 1677663144,
        height: Some(778784),
        size: Some(298),
        vsize: Some(271),
        vin: vec![TransactionInputEnum::Coinbase(CoinbaseTransactionInput {
            coinbase: Bytes(vec![
                3, 32, 226, 11, 19, 98, 105, 110, 97, 110, 99, 101, 47, 56, 48, 55, 178, 0, 48, 0, 35, 147, 150, 8,
                250, 190, 109, 109, 85, 246, 160, 219, 230, 155, 23, 161, 229, 13, 137, 107, 38, 16, 16, 102, 227, 14,
                26, 226, 84, 95, 222, 114, 228, 115, 87, 134, 57, 166, 219, 20, 4, 0, 0, 0, 0, 0, 0, 0, 0, 0, 205, 134,
                50, 142, 29, 0, 0, 0, 0, 0,
            ]),
            sequence: 4294967295,
        })],
        vout: vec![
            BlockBookTransactionOutput {
                value: Some(6.33568466),
                n: 0,
                hex: Bytes(vec![
                    169, 20, 202, 53, 177, 244, 208, 41, 7, 49, 72, 82, 240, 153, 53, 185, 96, 69, 7, 248, 215, 0, 135,
                ]),
                addresses: Some(vec!["3L8Ck6bm3sve1vJGKo6Ht2k167YKSKi8TZ".to_string()]),
                is_address: true,
                spent_txid: Some(
                    H256::from_str("5f9aae3398190cd6b64cf10d5c1a87dc2fbd5f68b7c05179ab1cf8709e614817").unwrap(),
                ),
                spent_index: Some(18),
                spent_height: Some(778917),
            },
            BlockBookTransactionOutput {
                value: Some(0.0),
                n: 1,
                hex: Bytes(vec![
                    106, 36, 170, 33, 169, 237, 221, 103, 104, 188, 22, 61, 130, 222, 19, 18, 194, 188, 165, 199, 134,
                    34, 252, 224, 38, 69, 251, 200, 242, 113, 177, 22, 24, 212, 103, 41, 103, 117,
                ]),
                addresses: Some(vec![
                    "OP_RETURN aa21a9eddd6768bc163d82de1312c2bca5c78622fce02645fbc8f271b11618d467296775".to_string(),
                ]),
                is_address: false,
                spent_txid: None,
                spent_index: None,
                spent_height: None,
            },
            BlockBookTransactionOutput {
                value: Some(0.0),
                n: 2,
                hex: Bytes(vec![
                    106, 41, 82, 83, 75, 66, 76, 79, 67, 75, 58, 67, 117, 154, 168, 149, 76, 65, 156, 1, 144, 79, 219,
                    132, 54, 75, 40, 241, 84, 76, 217, 31, 158, 196, 177, 90, 39, 117, 36, 0, 77, 184, 95,
                ]),
                addresses: Some(vec![
                    "OP_RETURN 52534b424c4f434b3a43759aa8954c419c01904fdb84364b28f1544cd91f9ec4b15a277524004db85f"
                        .to_string(),
                ]),
                is_address: false,
                spent_txid: None,
                spent_index: None,
                spent_height: None,
            },
        ],
        confirmations: 18223,
        confirmations_eta_blocks: 0,
        confirmations_eta_seconds: 0,
        value: Some(6.33568466),
        value_in: Some(0.0),
        fees: Some(0.0),
    };

    assert_eq!(expected_tx, transaction);
}
