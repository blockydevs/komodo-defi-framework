use super::{rpc::*, AllBalancesResult, TendermintCoin, TendermintCoinRpcError, TendermintToken};

use crate::my_tx_history_v2::{CoinWithTxHistoryV2, MyTxHistoryErrorV2, MyTxHistoryTarget, TxHistoryStorage};
use crate::tendermint::TendermintFeeDetails;
use crate::tx_history_storage::{GetTxHistoryFilters, WalletId};
use crate::utxo::utxo_common::big_decimal_from_sat_unsigned;
use crate::{HistorySyncState, MarketCoinOps, TransactionDetails, TransactionType, TxFeeDetails};
use async_trait::async_trait;
use bitcrypto::sha256;
use common::executor::Timer;
use common::log;
use common::state_machine::prelude::*;
use cosmrs::tendermint::abci::Code as TxCode;
use cosmrs::tendermint::abci::Event;
use cosmrs::tx::Fee;
use mm2_core::mm_ctx::MmArc;
use mm2_err_handle::prelude::MmResult;
use mm2_number::BigDecimal;
use primitives::hash::H256;
use rpc::v1::types::Bytes as BytesJson;
use std::cmp;

macro_rules! try_or_return_stopped_as_err {
    ($exp:expr, $reason: expr, $fmt:literal) => {
        match $exp {
            Ok(t) => t,
            Err(e) => {
                return Err(Stopped {
                    phantom: Default::default(),
                    stop_reason: $reason(format!("{}: {}", $fmt, e)),
                })
            },
        }
    };
}

macro_rules! try_or_continue {
    ($exp:expr, $fmt:literal) => {
        match $exp {
            Ok(t) => t,
            Err(e) => {
                log::debug!("{}: {}", $fmt, e);
                continue;
            },
        }
    };
}

macro_rules! some_or_continue {
    ($exp:expr) => {
        match $exp {
            Some(t) => t,
            None => {
                continue;
            },
        }
    };
}

macro_rules! some_or_return {
    ($exp:expr) => {
        match $exp {
            Some(t) => t,
            None => {
                return;
            },
        }
    };
}

#[async_trait]
pub trait TendermintTxHistoryOps: CoinWithTxHistoryV2 + MarketCoinOps + Send + Sync + 'static {
    async fn get_rpc_client(&self) -> MmResult<HttpClient, TendermintCoinRpcError>;

    fn decimals(&self) -> u8;

    fn platform_denom(&self) -> String;

    fn set_history_sync_state(&self, new_state: HistorySyncState);

    async fn all_balances(&self) -> MmResult<AllBalancesResult, TendermintCoinRpcError>;
}

#[async_trait]
impl CoinWithTxHistoryV2 for TendermintCoin {
    fn history_wallet_id(&self) -> WalletId { WalletId::new(self.ticker().into()) }

    async fn get_tx_history_filters(
        &self,
        _target: MyTxHistoryTarget,
    ) -> MmResult<GetTxHistoryFilters, MyTxHistoryErrorV2> {
        Ok(GetTxHistoryFilters::for_address(self.account_id.to_string()))
    }
}

#[async_trait]
impl CoinWithTxHistoryV2 for TendermintToken {
    fn history_wallet_id(&self) -> WalletId { WalletId::new(self.platform_ticker().into()) }

    async fn get_tx_history_filters(
        &self,
        _target: MyTxHistoryTarget,
    ) -> MmResult<GetTxHistoryFilters, MyTxHistoryErrorV2> {
        let denom_hash = sha256(self.denom.to_string().as_bytes());
        let id = H256::from(denom_hash.as_slice());

        Ok(GetTxHistoryFilters::for_address(self.platform_coin.account_id.to_string()).with_token_id(id.to_string()))
    }
}

struct TendermintTxHistoryCtx<Coin: TendermintTxHistoryOps, Storage: TxHistoryStorage> {
    coin: Coin,
    storage: Storage,
    balances: AllBalancesResult,
}

