use super::eth::{wei_from_big_decimal, EthCoin, EthCoinType, SignedEthTx, TAKER_SWAP_V2};
use super::{decode_contract_call, get_function_input_data, ParseCoinAssocTypes, RefundFundingSecretArgs,
            RefundTakerPaymentArgs, SendTakerFundingArgs, SwapTxTypeWithSecretHash, TakerPaymentStateV2, Transaction,
            TransactionErr, ValidateSwapV2TxError, ValidateSwapV2TxResult, ValidateTakerFundingArgs};
use crate::{FundingTxSpend, GenTakerFundingSpendArgs, GenTakerPaymentSpendArgs, SearchForFundingSpendErr,
            WaitForTakerPaymentSpendError};
use common::executor::Timer;
use common::now_sec;
use enum_derives::EnumFromStringify;
use ethabi::{Contract, Function, Token};
use ethcore_transaction::Action;
use ethereum_types::{Address, Public, U256};
use ethkey::public_to_address;
use futures::compat::Future01CompatExt;
use mm2_err_handle::prelude::{MapToMmResult, MmError, MmResult};
use mm2_number::BigDecimal;
use std::convert::TryInto;
use web3::types::{Transaction as Web3Tx, TransactionId};

/// ZERO_VALUE is used to represent a 0 amount in transactions where the value is encoded in the transaction input data.
/// This is typically used in function calls where the value is not directly transferred with the transaction, such as in
/// `spendTakerPayment` where the [amount](https://github.com/KomodoPlatform/etomic-swap/blob/5e15641cbf41766cd5b37b4d71842c270773f788/contracts/EtomicSwapTakerV2.sol#L166)
/// is provided as part of the input data rather than as an Ether value
pub(crate) const ZERO_VALUE: u32 = 0;
const ETH_TAKER_PAYMENT: &str = "ethTakerPayment";
const ERC20_TAKER_PAYMENT: &str = "erc20TakerPayment";
const TAKER_PAYMENT_APPROVE: &str = "takerPaymentApprove";

struct TakerFundingArgs {
    dex_fee: U256,
    payment_amount: U256,
    maker_address: Address,
    taker_secret_hash: [u8; 32],
    maker_secret_hash: [u8; 32],
    funding_time_lock: u64,
    payment_time_lock: u64,
}

struct TakerRefundArgs {
    dex_fee: U256,
    payment_amount: U256,
    maker_address: Address,
    taker_secret: [u8; 32],
    taker_secret_hash: [u8; 32],
    maker_secret_hash: [u8; 32],
    payment_time_lock: u64,
    token_address: Address,
}

impl EthCoin {
    /// Calls `"ethTakerPayment"` or `"erc20TakerPayment"` swap contract methods.
    /// Returns taker sent payment transaction.
    pub(crate) async fn send_taker_funding_impl(
        &self,
        args: SendTakerFundingArgs<'_>,
    ) -> Result<SignedEthTx, TransactionErr> {
        let taker_swap_v2_contract = self
            .swap_v2_contracts
            .as_ref()
            .map(|contracts| contracts.taker_swap_v2_contract)
            .ok_or_else(|| TransactionErr::Plain(ERRL!("Expected swap_v2_contracts to be Some, but found None")))?;
        // TODO add burnFee support
        let dex_fee = try_tx_s!(wei_from_big_decimal(&args.dex_fee.fee_amount().into(), self.decimals));

        let payment_amount = try_tx_s!(wei_from_big_decimal(
            &(args.trading_amount.clone() + args.premium_amount.clone()),
            self.decimals
        ));
        let funding_args = {
            let maker_address = public_to_address(&Public::from_slice(args.maker_pub));
            TakerFundingArgs {
                dex_fee,
                payment_amount,
                maker_address,
                taker_secret_hash: try_tx_s!(args.taker_secret_hash.try_into()),
                maker_secret_hash: try_tx_s!(args.maker_secret_hash.try_into()),
                funding_time_lock: args.funding_time_lock,
                payment_time_lock: args.payment_time_lock,
            }
        };
        match &self.coin_type {
            EthCoinType::Eth => {
                let data = try_tx_s!(self.prepare_taker_eth_funding_data(&funding_args).await);
                let eth_total_payment = payment_amount.checked_add(dex_fee).ok_or_else(|| {
                    TransactionErr::Plain(ERRL!("Overflow occurred while calculating eth_total_payment"))
                })?;
                self.sign_and_send_transaction(
                    eth_total_payment,
                    Action::Call(taker_swap_v2_contract),
                    data,
                    // TODO need new consts and params for v2 calls. now it uses v1. eth_payment_v2
                    U256::from(250_000),
                )
                .compat()
                .await
            },
            EthCoinType::Erc20 {
                platform: _,
                token_addr,
            } => {
                let allowed = self
                    .allowance(taker_swap_v2_contract)
                    .compat()
                    .await
                    .map_err(|e| TransactionErr::Plain(ERRL!("{}", e)))?;
                let data = try_tx_s!(self.prepare_taker_erc20_funding_data(&funding_args, *token_addr).await);
                if allowed < payment_amount {
                    let approved_tx = self.approve(taker_swap_v2_contract, U256::max_value()).compat().await?;
                    self.wait_for_required_allowance(taker_swap_v2_contract, payment_amount, args.funding_time_lock)
                        .compat()
                        .await
                        .map_err(|e| {
                            TransactionErr::Plain(ERRL!(
                                "Allowed value was not updated in time after sending approve transaction {:02x}: {}",
                                approved_tx.tx_hash_as_bytes(),
                                e
                            ))
                        })?;
                }
                self.sign_and_send_transaction(
                    U256::from(ZERO_VALUE),
                    Action::Call(taker_swap_v2_contract),
                    data,
                    // TODO need new consts and params for v2 calls. now it uses v1. erc20_payment_v2
                    U256::from(250_000),
                )
                .compat()
                .await
            },
            EthCoinType::Nft { .. } => Err(TransactionErr::ProtocolNotSupported(ERRL!(
                "NFT protocol is not supported for ETH and ERC20 Swaps"
            ))),
        }
    }

