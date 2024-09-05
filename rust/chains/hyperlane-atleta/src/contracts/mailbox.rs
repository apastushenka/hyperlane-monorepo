#![allow(clippy::enum_variant_names)]
#![allow(missing_docs)]

use std::collections::HashMap;
use std::num::NonZeroU64;
use std::ops::RangeInclusive;
use std::sync::Arc;

use async_trait::async_trait;
use derive_new::new;
use ethers::abi::{AbiEncode, Detokenize};
use ethers::prelude::Middleware;
use ethers_contract::builders::ContractCall;
use ethers_contract::{Multicall, MulticallResult};
use ethers_core::types::BlockNumber;
use futures_util::future::join_all;
use hyperlane_core::{BatchResult, QueueOperation, H512};
use itertools::Itertools;
use tracing::instrument;

use hyperlane_core::{
    utils::bytes_to_hex, BatchItem, ChainCommunicationError, ChainResult, ContractLocator,
    HyperlaneAbi, HyperlaneChain, HyperlaneContract, HyperlaneDomain, HyperlaneMessage,
    HyperlaneProtocolError, HyperlaneProvider, Indexed, Indexer, LogMeta, Mailbox,
    RawHyperlaneMessage, SequenceAwareIndexer, TxCostEstimate, TxOutcome, H160, H256, U256,
};
use hyperlane_ethereum::{
    BuildableWithProvider, ConnectionConf, EthereumProvider, TransactionOverrides,
};

use crate::error::HyperlaneEthereumError;
use crate::interfaces::arbitrum_node_interface::ArbitrumNodeInterface;
use crate::interfaces::i_mailbox::{
    IMailbox as EthereumMailboxInternal, ProcessCall, IMAILBOX_ABI,
};
use crate::interfaces::mailbox::DispatchFilter;
use crate::middleware_ext::{MiddlewareExt, BLOCK_ERROR_MSG};
use crate::tx::{ensure_block_finalized, fill_tx_gas_params, report_tx};

use super::multicall::{self, build_multicall};
use super::utils::fetch_raw_logs_and_log_meta;

impl<M> std::fmt::Display for EthereumMailboxInternal<M>
where
    M: Middleware,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}

pub struct SequenceIndexerBuilder {}

#[async_trait]
impl BuildableWithProvider for SequenceIndexerBuilder {
    type Output = Box<dyn SequenceAwareIndexer<HyperlaneMessage>>;
    const NEEDS_SIGNER: bool = false;

    async fn build_with_provider<M: Middleware + 'static>(
        &self,
        provider: M,
        _conn: &ConnectionConf,
        locator: &ContractLocator,
    ) -> Self::Output {
        Box::new(EthereumMailboxIndexer::new(Arc::new(provider), locator))
    }
}

pub struct DeliveryIndexerBuilder {}

#[async_trait]
impl BuildableWithProvider for DeliveryIndexerBuilder {
    type Output = Box<dyn SequenceAwareIndexer<H256>>;
    const NEEDS_SIGNER: bool = false;

    async fn build_with_provider<M: Middleware + 'static>(
        &self,
        provider: M,
        _conn: &ConnectionConf,
        locator: &ContractLocator,
    ) -> Self::Output {
        Box::new(EthereumMailboxIndexer::new(Arc::new(provider), locator))
    }
}

#[derive(Debug, Clone)]
/// Struct that retrieves event data for an Ethereum mailbox
pub struct EthereumMailboxIndexer<M>
where
    M: Middleware,
{
    contract: Arc<EthereumMailboxInternal<M>>,
    provider: Arc<M>,
}

impl<M> EthereumMailboxIndexer<M>
where
    M: Middleware + 'static,
{
    /// Create new EthereumMailboxIndexer
    pub fn new(provider: Arc<M>, locator: &ContractLocator) -> Self {
        let contract = Arc::new(EthereumMailboxInternal::new(
            locator.address,
            provider.clone(),
        ));
        Self { contract, provider }
    }

    #[instrument(level = "debug", err, ret, skip(self))]
    async fn get_finalized_block_number(&self) -> ChainResult<u32> {
        Ok(self
            .provider
            .get_finalized_block_number()
            .await
            .map_err(ChainCommunicationError::from_other)?
            .ok_or(ChainCommunicationError::CustomError(BLOCK_ERROR_MSG.into()))?
            .as_u32())
    }
}