struct TendermintInit<Coin, Storage> {
    phantom: std::marker::PhantomData<(Coin, Storage)>,
}

impl<Coin, Storage> TendermintInit<Coin, Storage> {
    fn new() -> Self {
        TendermintInit {
            phantom: Default::default(),
        }
    }
}

#[derive(Debug)]
enum StopReason {
    StorageError(String),
    RpcClient(String),
}

struct Stopped<Coin, Storage> {
    phantom: std::marker::PhantomData<(Coin, Storage)>,
    stop_reason: StopReason,
}

impl<Coin, Storage> Stopped<Coin, Storage> {
    fn storage_error<E>(e: E) -> Self
    where
        E: std::fmt::Debug,
    {
        Stopped {
            phantom: Default::default(),
            stop_reason: StopReason::StorageError(format!("{:?}", e)),
        }
    }
}

struct WaitForHistoryUpdateTrigger<Coin, Storage> {
    address: String,
    last_height_state: u64,
    phantom: std::marker::PhantomData<(Coin, Storage)>,
}

impl<Coin, Storage> WaitForHistoryUpdateTrigger<Coin, Storage> {
    fn new(address: String, last_height_state: u64) -> Self {
        WaitForHistoryUpdateTrigger {
            address,
            last_height_state,
            phantom: Default::default(),
        }
    }
}

struct OnIoErrorCooldown<Coin, Storage> {
    address: String,
    last_block_height: u64,
    phantom: std::marker::PhantomData<(Coin, Storage)>,
}

impl<Coin, Storage> OnIoErrorCooldown<Coin, Storage> {
    fn new(address: String, last_block_height: u64) -> Self {
        OnIoErrorCooldown {
            address,
            last_block_height,
            phantom: Default::default(),
        }
    }
}

impl<Coin, Storage> TransitionFrom<FetchingTransactionsData<Coin, Storage>> for OnIoErrorCooldown<Coin, Storage> {}

#[async_trait]
impl<Coin, Storage> State for OnIoErrorCooldown<Coin, Storage>
where
    Coin: TendermintTxHistoryOps,
    Storage: TxHistoryStorage,
{
    type Ctx = TendermintTxHistoryCtx<Coin, Storage>;
    type Result = ();

    async fn on_changed(mut self: Box<Self>, _ctx: &mut Self::Ctx) -> StateResult<Self::Ctx, Self::Result> {
        Timer::sleep(30.).await;

        // retry history fetching process from last saved block
        return Self::change_state(FetchingTransactionsData::new(self.address, self.last_block_height));
    }
}

struct FetchingTransactionsData<Coin, Storage> {
    /// The list of addresses for those we have requested [`UpdatingUnconfirmedTxes::all_tx_ids_with_height`] TX hashses
    /// at the `FetchingTxHashes` state.
    address: String,
    from_block_height: u64,
    phantom: std::marker::PhantomData<(Coin, Storage)>,
}

impl<Coin, Storage> FetchingTransactionsData<Coin, Storage> {
    fn new(address: String, from_block_height: u64) -> Self {
        FetchingTransactionsData {
            address,
            phantom: Default::default(),
            from_block_height,
        }
    }
}

impl<Coin, Storage> TransitionFrom<TendermintInit<Coin, Storage>> for Stopped<Coin, Storage> {}
impl<Coin, Storage> TransitionFrom<TendermintInit<Coin, Storage>> for FetchingTransactionsData<Coin, Storage> {}
impl<Coin, Storage> TransitionFrom<OnIoErrorCooldown<Coin, Storage>> for FetchingTransactionsData<Coin, Storage> {}
impl<Coin, Storage> TransitionFrom<WaitForHistoryUpdateTrigger<Coin, Storage>> for OnIoErrorCooldown<Coin, Storage> {}
impl<Coin, Storage> TransitionFrom<WaitForHistoryUpdateTrigger<Coin, Storage>> for Stopped<Coin, Storage> {}
impl<Coin, Storage> TransitionFrom<FetchingTransactionsData<Coin, Storage>> for Stopped<Coin, Storage> {}