    pub(crate) async fn validate_taker_funding_impl(
        &self,
        args: ValidateTakerFundingArgs<'_, Self>,
    ) -> ValidateSwapV2TxResult {
        if let EthCoinType::Nft { .. } = self.coin_type {
            return MmError::err(ValidateSwapV2TxError::ProtocolNotSupported(
                "NFT protocol is not supported for ETH and ERC20 Swaps".to_string(),
            ));
        }
        let taker_swap_v2_contract = self
            .swap_v2_contracts
            .as_ref()
            .map(|contracts| contracts.taker_swap_v2_contract)
            .ok_or_else(|| {
                ValidateSwapV2TxError::Internal("Expected swap_v2_contracts to be Some, but found None".to_string())
            })?;
        validate_payment_args(args.taker_secret_hash, args.maker_secret_hash, &args.trading_amount)
            .map_err(ValidateSwapV2TxError::Internal)?;
        let taker_address = public_to_address(args.taker_pub);
        let swap_id = self.etomic_swap_id_v2(args.payment_time_lock, args.maker_secret_hash);
        let taker_status = self
            .payment_status_v2(
                taker_swap_v2_contract,
                Token::FixedBytes(swap_id.clone()),
                &TAKER_SWAP_V2,
                EthPaymentType::TakerPayments,
                3,
            )
            .await?;

        let tx_from_rpc = self.transaction(TransactionId::Hash(args.funding_tx.tx_hash())).await?;
        let tx_from_rpc = tx_from_rpc.as_ref().ok_or_else(|| {
            ValidateSwapV2TxError::TxDoesNotExist(format!(
                "Didn't find provided tx {:?} on ETH node",
                args.funding_tx.tx_hash()
            ))
        })?;
        validate_from_to_and_status(
            tx_from_rpc,
            taker_address,
            taker_swap_v2_contract,
            taker_status,
            TakerPaymentStateV2::PaymentSent as u8,
        )?;

        let validation_args = {
            let dex_fee = wei_from_big_decimal(&args.dex_fee.fee_amount().into(), self.decimals)?;
            let payment_amount = wei_from_big_decimal(&(args.trading_amount + args.premium_amount), self.decimals)?;
            TakerValidationArgs {
                swap_id,
                amount: payment_amount,
                dex_fee,
                receiver: self.my_addr().await,
                taker_secret_hash: args.taker_secret_hash,
                maker_secret_hash: args.maker_secret_hash,
                funding_time_lock: args.funding_time_lock,
                payment_time_lock: args.payment_time_lock,
            }
        };
        match self.coin_type {
            EthCoinType::Eth => {
                let function = TAKER_SWAP_V2.function(ETH_TAKER_PAYMENT)?;
                let decoded = decode_contract_call(function, &tx_from_rpc.input.0)?;
                validate_eth_taker_payment_data(&decoded, &validation_args, function, tx_from_rpc.value)?;
            },
            EthCoinType::Erc20 { token_addr, .. } => {
                let function = TAKER_SWAP_V2.function(ERC20_TAKER_PAYMENT)?;
                let decoded = decode_contract_call(function, &tx_from_rpc.input.0)?;
                validate_erc20_taker_payment_data(&decoded, &validation_args, function, token_addr)?;
            },
            EthCoinType::Nft { .. } => unreachable!(),
        }
        Ok(())
    }