#[async_trait]
impl<M> Indexer<HyperlaneMessage> for EthereumMailboxIndexer<M>
where
    M: Middleware + 'static,
{
    async fn get_finalized_block_number(&self) -> ChainResult<u32> {
        self.get_finalized_block_number().await
    }

    /// Note: This call may return duplicates depending on the provider used
    #[instrument(err, skip(self))]
    async fn fetch_logs_in_range(
        &self,
        range: RangeInclusive<u32>,
    ) -> ChainResult<Vec<(Indexed<HyperlaneMessage>, LogMeta)>> {
        let mut events: Vec<(Indexed<HyperlaneMessage>, LogMeta)> = self
            .contract
            .dispatch_filter()
            .from_block(*range.start())
            .to_block(*range.end())
            .query_with_meta()
            .await?
            .into_iter()
            .map(|(event, meta)| {
                (
                    HyperlaneMessage::from(event.message.to_vec()).into(),
                    meta.into(),
                )
            })
            .collect();

        events.sort_by(|a, b| a.0.inner().nonce.cmp(&b.0.inner().nonce));
        Ok(events)
    }

    async fn fetch_logs_by_tx_hash(
        &self,
        tx_hash: H512,
    ) -> ChainResult<Vec<(Indexed<HyperlaneMessage>, LogMeta)>> {
        let logs = fetch_raw_logs_and_log_meta::<DispatchFilter, M>(
            tx_hash,
            self.provider.clone(),
            self.contract.address(),
        )
        .await?
        .into_iter()
        .map(|(log, log_meta)| {
            (
                HyperlaneMessage::from(log.message.to_vec()).into(),
                log_meta,
            )
        })
        .collect();
        Ok(logs)
    }
}

#[async_trait]
impl<M> SequenceAwareIndexer<HyperlaneMessage> for EthereumMailboxIndexer<M>
where
    M: Middleware + 'static,
{
    #[instrument(err, skip(self), ret)]
    async fn latest_sequence_count_and_tip(&self) -> ChainResult<(Option<u32>, u32)> {
        let tip = Indexer::<HyperlaneMessage>::get_finalized_block_number(self).await?;
        let sequence = self.contract.nonce().block(u64::from(tip)).call().await?;
        Ok((Some(sequence), tip))
    }
}

#[async_trait]
impl<M> Indexer<H256> for EthereumMailboxIndexer<M>
where
    M: Middleware + 'static,
{
    async fn get_finalized_block_number(&self) -> ChainResult<u32> {
        self.get_finalized_block_number().await
    }

    /// Note: This call may return duplicates depending on the provider used
    #[instrument(err, skip(self))]
    async fn fetch_logs_in_range(
        &self,
        range: RangeInclusive<u32>,
    ) -> ChainResult<Vec<(Indexed<H256>, LogMeta)>> {
        Ok(self
            .contract
            .process_id_filter()
            .from_block(*range.start())
            .to_block(*range.end())
            .query_with_meta()
            .await?
            .into_iter()
            .map(|(event, meta)| (Indexed::new(H256::from(event.message_id)), meta.into()))
            .collect())
    }
}

#[async_trait]
impl<M> SequenceAwareIndexer<H256> for EthereumMailboxIndexer<M>
where
    M: Middleware + 'static,
{
    async fn latest_sequence_count_and_tip(&self) -> ChainResult<(Option<u32>, u32)> {
        // A blanket implementation for this trait is fine for the EVM.
        // TODO: Consider removing `Indexer` as a supertrait of `SequenceAwareIndexer`
        let tip = Indexer::<H256>::get_finalized_block_number(self).await?;
        Ok((None, tip))
    }
}

pub struct MailboxBuilder {}

#[async_trait]
impl BuildableWithProvider for MailboxBuilder {
    type Output = Box<dyn Mailbox>;
    const NEEDS_SIGNER: bool = true;

    async fn build_with_provider<M: Middleware + 'static>(
        &self,
        provider: M,
        conn: &ConnectionConf,
        locator: &ContractLocator,
    ) -> Self::Output {
        Box::new(EthereumMailbox::new(Arc::new(provider), conn, locator))
    }
}

/// A reference to a Mailbox contract on some Ethereum chain
#[derive(Debug)]
pub struct EthereumMailbox<M>
where
    M: Middleware,
{
    contract: Arc<EthereumMailboxInternal<M>>,
    domain: HyperlaneDomain,
    provider: Arc<M>,
    arbitrum_node_interface: Option<Arc<ArbitrumNodeInterface<M>>>,
    conn: ConnectionConf,
}