impl<Coin, Storage> TransitionFrom<WaitForHistoryUpdateTrigger<Coin, Storage>>
    for FetchingTransactionsData<Coin, Storage>
{
}

impl<Coin, Storage> TransitionFrom<FetchingTransactionsData<Coin, Storage>>
    for WaitForHistoryUpdateTrigger<Coin, Storage>
{
}

#[async_trait]
impl<Coin, Storage> State for WaitForHistoryUpdateTrigger<Coin, Storage>
where
    Coin: TendermintTxHistoryOps,
    Storage: TxHistoryStorage,
{
    type Ctx = TendermintTxHistoryCtx<Coin, Storage>;
    type Result = ();

    async fn on_changed(self: Box<Self>, ctx: &mut Self::Ctx) -> StateResult<Self::Ctx, Self::Result> {
        loop {
            Timer::sleep(30.).await;

            let ctx_balances = ctx.balances.clone();

            let balances = match ctx.coin.all_balances().await {
                Ok(balances) => balances,
                Err(_) => {
                    return Self::change_state(OnIoErrorCooldown::new(self.address.clone(), self.last_height_state));
                },
            };

            if balances != ctx_balances {
                // Update balances
                ctx.balances = balances;

                return Self::change_state(FetchingTransactionsData::new(
                    self.address.clone(),
                    self.last_height_state,
                ));
            }
        }
    }
}