    /// Taker approves payment calling `takerPaymentApprove` for EVM based chains.
    /// Function accepts taker payment transaction, returns taker approve payment transaction.
    pub(crate) async fn taker_payment_approve(
        &self,
        args: &GenTakerFundingSpendArgs<'_, Self>,
    ) -> Result<SignedEthTx, TransactionErr> {
        // TODO need new consts and params for v2 calls, here should be common `gas_limit.taker_approve_v2` param for Eth and Erc20
        let gas_limit = match self.coin_type {
            EthCoinType::Eth | EthCoinType::Erc20 { .. } => U256::from(100_000),
            EthCoinType::Nft { .. } => {
                return Err(TransactionErr::ProtocolNotSupported(ERRL!(
                    "NFT protocol is not supported for ETH and ERC20 Swaps"
                )))
            },
        };
        let (taker_swap_v2_contract, send_func, token_address) = self
            .taker_swap_v2_details(ETH_TAKER_PAYMENT, ERC20_TAKER_PAYMENT)
            .await?;
        let decoded = try_tx_s!(decode_contract_call(send_func, args.funding_tx.unsigned().data()));
        let taker_status = try_tx_s!(
            self.payment_status_v2(
                taker_swap_v2_contract,
                decoded[0].clone(),
                &TAKER_SWAP_V2,
                EthPaymentType::TakerPayments,
                3,
            )
            .await
        );
        validate_payment_state(args.funding_tx, taker_status, TakerPaymentStateV2::PaymentSent as u8)
            .map_err(|e| TransactionErr::Plain(ERRL!("{}", e)))?;
        let data = try_tx_s!(
            self.prepare_taker_payment_approve_data(args, decoded, token_address)
                .await
        );
        let approve_tx = self
            .sign_and_send_transaction(
                U256::from(ZERO_VALUE),
                Action::Call(taker_swap_v2_contract),
                data,
                gas_limit,
            )
            .compat()
            .await?;
        Ok(approve_tx)
    }

    pub(crate) async fn refund_taker_payment_with_timelock_impl(
        &self,
        args: RefundTakerPaymentArgs<'_>,
    ) -> Result<SignedEthTx, TransactionErr> {
        let (token_address, gas_limit) = match &self.coin_type {
            // TODO need new consts and params for v2 calls. eth_sender_refund_v2, erc20_sender_refund_v2
            EthCoinType::Eth => (Address::default(), 180_000),
            EthCoinType::Erc20 {
                platform: _,
                token_addr,
            } => (*token_addr, 200_000),
            EthCoinType::Nft { .. } => {
                return Err(TransactionErr::ProtocolNotSupported(ERRL!(
                    "NFT protocol is not supported for ETH and ERC20 Swaps"
                )))
            },
        };

        let taker_swap_v2_contract = self
            .swap_v2_contracts
            .as_ref()
            .map(|contracts| contracts.taker_swap_v2_contract)
            .ok_or_else(|| TransactionErr::Plain(ERRL!("Expected swap_v2_contracts to be Some, but found None")))?;
        let dex_fee = try_tx_s!(wei_from_big_decimal(
            &args.dex_fee.fee_amount().to_decimal(),
            self.decimals
        ));
        let payment_amount = try_tx_s!(wei_from_big_decimal(
            &(args.trading_amount + args.premium_amount),
            self.decimals
        ));
        let maker_address = public_to_address(&Public::from_slice(args.maker_pub));
        let (maker_secret_hash, taker_secret_hash) = match args.tx_type_with_secret_hash {
            SwapTxTypeWithSecretHash::TakerPaymentV2 {
                maker_secret_hash,
                taker_secret_hash,
            } => (maker_secret_hash, taker_secret_hash),
            _ => {
                return Err(TransactionErr::Plain(ERRL!(
                    "Unsupported swap tx type for timelock refund"
                )))
            },
        };

        let args = TakerRefundArgs {
            dex_fee,
            payment_amount,
            maker_address,
            taker_secret: [0u8; 32],
            taker_secret_hash: try_tx_s!(taker_secret_hash.try_into()),
            maker_secret_hash: try_tx_s!(maker_secret_hash.try_into()),
            payment_time_lock: args.time_lock,
            token_address,
        };
        let data = try_tx_s!(self.prepare_taker_refund_payment_timelock_data(args).await);

        self.sign_and_send_transaction(
            U256::from(ZERO_VALUE),
            Action::Call(taker_swap_v2_contract),
            data,
            U256::from(gas_limit),
        )
        .compat()
        .await
    }