impl<M> EthereumMailbox<M>
where
    M: Middleware + 'static,
{
    /// Create a reference to a mailbox at a specific Ethereum address on some
    /// chain
    pub fn new(provider: Arc<M>, conn: &ConnectionConf, locator: &ContractLocator) -> Self {
        // Arbitrum Nitro based chains are a special case for transaction cost estimation.
        // The gas amount that eth_estimateGas returns considers both L1 and L2 gas costs.
        // We use the NodeInterface, found at address(0xC8), to isolate the L2 gas costs.
        // See https://developer.arbitrum.io/arbos/gas#nodeinterfacesol or https://github.com/OffchainLabs/nitro/blob/master/contracts/src/node-interface/NodeInterface.sol#L25
        let arbitrum_node_interface = locator.domain.is_arbitrum_nitro().then(|| {
            Arc::new(ArbitrumNodeInterface::new(
                H160::from_low_u64_be(0xC8),
                provider.clone(),
            ))
        });

        Self {
            contract: Arc::new(EthereumMailboxInternal::new(
                locator.address,
                provider.clone(),
            )),
            domain: locator.domain.clone(),
            provider,
            arbitrum_node_interface,
            conn: conn.clone(),
        }
    }

    /// Returns a ContractCall that processes the provided message.
    async fn process_contract_call(
        &self,
        message: &HyperlaneMessage,
        metadata: &[u8],
        tx_gas_estimate: Option<U256>,
    ) -> ChainResult<ContractCall<M, ()>> {
        let mut tx = self.contract.process(
            metadata.to_vec().into(),
            RawHyperlaneMessage::from(message).to_vec().into(),
        );
        if let Some(gas_estimate) = tx_gas_estimate {
            tx = tx.gas(gas_estimate);
        }
        self.add_gas_overrides(tx).await
    }

    async fn add_gas_overrides<D: Detokenize>(
        &self,
        tx: ContractCall<M, D>,
    ) -> ChainResult<ContractCall<M, D>> {
        fill_tx_gas_params(
            tx,
            self.provider.clone(),
            &self.conn.transaction_overrides.clone(),
        )
        .await
    }

    async fn simulate_batch(
        &self,
        multicall: &mut Multicall<M>,
        contract_calls: Vec<ContractCall<M, ()>>,
    ) -> ChainResult<BatchSimulation<M>> {
        let batch = multicall::batch::<_, ()>(multicall, contract_calls.clone()).await?;
        let call_results = batch.call().await?;

        let failed_calls = contract_calls
            .iter()
            .zip(call_results.iter())
            .enumerate()
            .filter_map(
                |(index, (_, result))| {
                    if !result.success {
                        Some(index)
                    } else {
                        None
                    }
                },
            )
            .collect_vec();

        // only send a batch if there are at least two successful calls
        let call_count = contract_calls.len();
        let successful_calls = call_count - failed_calls.len();
        if successful_calls >= 2 {
            Ok(BatchSimulation::new(
                Some(self.submittable_batch(batch)),
                failed_calls,
            ))
        } else {
            Ok(BatchSimulation::failed(call_count))
        }
    }

    fn submittable_batch(
        &self,
        call: ContractCall<M, Vec<MulticallResult>>,
    ) -> SubmittableBatch<M> {
        SubmittableBatch {
            call,
            provider: self.provider.clone(),
            transaction_overrides: self.conn.transaction_overrides.clone(),
        }
    }
}

#[derive(new)]
pub struct BatchSimulation<M> {
    pub call: Option<SubmittableBatch<M>>,
    /// Indexes of excluded calls in the batch (because they either failed the simulation
    /// or they were the only successful call)
    pub excluded_call_indexes: Vec<usize>,
}

impl<M> BatchSimulation<M> {
    pub fn failed(ops_count: usize) -> Self {
        Self::new(None, (0..ops_count).collect())
    }
}

impl<M: Middleware + 'static> BatchSimulation<M> {
    pub async fn try_submit(self) -> ChainResult<BatchResult> {
        if let Some(submittable_batch) = self.call {
            let batch_outcome = submittable_batch.submit().await?;
            Ok(BatchResult::new(
                Some(batch_outcome),
                self.excluded_call_indexes,
            ))
        } else {
            Ok(BatchResult::failed(self.excluded_call_indexes.len()))
        }
    }
}

pub struct SubmittableBatch<M> {
    pub call: ContractCall<M, Vec<MulticallResult>>,
    provider: Arc<M>,
    transaction_overrides: TransactionOverrides,
}

impl<M: Middleware + 'static> SubmittableBatch<M> {
    pub async fn submit(self) -> ChainResult<TxOutcome> {
        let call_with_gas_overrides = fill_tx_gas_params(
            self.call,
            self.provider.clone(),
            &self.transaction_overrides,
        )
        .await?;
        let outcome = report_tx(call_with_gas_overrides).await?;
        ensure_block_finalized(self.provider, outcome.block_number.unwrap()).await?;

        Ok(outcome.into())
    }
}