#[async_trait]
impl<Coin, Storage> State for FetchingTransactionsData<Coin, Storage>
where
    Coin: TendermintTxHistoryOps,
    Storage: TxHistoryStorage,
{
    type Ctx = TendermintTxHistoryCtx<Coin, Storage>;
    type Result = ();

    async fn on_changed(self: Box<Self>, ctx: &mut Self::Ctx) -> StateResult<Self::Ctx, Self::Result> {
        const TX_PAGE_SIZE: u8 = 50;

        const DEFAULT_TRANSFER_EVENT_COUNT: usize = 1;
        const CREATE_HTLC_EVENT: &str = "create_htlc";
        const CLAIM_HTLC_EVENT: &str = "claim_htlc";
        const TRANSFER_EVENT: &str = "transfer";
        const ACCEPTED_EVENTS: &[&str] = &[CREATE_HTLC_EVENT, CLAIM_HTLC_EVENT, TRANSFER_EVENT];
        const RECIPIENT_TAG_KEY: &str = "recipient";
        const SENDER_TAG_KEY: &str = "sender";
        const RECEIVER_TAG_KEY: &str = "receiver";
        const AMOUNT_TAG_KEY: &str = "amount";

        struct TxAmounts {
            total: BigDecimal,
            spent_by_me: BigDecimal,
            received_by_me: BigDecimal,
        }

        fn get_tx_amounts(amount: u64, sent_by_me: bool, is_self_transfer: bool) -> TxAmounts {
            let amount = BigDecimal::from(amount);

            let spent_by_me = if sent_by_me && !is_self_transfer {
                amount.clone()
            } else {
                BigDecimal::default()
            };

            let received_by_me = if !sent_by_me || is_self_transfer {
                amount.clone()
            } else {
                BigDecimal::default()
            };

            TxAmounts {
                total: amount,
                spent_by_me,
                received_by_me,
            }
        }

        fn get_fee_details<Coin>(fee: Fee, coin: &Coin) -> Result<TendermintFeeDetails, String>
        where
            Coin: TendermintTxHistoryOps,
        {
            let fee_coin = fee
                .amount
                .first()
                .ok_or_else(|| "fee coin can't be empty".to_string())?;
            let fee_uamount: u64 = fee_coin.amount.to_string().parse().map_err(|e| format!("{:?}", e))?;

            Ok(TendermintFeeDetails {
                coin: coin.platform_ticker().to_string(),
                amount: big_decimal_from_sat_unsigned(fee_uamount, coin.decimals()),
                uamount: fee_uamount,
                gas_limit: fee.gas_limit.value(),
            })
        }

        #[derive(Clone)]
        struct TransferDetails {
            from: String,
            to: String,
            denom: String,
            amount: u64,
        }

        // updates sender and receiver addressses if tx is htlc, and if not leaves as it is.
        fn fix_tx_addresses_if_htlc(transfer_details: &mut TransferDetails, msg_event: &&Event, event_type: &str) {
            match event_type {
                CREATE_HTLC_EVENT => {
                    transfer_details.from = some_or_return!(msg_event
                        .attributes
                        .iter()
                        .find(|tag| tag.key.to_string() == SENDER_TAG_KEY))
                    .value
                    .to_string();

                    transfer_details.to = some_or_return!(msg_event
                        .attributes
                        .iter()
                        .find(|tag| tag.key.to_string() == RECEIVER_TAG_KEY))
                    .value
                    .to_string();
                },
                CLAIM_HTLC_EVENT => {
                    transfer_details.from = some_or_return!(msg_event
                        .attributes
                        .iter()
                        .find(|tag| tag.key.to_string() == SENDER_TAG_KEY))
                    .value
                    .to_string();
                },
                _ => {},
            }
        }

        fn parse_transfer_values_from_events(tx_events: Vec<&Event>) -> Vec<TransferDetails> {
            let mut transfer_details_list = vec![];

            for (index, event) in tx_events.iter().enumerate() {
                if event.type_str.as_str() == TRANSFER_EVENT {
                    let amount_with_denoms = some_or_continue!(event
                        .attributes
                        .iter()
                        .find(|tag| tag.key.to_string() == AMOUNT_TAG_KEY))
                    .value
                    .to_string();
                    let amount_with_denoms = amount_with_denoms.split(',');

                    // TODO
                    // 4999017E91E576DD2E53E034EB8F7F52637C8DA35A29DBCD466DCD2DE3A0FBD2
                    for amount_with_denom in amount_with_denoms {
                        let extracted_amount: String =
                            amount_with_denom.chars().take_while(|c| c.is_numeric()).collect();
                        let denom = &amount_with_denom[extracted_amount.len()..];
                        let amount = some_or_continue!(extracted_amount.parse().ok());

                        let from = some_or_continue!(event
                            .attributes
                            .iter()
                            .find(|tag| tag.key.to_string() == SENDER_TAG_KEY))
                        .value
                        .to_string();

                        let to = some_or_continue!(event
                            .attributes
                            .iter()
                            .find(|tag| tag.key.to_string() == RECIPIENT_TAG_KEY))
                        .value
                        .to_string();

                        let mut tx_details = TransferDetails {
                            from,
                            to,
                            denom: denom.to_owned(),
                            amount,
                        };

                        if index != 0 {
                            // If previous message is htlc related, that means current transfer
                            // addresses will be wrong.
                            if let Some(prev_event) = tx_events.get(index - 1) {
                                if [CREATE_HTLC_EVENT, CLAIM_HTLC_EVENT].contains(&prev_event.type_str.as_str()) {
                                    fix_tx_addresses_if_htlc(&mut tx_details, prev_event, prev_event.type_str.as_str());
                                }
                            };
                        }

                        transfer_details_list.push(tx_details);
                    }
                }
            }

            transfer_details_list
        }

        fn get_transfer_details(tx_events: Vec<Event>, fee_amount_with_denom: String) -> Vec<TransferDetails> {
            // Filter out irrelevant events
            let mut events: Vec<&Event> = tx_events
                .iter()
                .filter(|event| ACCEPTED_EVENTS.contains(&event.type_str.as_str()))
                .collect();

            events.reverse();

            if events.len() > DEFAULT_TRANSFER_EVENT_COUNT {
                // Retain fee related events
                events.retain(|event| {
                    if event.type_str == TRANSFER_EVENT {
                        let amount_with_denom = event
                            .attributes
                            .iter()
                            .find(|tag| tag.key.to_string() == AMOUNT_TAG_KEY)
                            .map(|t| t.value.to_string());

                        amount_with_denom != Some(fee_amount_with_denom.clone())
                    } else {
                        true
                    }
                });
            }

            parse_transfer_values_from_events(events)
        }

        async fn fetch_and_insert_txs<Coin, Storage>(
            address: String,
            coin: &Coin,
            storage: &Storage,
            query: String,
            from_height: u64,
        ) -> Result<u64, Stopped<Coin, Storage>>
        where
            Coin: TendermintTxHistoryOps,
            Storage: TxHistoryStorage,
        {
            let mut page = 1;
            let mut iterate_more = true;
            let mut highest_height = from_height;

            let client = try_or_return_stopped_as_err!(
                coin.get_rpc_client().await,
                StopReason::RpcClient,
                "could not get rpc client"
            );

            while iterate_more {
                let response = try_or_return_stopped_as_err!(
                    client
                        .perform(TxSearchRequest::new(
                            query.clone(),
                            false,
                            page,
                            TX_PAGE_SIZE,
                            TendermintResultOrder::Ascending.into(),
                        ))
                        .await,
                    StopReason::RpcClient,
                    "tx search rpc call failed"
                );

                let mut tx_details = vec![];
                for tx in response.txs {
                    if tx.tx_result.code != TxCode::Ok {
                        continue;
                    }

                    let tx_hash = tx.hash.to_string();

                    highest_height = cmp::max(highest_height, tx.height.into());

                    let deserialized_tx = try_or_continue!(
                        cosmrs::Tx::from_bytes(tx.tx.as_bytes()),
                        "Could not deserialize transaction"
                    );

                    let msg = try_or_continue!(
                        deserialized_tx.body.messages.first().ok_or("Tx body couldn't be read."),
                        "Tx body messages is empty"
                    )
                    .value
                    .as_slice();

                    let fee_data = match deserialized_tx.auth_info.fee.amount.first() {
                        Some(data) => data,
                        None => {
                            log::debug!("Could not read transaction fee for tx '{}', skipping it", &tx_hash);
                            continue;
                        },
                    };

                    let fee_amount_with_denom = format!("{}{}", fee_data.amount, fee_data.denom);

                    let transfer_details_list = get_transfer_details(tx.tx_result.events, fee_amount_with_denom);

                    if transfer_details_list.is_empty() {
                        log::debug!(
                            "Could not find transfer details in events for tx '{}', skipping it",
                            &tx_hash
                        );
                        continue;
                    }

                    let fee_details = try_or_continue!(
                        get_fee_details(deserialized_tx.auth_info.fee, coin),
                        "get_fee_details failed"
                    );

                    let mut fee_added = false;
                    for (index, transfer_details) in transfer_details_list.iter().enumerate() {
                        let mut internal_id_hash = index.to_le_bytes().to_vec();
                        internal_id_hash.extend_from_slice(tx_hash.as_bytes());
                        drop_mutability!(internal_id_hash);

                        let internal_id = H256::from(internal_id_hash.as_slice()).reversed().to_vec().into();

                        if let Ok(Some(_)) = storage
                            .get_tx_from_history(&coin.history_wallet_id(), &internal_id)
                            .await
                        {
                            log::debug!("Tx '{}' already exists in tx_history. Skipping it.", &tx_hash);
                            continue;
                        }

                        let tx_sent_by_me = address.clone() == transfer_details.from;
                        let is_platform_coin_tx = transfer_details.denom == coin.platform_denom();
                        let is_self_tx = transfer_details.to == transfer_details.from;
                        let mut tx_amounts = get_tx_amounts(transfer_details.amount, tx_sent_by_me, is_self_tx);

                        if !fee_added
                        // if tx is platform coin tx and sent by me
                            && is_platform_coin_tx && tx_sent_by_me && !is_self_tx
                        {
                            tx_amounts.total += BigDecimal::from(fee_details.uamount);
                            tx_amounts.spent_by_me += BigDecimal::from(fee_details.uamount);
                            fee_added = true;
                        }
                        drop_mutability!(tx_amounts);

                        let token_id: Option<BytesJson> = match !is_platform_coin_tx {
                            true => {
                                let denom_hash = sha256(transfer_details.denom.clone().as_bytes());
                                Some(H256::from(denom_hash.as_slice()).to_vec().into())
                            },
                            false => None,
                        };

                        let transaction_type = if let Some(token_id) = token_id.clone() {
                            TransactionType::TokenTransfer(token_id)
                        } else {
                            TransactionType::StandardTransfer
                        };

                        let details = TransactionDetails {
                            from: vec![transfer_details.from.clone()],
                            to: vec![transfer_details.to.clone()],
                            total_amount: tx_amounts.total,
                            spent_by_me: tx_amounts.spent_by_me,
                            received_by_me: tx_amounts.received_by_me,
                            my_balance_change: BigDecimal::default(),
                            tx_hash: tx_hash.to_string(),
                            tx_hex: msg.into(),
                            fee_details: Some(TxFeeDetails::Tendermint(fee_details.clone())),
                            block_height: tx.height.into(),
                            coin: transfer_details.denom.clone(),
                            internal_id,
                            timestamp: common::now_ms() / 1000,
                            kmd_rewards: None,
                            transaction_type,
                        };
                        tx_details.push(details.clone());

                        // Display fees as extra transactions for asset txs sent by user
                        if !fee_added {
                            if let Some(token_id) = token_id {
                                if !tx_sent_by_me {
                                    continue;
                                }

                                let fee_details = fee_details.clone();
                                let mut fee_tx_details = details;
                                fee_tx_details.to = vec![];
                                fee_tx_details.total_amount = fee_details.amount.clone();
                                fee_tx_details.spent_by_me = fee_details.amount.clone();
                                fee_tx_details.received_by_me = BigDecimal::default();
                                fee_tx_details.my_balance_change = BigDecimal::default() - &fee_details.amount;
                                fee_tx_details.fee_details = None;
                                fee_tx_details.coin = coin.platform_ticker().to_string();
                                // Non-reversed version of original internal id
                                fee_tx_details.internal_id = H256::from(internal_id_hash.as_slice()).to_vec().into();
                                fee_tx_details.transaction_type = TransactionType::Fee(token_id);

                                tx_details.push(fee_tx_details);
                                fee_added = true;
                            }
                        }
                    }

                    log::debug!("Tx '{}' successfuly parsed.", tx.hash);
                }

                try_or_return_stopped_as_err!(
                    storage
                        .add_transactions_to_history(&coin.history_wallet_id(), tx_details)
                        .await
                        .map_err(|e| format!("{:?}", e)),
                    StopReason::StorageError,
                    "add_transactions_to_history failed"
                );

                iterate_more = (TX_PAGE_SIZE as u32 * page) < response.total_count;
                page += 1;
            }

            Ok(highest_height)
        }

        let q = format!(
            "coin_spent.spender = '{}' AND tx.height > {}",
            self.address.clone(),
            self.from_block_height
        );
        let highest_send_tx_height = match fetch_and_insert_txs(
            self.address.clone(),
            &ctx.coin,
            &ctx.storage,
            q,
            self.from_block_height,
        )
        .await
        {
            Ok(block) => block,
            Err(stopped) => {
                if let StopReason::RpcClient(e) = &stopped.stop_reason {
                    log::error!("Sent tx history process turned into cooldown mode due to rpc error: {e}");
                    return Self::change_state(OnIoErrorCooldown::new(self.address.clone(), self.from_block_height));
                }

                return Self::change_state(stopped);
            },
        };

        let q = format!(
            "coin_received.receiver = '{}' AND tx.height > {}",
            self.address.clone(),
            self.from_block_height
        );
        let highest_recieved_tx_height = match fetch_and_insert_txs(
            self.address.clone(),
            &ctx.coin,
            &ctx.storage,
            q,
            self.from_block_height,
        )
        .await
        {
            Ok(block) => block,
            Err(stopped) => {
                if let StopReason::RpcClient(e) = &stopped.stop_reason {
                    log::error!("Received tx history process turned into cooldown mode due to rpc error: {e}");
                    return Self::change_state(OnIoErrorCooldown::new(self.address.clone(), self.from_block_height));
                }

                return Self::change_state(stopped);
            },
        };

        let last_fetched_block = cmp::max(highest_send_tx_height, highest_recieved_tx_height);

        log::info!(
            "Tx history fetching finished for {}. Last fetched block {}",
            ctx.coin.platform_ticker(),
            last_fetched_block
        );

        ctx.coin.set_history_sync_state(HistorySyncState::Finished);
        Self::change_state(WaitForHistoryUpdateTrigger::new(
            self.address.clone(),
            last_fetched_block,
        ))
    }
}