    pub(crate) async fn refund_taker_funding_secret_impl(
        &self,
        args: RefundFundingSecretArgs<'_, Self>,
    ) -> Result<SignedEthTx, TransactionErr> {
        let (token_address, gas_limit) = match &self.coin_type {
            // TODO need new consts and params for v2 calls. eth_sender_refund_v2, erc20_sender_refund_v2
            EthCoinType::Eth => (Address::default(), 180_000),
            EthCoinType::Erc20 {
                platform: _,
                token_addr,
            } => (*token_addr, 200_000),
            EthCoinType::Nft { .. } => {
                return Err(TransactionErr::ProtocolNotSupported(ERRL!(
                    "NFT protocol is not supported for ETH and ERC20 Swaps"
                )))
            },
        };

        let taker_swap_v2_contract = self
            .swap_v2_contracts
            .as_ref()
            .map(|contracts| contracts.taker_swap_v2_contract)
            .ok_or_else(|| TransactionErr::Plain(ERRL!("Expected swap_v2_contracts to be Some, but found None")))?;
        let taker_secret = try_tx_s!(args.taker_secret.try_into());
        let maker_secret_hash = try_tx_s!(args.maker_secret_hash.try_into());
        let dex_fee = try_tx_s!(wei_from_big_decimal(
            &args.dex_fee.fee_amount().to_decimal(),
            self.decimals
        ));
        let payment_amount = try_tx_s!(wei_from_big_decimal(
            &(args.trading_amount + args.premium_amount),
            self.decimals
        ));
        let maker_address = public_to_address(args.maker_pubkey);

        let refund_args = TakerRefundArgs {
            dex_fee,
            payment_amount,
            maker_address,
            taker_secret,
            taker_secret_hash: [0u8; 32],
            maker_secret_hash,
            payment_time_lock: args.payment_time_lock,
            token_address,
        };
        let data = try_tx_s!(self.prepare_taker_refund_payment_secret_data(&refund_args).await);

        self.sign_and_send_transaction(
            U256::from(ZERO_VALUE),
            Action::Call(taker_swap_v2_contract),
            data,
            U256::from(gas_limit),
        )
        .compat()
        .await
    }

    /// Checks that taker payment state is `TakerApproved`.
    /// Accepts a taker-approved payment transaction and returns it if the state is correct.
    pub(crate) async fn search_for_taker_funding_spend_impl(
        &self,
        tx: &SignedEthTx,
    ) -> Result<Option<FundingTxSpend<Self>>, SearchForFundingSpendErr> {
        let (decoded, taker_swap_v2_contract) = self
            .get_decoded_and_swap_contract(tx, TAKER_PAYMENT_APPROVE)
            .await
            .map_err(|e| SearchForFundingSpendErr::Internal(ERRL!("{}", e)))?;
        let taker_status = self
            .payment_status_v2(
                taker_swap_v2_contract,
                decoded[0].clone(), // id from takerPaymentApprove
                &TAKER_SWAP_V2,
                EthPaymentType::TakerPayments,
                3,
            )
            .await
            .map_err(|e| SearchForFundingSpendErr::Internal(ERRL!("{}", e)))?;
        if taker_status == U256::from(TakerPaymentStateV2::TakerApproved as u8) {
            return Ok(Some(FundingTxSpend::TransferredToTakerPayment(tx.clone())));
        }
        Ok(None)
    }

    /// Taker swap contract `spendTakerPayment` method is called for EVM based chains.
    /// Returns maker spent payment transaction.
    pub(crate) async fn sign_and_broadcast_taker_payment_spend_impl(
        &self,
        gen_args: &GenTakerPaymentSpendArgs<'_, Self>,
        secret: &[u8],
    ) -> Result<SignedEthTx, TransactionErr> {
        // TODO need new consts and params for v2 calls. eth_receiver_spend_v2, erc20_receiver_spend_v2
        let gas_limit = match self.coin_type {
            EthCoinType::Eth => U256::from(200_000),
            EthCoinType::Erc20 { .. } => U256::from(200_000),
            EthCoinType::Nft { .. } => {
                return Err(TransactionErr::ProtocolNotSupported(ERRL!(
                    "NFT protocol is not supported for ETH and ERC20 Swaps"
                )))
            },
        };
        let (taker_swap_v2_contract, approve_func, token_address) = self
            .taker_swap_v2_details(TAKER_PAYMENT_APPROVE, TAKER_PAYMENT_APPROVE)
            .await?;
        let decoded = try_tx_s!(decode_contract_call(approve_func, gen_args.taker_tx.unsigned().data()));
        let taker_status = try_tx_s!(
            self.payment_status_v2(
                taker_swap_v2_contract,
                decoded[0].clone(),
                &TAKER_SWAP_V2,
                EthPaymentType::TakerPayments,
                3,
            )
            .await
        );
        validate_payment_state(
            gen_args.taker_tx,
            taker_status,
            TakerPaymentStateV2::TakerApproved as u8,
        )
        .map_err(|e| TransactionErr::Plain(ERRL!("{}", e)))?;
        let data = try_tx_s!(
            self.prepare_spend_taker_payment_data(gen_args, secret, decoded, token_address)
                .await
        );
        let spend_payment_tx = self
            .sign_and_send_transaction(
                U256::from(ZERO_VALUE),
                Action::Call(taker_swap_v2_contract),
                data,
                gas_limit,
            )
            .compat()
            .await?;
        Ok(spend_payment_tx)
    }