impl<M> HyperlaneChain for EthereumMailbox<M>
where
    M: Middleware + 'static,
{
    fn domain(&self) -> &HyperlaneDomain {
        &self.domain
    }

    fn provider(&self) -> Box<dyn HyperlaneProvider> {
        Box::new(EthereumProvider::new(
            self.provider.clone(),
            self.domain.clone(),
        ))
    }
}

impl<M> HyperlaneContract for EthereumMailbox<M>
where
    M: Middleware + 'static,
{
    fn address(&self) -> H256 {
        self.contract.address().into()
    }
}

#[async_trait]
impl<M> Mailbox for EthereumMailbox<M>
where
    M: Middleware + 'static,
{
    #[instrument(skip(self))]
    async fn count(&self, _maybe_lag: Option<NonZeroU64>) -> ChainResult<u32> {
        let call = self.contract.nonce().block(BlockNumber::Finalized);
        let nonce = call.call().await?;
        Ok(nonce)
    }

    #[instrument(skip(self))]
    async fn delivered(&self, id: H256) -> ChainResult<bool> {
        Ok(self.contract.delivered(id.into()).call().await?)
    }

    #[instrument(skip(self))]
    async fn default_ism(&self) -> ChainResult<H256> {
        Ok(self.contract.default_ism().call().await?.into())
    }

    #[instrument(skip(self))]
    async fn recipient_ism(&self, recipient: H256) -> ChainResult<H256> {
        Ok(self
            .contract
            .recipient_ism(recipient.into())
            .call()
            .await?
            .into())
    }

    #[instrument(skip(self), fields(metadata=%bytes_to_hex(metadata)))]
    async fn process(
        &self,
        message: &HyperlaneMessage,
        metadata: &[u8],
        tx_gas_limit: Option<U256>,
    ) -> ChainResult<TxOutcome> {
        let contract_call = self
            .process_contract_call(message, metadata, tx_gas_limit)
            .await?;
        let receipt = report_tx(contract_call).await?;
        ensure_block_finalized(self.provider.clone(), receipt.block_number.unwrap()).await?;

        Ok(receipt.into())
    }

    #[instrument(skip(self, ops), fields(size=%ops.len()))]
    async fn try_process_batch<'a>(
        &self,
        ops: Vec<&'a QueueOperation>,
    ) -> ChainResult<BatchResult> {
        let messages = ops
            .iter()
            .map(|op| op.try_batch())
            .collect::<ChainResult<Vec<BatchItem<HyperlaneMessage>>>>()?;
        let mut multicall = build_multicall(self.provider.clone(), &self.conn, self.domain.clone())
            .await
            .map_err(|e| HyperlaneEthereumError::MulticallError(e.to_string()))?;
        let contract_call_futures = messages
            .iter()
            .map(|batch_item| async {
                self.process_contract_call(
                    &batch_item.data,
                    &batch_item.submission_data.metadata,
                    Some(batch_item.submission_data.gas_limit),
                )
                .await
            })
            .collect::<Vec<_>>();
        let contract_calls = join_all(contract_call_futures)
            .await
            .into_iter()
            .collect::<ChainResult<Vec<_>>>()?;

        let batch_simulation = self.simulate_batch(&mut multicall, contract_calls).await?;
        batch_simulation.try_submit().await
    }

    #[instrument(skip(self), fields(msg=%message, metadata=%bytes_to_hex(metadata)))]
    async fn process_estimate_costs(
        &self,
        message: &HyperlaneMessage,
        metadata: &[u8],
    ) -> ChainResult<TxCostEstimate> {
        let contract_call = self.process_contract_call(message, metadata, None).await?;
        let gas_limit = contract_call
            .tx
            .gas()
            .copied()
            .ok_or(HyperlaneProtocolError::ProcessGasLimitRequired)?;

        // If we have a ArbitrumNodeInterface, we need to set the l2_gas_limit.
        let l2_gas_limit = if let Some(arbitrum_node_interface) = &self.arbitrum_node_interface {
            Some(
                arbitrum_node_interface
                    .estimate_retryable_ticket(
                        H160::zero().into(),
                        // Give the sender a deposit, otherwise it reverts
                        U256::MAX.into(),
                        self.contract.address(),
                        U256::zero().into(),
                        H160::zero().into(),
                        H160::zero().into(),
                        contract_call.calldata().unwrap_or_default(),
                    )
                    .estimate_gas()
                    .await?,
            )
        } else {
            None
        };

        let gas_price: U256 = self
            .provider
            .get_gas_price()
            .await
            .map_err(ChainCommunicationError::from_other)?
            .into();

        Ok(TxCostEstimate {
            gas_limit: gas_limit.into(),
            gas_price: gas_price.try_into()?,
            l2_gas_limit: l2_gas_limit.map(|v| v.into()),
        })
    }

    fn process_calldata(&self, message: &HyperlaneMessage, metadata: &[u8]) -> Vec<u8> {
        let process_call = ProcessCall {
            message: RawHyperlaneMessage::from(message).to_vec().into(),
            metadata: metadata.to_vec().into(),
        };

        AbiEncode::encode(process_call)
    }
}