#[async_trait]
impl<Coin, Storage> State for TendermintInit<Coin, Storage>
where
    Coin: TendermintTxHistoryOps,
    Storage: TxHistoryStorage,
{
    type Ctx = TendermintTxHistoryCtx<Coin, Storage>;
    type Result = ();

    async fn on_changed(self: Box<Self>, ctx: &mut Self::Ctx) -> StateResult<Self::Ctx, Self::Result> {
        const INITIAL_SEARCH_HEIGHT: u64 = 0;

        ctx.coin.set_history_sync_state(HistorySyncState::NotStarted);

        if let Err(e) = ctx.storage.init(&ctx.coin.history_wallet_id()).await {
            return Self::change_state(Stopped::storage_error(e));
        }

        let _search_from = match ctx
            .storage
            .get_highest_block_height(&ctx.coin.history_wallet_id())
            .await
        {
            Ok(Some(height)) if height > 0 => height as u64 - 1,
            _ => INITIAL_SEARCH_HEIGHT,
        };

        let search_from = 6135052;

        Self::change_state(FetchingTransactionsData::new(
            ctx.coin.my_address().expect("my_address can't fail"),
            search_from,
        ))
    }
}

#[async_trait]
impl<Coin, Storage> LastState for Stopped<Coin, Storage>
where
    Coin: TendermintTxHistoryOps,
    Storage: TxHistoryStorage,
{
    type Ctx = TendermintTxHistoryCtx<Coin, Storage>;
    type Result = ();

    async fn on_changed(self: Box<Self>, ctx: &mut Self::Ctx) -> Self::Result {
        log::info!(
            "Stopping tx history fetching for {}. Reason: {:?}",
            ctx.coin.ticker(),
            self.stop_reason
        );

        let new_state_json = json!({
            "message": format!("{:?}", self.stop_reason),
        });

        ctx.coin.set_history_sync_state(HistorySyncState::Error(new_state_json));
    }
}

#[async_trait]
impl TendermintTxHistoryOps for TendermintCoin {
    async fn get_rpc_client(&self) -> MmResult<HttpClient, TendermintCoinRpcError> { self.rpc_client().await }

    fn decimals(&self) -> u8 { self.decimals }

    fn platform_denom(&self) -> String { self.denom.to_string() }

    fn set_history_sync_state(&self, new_state: HistorySyncState) {
        *self.history_sync_state.lock().unwrap() = new_state;
    }

    async fn all_balances(&self) -> MmResult<AllBalancesResult, TendermintCoinRpcError> { self.all_balances().await }
}

pub async fn tendermint_history_loop(
    coin: TendermintCoin,
    storage: impl TxHistoryStorage,
    _ctx: MmArc,
    _current_balance: BigDecimal,
) {
    let balances = match coin.all_balances().await {
        Ok(balances) => balances,
        Err(e) => {
            log::error!("{}", e);
            return;
        },
    };

    let ctx = TendermintTxHistoryCtx {
        coin,
        storage,
        balances,
    };

    let state_machine: StateMachine<_, ()> = StateMachine::from_ctx(ctx);
    state_machine.run(TendermintInit::new()).await;
}