    /// Checks that taker payment state is `MakerSpent`.
    /// Accepts maker spent payment transaction and returns it if payment status is correct.
    pub(crate) async fn wait_for_taker_payment_spend_impl(
        &self,
        taker_payment: &SignedEthTx,
        wait_until: u64,
    ) -> MmResult<SignedEthTx, WaitForTakerPaymentSpendError> {
        let (decoded, taker_swap_v2_contract) = self
            .get_decoded_and_swap_contract(taker_payment, "spendTakerPayment")
            .await?;
        loop {
            let taker_status = self
                .payment_status_v2(
                    taker_swap_v2_contract,
                    decoded[0].clone(), // id from spendTakerPayment
                    &TAKER_SWAP_V2,
                    EthPaymentType::TakerPayments,
                    3,
                )
                .await?;
            if taker_status == U256::from(TakerPaymentStateV2::MakerSpent as u8) {
                return Ok(taker_payment.clone());
            }
            let now = now_sec();
            if now > wait_until {
                return MmError::err(WaitForTakerPaymentSpendError::Timeout { wait_until, now });
            }
            Timer::sleep(10.).await;
        }
    }

    /// Prepares data for EtomicSwapTakerV2 contract `ethTakerPayment` method
    ///     function ethTakerPayment(
    ///         bytes32 id,
    ///         uint256 dexFee,
    ///         address receiver,
    ///         bytes32 takerSecretHash,
    ///         bytes32 makerSecretHash,
    ///         uint32 preApproveLockTime,
    ///         uint32 paymentLockTime
    async fn prepare_taker_eth_funding_data(&self, args: &TakerFundingArgs) -> Result<Vec<u8>, PrepareTxDataError> {
        let function = TAKER_SWAP_V2.function(ETH_TAKER_PAYMENT)?;
        let id = self.etomic_swap_id_v2(args.payment_time_lock, &args.maker_secret_hash);
        let data = function.encode_input(&[
            Token::FixedBytes(id),
            Token::Uint(args.dex_fee),
            Token::Address(args.maker_address),
            Token::FixedBytes(args.taker_secret_hash.to_vec()),
            Token::FixedBytes(args.maker_secret_hash.to_vec()),
            Token::Uint(args.funding_time_lock.into()),
            Token::Uint(args.payment_time_lock.into()),
        ])?;
        Ok(data)
    }

    /// Prepares data for EtomicSwapTakerV2 contract `erc20TakerPayment` method
    ///     function erc20TakerPayment(
    ///         bytes32 id,
    ///         uint256 amount,
    ///         uint256 dexFee,
    ///         address tokenAddress,
    ///         address receiver,
    ///         bytes32 takerSecretHash,
    ///         bytes32 makerSecretHash,
    ///         uint32 preApproveLockTime,
    ///         uint32 paymentLockTime
    async fn prepare_taker_erc20_funding_data(
        &self,
        args: &TakerFundingArgs,
        token_address: Address,
    ) -> Result<Vec<u8>, PrepareTxDataError> {
        let function = TAKER_SWAP_V2.function(ERC20_TAKER_PAYMENT)?;
        let id = self.etomic_swap_id_v2(args.payment_time_lock, &args.maker_secret_hash);
        let data = function.encode_input(&[
            Token::FixedBytes(id),
            Token::Uint(args.payment_amount),
            Token::Uint(args.dex_fee),
            Token::Address(token_address),
            Token::Address(args.maker_address),
            Token::FixedBytes(args.taker_secret_hash.to_vec()),
            Token::FixedBytes(args.maker_secret_hash.to_vec()),
            Token::Uint(args.funding_time_lock.into()),
            Token::Uint(args.payment_time_lock.into()),
        ])?;
        Ok(data)
    }

    /// Prepares data for EtomicSwapTakerV2 contract `refundTakerPaymentTimelock` method
    /// function refundMakerPaymentTimelock(
    ///         bytes32 id,
    ///         uint256 amount,
    ///         address taker,
    ///         bytes32 takerSecretHash,
    ///         bytes32 makerSecretHash,
    ///         address tokenAddress
    async fn prepare_taker_refund_payment_timelock_data(
        &self,
        args: TakerRefundArgs,
    ) -> Result<Vec<u8>, PrepareTxDataError> {
        let function = TAKER_SWAP_V2.function("refundTakerPaymentTimelock")?;
        let id = self.etomic_swap_id_v2(args.payment_time_lock, &args.maker_secret_hash);
        let data = function.encode_input(&[
            Token::FixedBytes(id),
            Token::Uint(args.payment_amount),
            Token::Uint(args.dex_fee),
            Token::Address(args.maker_address),
            Token::FixedBytes(args.taker_secret_hash.to_vec()),
            Token::FixedBytes(args.maker_secret_hash.to_vec()),
            Token::Address(args.token_address),
        ])?;
        Ok(data)
    }