pub struct EthereumMailboxAbi;

impl HyperlaneAbi for EthereumMailboxAbi {
    const SELECTOR_SIZE_BYTES: usize = 4;

    fn fn_map() -> HashMap<Vec<u8>, &'static str> {
        crate::extract_fn_map(&IMAILBOX_ABI)
    }
}

#[cfg(test)]
mod test {
    use std::{str::FromStr, sync::Arc};

    use ethers::{
        providers::{MockProvider, Provider},
        types::{Block, Transaction, U256 as EthersU256},
    };

    use hyperlane_core::{
        ContractLocator, HyperlaneDomain, HyperlaneMessage, KnownHyperlaneDomain, Mailbox,
        TxCostEstimate, H160, H256, U256,
    };
    use hyperlane_ethereum::{ConnectionConf, RpcConnectionConf};

    use crate::contracts::EthereumMailbox;

    /// An amount of gas to add to the estimated gas
    const GAS_ESTIMATE_BUFFER: u32 = 75_000;

    #[tokio::test]
    async fn test_process_estimate_costs_sets_l2_gas_limit_for_arbitrum() {
        let mock_provider = Arc::new(MockProvider::new());
        let provider = Arc::new(Provider::new(mock_provider.clone()));
        let connection_conf = ConnectionConf {
            rpc_connection: RpcConnectionConf::Http {
                url: "http://127.0.0.1:8545".parse().unwrap(),
            },
            transaction_overrides: Default::default(),
            operation_batch: Default::default(),
        };

        let mailbox = EthereumMailbox::new(
            provider.clone(),
            &connection_conf,
            &ContractLocator {
                // An Arbitrum Nitro chain
                domain: &HyperlaneDomain::Known(KnownHyperlaneDomain::PlumeTestnet),
                // Address doesn't matter because we're using a MockProvider
                address: H256::default(),
            },
        );

        let message = HyperlaneMessage::default();
        let metadata: Vec<u8> = vec![];

        assert!(mailbox.arbitrum_node_interface.is_some());
        // Confirm `H160::from_low_u64_ne(0xC8)` does what's expected
        assert_eq!(
            H160::from(mailbox.arbitrum_node_interface.as_ref().unwrap().address()),
            H160::from_str("0x00000000000000000000000000000000000000C8").unwrap(),
        );

        // The MockProvider responses we push are processed in LIFO
        // order, so we start with the final RPCs and work toward the first
        // RPCs

        // RPC 4: eth_gasPrice by process_estimate_costs
        // Return 15 gwei
        let gas_price: U256 =
            EthersU256::from(ethers::utils::parse_units("15", "gwei").unwrap()).into();
        mock_provider.push(gas_price).unwrap();

        // RPC 3: eth_estimateGas to the ArbitrumNodeInterface's estimateRetryableTicket function by process_estimate_costs
        let l2_gas_limit = U256::from(200000); // 200k gas
        mock_provider.push(l2_gas_limit).unwrap();

        // RPC 2: eth_getBlockByNumber from the estimate_eip1559_fees call in process_contract_call
        mock_provider.push(Block::<Transaction>::default()).unwrap();

        // RPC 1: eth_estimateGas from the estimate_gas call in process_contract_call
        // Return 1M gas
        let gas_limit = U256::from(1000000u32);
        mock_provider.push(gas_limit).unwrap();

        let tx_cost_estimate = mailbox
            .process_estimate_costs(&message, &metadata)
            .await
            .unwrap();

        // The TxCostEstimat's gas limit includes the buffer
        let estimated_gas_limit = gas_limit.saturating_add(GAS_ESTIMATE_BUFFER.into());

        assert_eq!(
            tx_cost_estimate,
            TxCostEstimate {
                gas_limit: estimated_gas_limit,
                gas_price: gas_price.try_into().unwrap(),
                l2_gas_limit: Some(l2_gas_limit),
            },
        );
    }
}