    /// Prepares data for EtomicSwapTakerV2 contract `refundTakerPaymentSecret` method
    /// function refundTakerPaymentSecret(
    ///         bytes32 id,
    ///         uint256 amount,
    ///         uint256 dexFee,
    ///         address maker,
    ///         bytes32 takerSecret,
    ///         bytes32 makerSecretHash,
    ///         address tokenAddress
    async fn prepare_taker_refund_payment_secret_data(
        &self,
        args: &TakerRefundArgs,
    ) -> Result<Vec<u8>, PrepareTxDataError> {
        let function = TAKER_SWAP_V2.function("refundTakerPaymentSecret")?;
        let id = self.etomic_swap_id_v2(args.payment_time_lock, &args.maker_secret_hash);
        let data = function.encode_input(&[
            Token::FixedBytes(id),
            Token::Uint(args.payment_amount),
            Token::Uint(args.dex_fee),
            Token::Address(args.maker_address),
            Token::FixedBytes(args.taker_secret.to_vec()),
            Token::FixedBytes(args.maker_secret_hash.to_vec()),
            Token::Address(args.token_address),
        ])?;
        Ok(data)
    }

    /// This function constructs the encoded transaction input data required to approve the taker payment.
    /// The `decoded` parameter should contain the transaction input data from the `ethTakerPayment` or `erc20TakerPayment` function of the EtomicSwapTakerV2 contract.
    async fn prepare_taker_payment_approve_data(
        &self,
        args: &GenTakerFundingSpendArgs<'_, Self>,
        decoded: Vec<Token>,
        token_address: Address,
    ) -> Result<Vec<u8>, PrepareTxDataError> {
        let function = TAKER_SWAP_V2.function(TAKER_PAYMENT_APPROVE)?;
        let data = match self.coin_type {
            EthCoinType::Eth => {
                let dex_fee = match &decoded[1] {
                    Token::Uint(value) => value,
                    _ => return Err(PrepareTxDataError::Internal("Invalid token type for dex fee".into())),
                };
                let amount = args
                    .funding_tx
                    .unsigned()
                    .value()
                    .checked_sub(*dex_fee)
                    .ok_or_else(|| {
                        PrepareTxDataError::Internal("Underflow occurred while calculating amount".into())
                    })?;
                function.encode_input(&[
                    decoded[0].clone(),  // id from ethTakerPayment
                    Token::Uint(amount), // calculated payment amount (tx value - dexFee)
                    decoded[1].clone(),  // dexFee from ethTakerPayment
                    decoded[2].clone(),  // receiver from ethTakerPayment
                    Token::FixedBytes(args.taker_secret_hash.to_vec()),
                    Token::FixedBytes(args.maker_secret_hash.to_vec()),
                    Token::Address(token_address), // should be zero address Address::default()
                ])?
            },
            EthCoinType::Erc20 { .. } => function.encode_input(&[
                decoded[0].clone(), // id from erc20TakerPayment
                decoded[1].clone(), // amount from erc20TakerPayment
                decoded[2].clone(), // dexFee from erc20TakerPayment
                decoded[4].clone(), // receiver from erc20TakerPayment
                Token::FixedBytes(args.taker_secret_hash.to_vec()),
                Token::FixedBytes(args.maker_secret_hash.to_vec()),
                Token::Address(token_address), // erc20 token address from EthCoinType::Erc20
            ])?,
            EthCoinType::Nft { .. } => {
                return Err(PrepareTxDataError::Internal("EthCoinType must be ETH or ERC20".into()))
            },
        };
        Ok(data)
    }

    async fn prepare_spend_taker_payment_data(
        &self,
        args: &GenTakerPaymentSpendArgs<'_, Self>,
        secret: &[u8],
        decoded: Vec<Token>,
        token_address: Address,
    ) -> Result<Vec<u8>, PrepareTxDataError> {
        let function = TAKER_SWAP_V2.function("spendTakerPayment")?;
        let taker_address = public_to_address(args.taker_pub);
        let data = function.encode_input(&[
            decoded[0].clone(),                 // id from takerPaymentApprove
            decoded[1].clone(),                 // amount from takerPaymentApprove
            decoded[2].clone(),                 // dexFee from takerPaymentApprove
            Token::Address(taker_address),      // taker address
            decoded[4].clone(),                 // takerSecretHash from ethTakerPayment
            Token::FixedBytes(secret.to_vec()), // makerSecret
            Token::Address(token_address),      // tokenAddress
        ])?;
        Ok(data)
    }

    /// Retrieves the payment status from a given smart contract address based on the swap ID and state type.
    pub(crate) async fn payment_status_v2(
        &self,
        swap_address: Address,
        swap_id: Token,
        contract_abi: &Contract,
        payment_type: EthPaymentType,
        state_index: usize,
    ) -> Result<U256, PaymentStatusErr> {
        let function_name = payment_type.as_str();
        let function = contract_abi.function(function_name)?;
        let data = function.encode_input(&[swap_id])?;
        let bytes = self
            .call_request(self.my_addr().await, swap_address, None, Some(data.into()))
            .await?;
        let decoded_tokens = function.decode_output(&bytes.0)?;

        let state = decoded_tokens.get(state_index).ok_or_else(|| {
            PaymentStatusErr::Internal(format!(
                "Payment status must contain 'state' as the {} token",
                state_index
            ))
        })?;
        match state {
            Token::Uint(state) => Ok(*state),
            _ => Err(PaymentStatusErr::InvalidData(format!(
                "Payment status must be Uint, got {:?}",
                state
            ))),
        }
    }

    /// Retrieves the taker smart contract address, the corresponding function, and the token address.
    ///
    /// Depending on the coin type (ETH or ERC20), it fetches the appropriate function name  and token address.
    /// Returns an error if the coin type is NFT or if the `swap_v2_contracts` is None.
    async fn taker_swap_v2_details(
        &self,
        eth_func_name: &str,
        erc20_func_name: &str,
    ) -> Result<(Address, &Function, Address), TransactionErr> {
        let (func, token_address) = match self.coin_type {
            EthCoinType::Eth => (try_tx_s!(TAKER_SWAP_V2.function(eth_func_name)), Address::default()),
            EthCoinType::Erc20 { token_addr, .. } => (try_tx_s!(TAKER_SWAP_V2.function(erc20_func_name)), token_addr),
            EthCoinType::Nft { .. } => {
                return Err(TransactionErr::ProtocolNotSupported(ERRL!(
                    "NFT protocol is not supported for ETH and ERC20 Swaps"
                )))
            },
        };
        let taker_swap_v2_contract = self
            .swap_v2_contracts
            .as_ref()
            .map(|contracts| contracts.taker_swap_v2_contract)
            .ok_or_else(|| TransactionErr::Plain(ERRL!("Expected swap_v2_contracts to be Some, but found None")))?;
        Ok((taker_swap_v2_contract, func, token_address))
    }

    async fn get_decoded_and_swap_contract(
        &self,
        tx: &SignedEthTx,
        function_name: &str,
    ) -> Result<(Vec<Token>, Address), PrepareTxDataError> {
        let decoded = {
            let func = match self.coin_type {
                EthCoinType::Eth | EthCoinType::Erc20 { .. } => TAKER_SWAP_V2.function(function_name)?,
                EthCoinType::Nft { .. } => {
                    return Err(PrepareTxDataError::Internal(
                        "NFT protocol is not supported for ETH and ERC20 Swaps".to_string(),
                    ));
                },
            };
            decode_contract_call(func, tx.unsigned().data())?
        };
        let taker_swap_v2_contract = self
            .swap_v2_contracts
            .as_ref()
            .map(|contracts| contracts.taker_swap_v2_contract)
            .ok_or_else(|| {
                PrepareTxDataError::Internal("Expected swap_v2_contracts to be Some, but found None".to_string())
            })?;

        Ok((decoded, taker_swap_v2_contract))
    }
}

#[derive(Debug, Display, EnumFromStringify)]
pub(crate) enum PrepareTxDataError {
    #[from_stringify("ethabi::Error")]
    #[display(fmt = "ABI error: {}", _0)]
    ABIError(String),
    #[display(fmt = "Internal error: {}", _0)]
    Internal(String),
}

// TODO validate premium when add its support in swap_v2
fn validate_payment_args<'a>(
    taker_secret_hash: &'a [u8],
    maker_secret_hash: &'a [u8],
    trading_amount: &BigDecimal,
) -> Result<(), String> {
    if !is_positive(trading_amount) {
        return Err("trading_amount must be a positive value".to_string());
    }
    if taker_secret_hash.len() != 32 {
        return Err("taker_secret_hash must be 32 bytes".to_string());
    }
    if maker_secret_hash.len() != 32 {
        return Err("maker_secret_hash must be 32 bytes".to_string());
    }
    Ok(())
}

/// function to check if BigDecimal is a positive value
#[inline(always)]
fn is_positive(amount: &BigDecimal) -> bool { amount > &BigDecimal::from(0) }

pub(crate) fn validate_from_to_and_status(
    tx_from_rpc: &Web3Tx,
    expected_from: Address,
    expected_to: Address,
    status: U256,
    expected_status: u8,
) -> Result<(), MmError<ValidatePaymentV2Err>> {
    if status != U256::from(expected_status) {
        return MmError::err(ValidatePaymentV2Err::UnexpectedPaymentState(format!(
            "Payment state is not `PaymentSent`, got {}",
            status
        )));
    }
    if tx_from_rpc.from != Some(expected_from) {
        return MmError::err(ValidatePaymentV2Err::WrongPaymentTx(format!(
            "Payment tx {:?} was sent from wrong address, expected {:?}",
            tx_from_rpc, expected_from
        )));
    }
    // (in NFT case) as NFT owner calls "safeTransferFrom" directly, then in Transaction 'to' field we expect token_address
    if tx_from_rpc.to != Some(expected_to) {
        return MmError::err(ValidatePaymentV2Err::WrongPaymentTx(format!(
            "Payment tx {:?} was sent to wrong address, expected {:?}",
            tx_from_rpc, expected_to,
        )));
    }
    Ok(())
}

struct TakerValidationArgs<'a> {
    swap_id: Vec<u8>,
    amount: U256,
    dex_fee: U256,
    receiver: Address,
    taker_secret_hash: &'a [u8],
    maker_secret_hash: &'a [u8],
    funding_time_lock: u64,
    payment_time_lock: u64,
}

/// Validation function for ETH taker payment data
fn validate_eth_taker_payment_data(
    decoded: &[Token],
    args: &TakerValidationArgs,
    func: &Function,
    tx_value: U256,
) -> Result<(), MmError<ValidateSwapV2TxError>> {
    let checks = vec![
        (0, Token::FixedBytes(args.swap_id.clone()), "id"),
        (1, Token::Uint(args.dex_fee), "dexFee"),
        (2, Token::Address(args.receiver), "receiver"),
        (3, Token::FixedBytes(args.taker_secret_hash.to_vec()), "takerSecretHash"),
        (4, Token::FixedBytes(args.maker_secret_hash.to_vec()), "makerSecretHash"),
        (5, Token::Uint(U256::from(args.funding_time_lock)), "preApproveLockTime"),
        (6, Token::Uint(U256::from(args.payment_time_lock)), "paymentLockTime"),
    ];

    for (index, expected_token, field_name) in checks {
        let token = get_function_input_data(decoded, func, index).map_to_mm(ValidateSwapV2TxError::Internal)?;
        if token != expected_token {
            return MmError::err(ValidateSwapV2TxError::WrongPaymentTx(format!(
                "ETH Taker Payment `{}` {:?} is invalid, expected {:?}",
                field_name,
                decoded.get(index),
                expected_token
            )));
        }
    }
    let total = args.amount.checked_add(args.dex_fee).ok_or_else(|| {
        ValidateSwapV2TxError::Overflow("Overflow occurred while calculating total payment".to_string())
    })?;
    if total != tx_value {
        return MmError::err(ValidateSwapV2TxError::WrongPaymentTx(format!(
            "ETH Taker Payment amount, is invalid, expected {:?}, got {:?}",
            total, tx_value
        )));
    }
    Ok(())
}

/// Validation function for ERC20 taker payment data
fn validate_erc20_taker_payment_data(
    decoded: &[Token],
    args: &TakerValidationArgs,
    func: &Function,
    token_addr: Address,
) -> Result<(), MmError<ValidateSwapV2TxError>> {
    let checks = vec![
        (0, Token::FixedBytes(args.swap_id.clone()), "id"),
        (1, Token::Uint(args.amount), "amount"),
        (2, Token::Uint(args.dex_fee), "dexFee"),
        (3, Token::Address(token_addr), "tokenAddress"),
        (4, Token::Address(args.receiver), "receiver"),
        (5, Token::FixedBytes(args.taker_secret_hash.to_vec()), "takerSecretHash"),
        (6, Token::FixedBytes(args.maker_secret_hash.to_vec()), "makerSecretHash"),
        (7, Token::Uint(U256::from(args.funding_time_lock)), "preApproveLockTime"),
        (8, Token::Uint(U256::from(args.payment_time_lock)), "paymentLockTime"),
    ];

    for (index, expected_token, field_name) in checks {
        let token = get_function_input_data(decoded, func, index).map_to_mm(ValidateSwapV2TxError::Internal)?;
        if token != expected_token {
            return MmError::err(ValidateSwapV2TxError::WrongPaymentTx(format!(
                "ERC20 Taker Payment `{}` {:?} is invalid, expected {:?}",
                field_name,
                decoded.get(index),
                expected_token
            )));
        }
    }
    Ok(())
}

pub(crate) fn validate_payment_state(
    tx: &SignedEthTx,
    state: U256,
    expected_state: u8,
) -> Result<(), PrepareTxDataError> {
    if state != U256::from(expected_state) {
        return Err(PrepareTxDataError::Internal(format!(
            "Payment {:?} state is not `{}`, got `{}`",
            tx, expected_state, state
        )));
    }
    Ok(())
}

#[derive(Debug, Display)]
pub(crate) enum ValidatePaymentV2Err {
    UnexpectedPaymentState(String),
    WrongPaymentTx(String),
}

pub(crate) enum EthPaymentType {
    MakerPayments,
    TakerPayments,
}

impl EthPaymentType {
    pub(crate) fn as_str(&self) -> &'static str {
        match self {
            EthPaymentType::MakerPayments => "makerPayments",
            EthPaymentType::TakerPayments => "takerPayments",
        }
    }
}

#[derive(Debug, Display, EnumFromStringify)]
pub(crate) enum PaymentStatusErr {
    #[from_stringify("ethabi::Error")]
    #[display(fmt = "ABI error: {}", _0)]
    ABIError(String),
    #[from_stringify("web3::Error")]
    #[display(fmt = "Transport error: {}", _0)]
    Transport(String),
    #[display(fmt = "Internal error: {}", _0)]
    Internal(String),
    #[display(fmt = "Invalid data error: {}", _0)]
    InvalidData(String),
}
