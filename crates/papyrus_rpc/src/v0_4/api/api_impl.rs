use std::sync::Arc;

use async_trait::async_trait;
use jsonrpsee::core::RpcResult;
use jsonrpsee::types::ErrorObjectOwned;
use jsonrpsee::RpcModule;
use lazy_static::lazy_static;
use papyrus_common::pending_classes::{PendingClasses, PendingClassesTrait};
use papyrus_execution::{
    estimate_fee as exec_estimate_fee,
    execute_call,
    simulate_transactions as exec_simulate_transactions,
    ExecutionConfigByBlock,
    ExecutionError,
};
use papyrus_storage::body::events::{EventIndex, EventsReader};
use papyrus_storage::body::{BodyStorageReader, TransactionIndex};
use papyrus_storage::compiled_class::CasmStorageReader;
use papyrus_storage::state::StateStorageReader;
use papyrus_storage::{StorageError, StorageReader};
use starknet_api::block::{BlockHash, BlockNumber, BlockStatus};
use starknet_api::core::{
    ChainId,
    ClassHash,
    ContractAddress,
    EntryPointSelector,
    GlobalRoot,
    Nonce,
};
use starknet_api::hash::{StarkFelt, StarkHash, GENESIS_HASH};
use starknet_api::state::{StateNumber, StorageKey};
use starknet_api::transaction::{
    Calldata,
    EventContent,
    EventIndexInTransactionOutput,
    Transaction as StarknetApiTransaction,
    TransactionHash,
    TransactionOffsetInBlock,
};
use starknet_client::reader::objects::pending_data::{
    PendingBlock,
    PendingStateUpdate as ClientPendingStateUpdate,
};
use starknet_client::reader::{DeployedContract, PendingData, StorageEntry};
use starknet_client::writer::{StarknetWriter, WriterClientError};
use starknet_client::ClientError;
use tokio::sync::RwLock;
use tracing::{debug, instrument, trace, warn};

use super::super::block::{
    get_block_header_by_number,
    get_block_number,
    Block,
    BlockHeader,
    GeneralBlockHeader,
    PendingBlockHeader,
};
use super::super::broadcasted_transaction::{
    BroadcastedDeclareTransaction,
    BroadcastedTransaction,
};
use super::super::error::{
    JsonRpcError,
    BLOCK_NOT_FOUND,
    CLASS_HASH_NOT_FOUND,
    CONTRACT_NOT_FOUND,
    INVALID_BLOCK_HASH,
    INVALID_TRANSACTION_HASH,
    INVALID_TRANSACTION_INDEX,
    NO_BLOCKS,
    PAGE_SIZE_TOO_BIG,
    TOO_MANY_KEYS_IN_FILTER,
    TRANSACTION_HASH_NOT_FOUND,
};
use super::super::execution::TransactionTrace;
use super::super::state::{AcceptedStateUpdate, PendingStateUpdate, StateUpdate};
use super::super::transaction::{
    get_block_tx_hashes_by_number,
    get_block_txs_by_number,
    DeployAccountTransaction,
    Event,
    GeneralTransactionReceipt,
    InvokeTransactionV1,
    PendingTransactionFinalityStatus,
    PendingTransactionOutput,
    PendingTransactionReceipt,
    Transaction,
    TransactionOutput,
    TransactionReceipt,
    TransactionWithHash,
    Transactions,
};
use super::super::write_api_error::{
    starknet_error_to_declare_error,
    starknet_error_to_deploy_account_error,
    starknet_error_to_invoke_error,
};
use super::super::write_api_result::{
    AddDeclareOkResult,
    AddDeployAccountOkResult,
    AddInvokeOkResult,
};
use super::{
    stored_txn_to_executable_txn,
    BlockHashAndNumber,
    BlockId,
    ContinuationToken,
    EventFilter,
    EventsChunk,
    FeeEstimate,
    GatewayContractClass,
    JsonRpcV0_4Server,
    SimulatedTransaction,
    SimulationFlag,
    TransactionTraceWithHash,
};
use crate::api::{BlockHashOrNumber, JsonRpcServerImpl, Tag};
use crate::syncing_state::{get_last_synced_block, SyncStatus, SyncingState};
use crate::{
    get_block_status,
    get_latest_block_number,
    internal_server_error,
    ContinuationTokenAsStruct,
};

// TODO(yael): implement address 0x1 as a const function in starknet_api.
lazy_static! {
    pub static ref BLOCK_HASH_TABLE_ADDRESS: ContractAddress = ContractAddress::from(1_u8);
}

/// Rpc server.
pub struct JsonRpcServerV0_4Impl {
    pub chain_id: ChainId,
    pub execution_config: ExecutionConfigByBlock,
    pub storage_reader: StorageReader,
    pub max_events_chunk_size: usize,
    pub max_events_keys: usize,
    pub starting_block: BlockHashAndNumber,
    pub shared_highest_block: Arc<RwLock<Option<BlockHashAndNumber>>>,
    pub pending_data: Arc<RwLock<PendingData>>,
    pub pending_classes: Arc<RwLock<PendingClasses>>,
    pub writer_client: Arc<dyn StarknetWriter>,
}

#[async_trait]
impl JsonRpcV0_4Server for JsonRpcServerV0_4Impl {
    #[instrument(skip(self), level = "debug", err, ret)]
    fn block_number(&self) -> RpcResult<BlockNumber> {
        let txn = self.storage_reader.begin_ro_txn().map_err(internal_server_error)?;
        get_latest_block_number(&txn)?.ok_or_else(|| ErrorObjectOwned::from(NO_BLOCKS))
    }

    #[instrument(skip(self), level = "debug", err, ret)]
    fn block_hash_and_number(&self) -> RpcResult<BlockHashAndNumber> {
        let txn = self.storage_reader.begin_ro_txn().map_err(internal_server_error)?;
        let block_number =
            get_latest_block_number(&txn)?.ok_or_else(|| ErrorObjectOwned::from(NO_BLOCKS))?;
        let header: BlockHeader = get_block_header_by_number(&txn, block_number)?;

        Ok(BlockHashAndNumber { block_hash: header.block_hash, block_number })
    }

    #[instrument(skip(self), level = "debug", err, ret)]
    async fn get_block_w_transaction_hashes(&self, block_id: BlockId) -> RpcResult<Block> {
        if let BlockId::Tag(Tag::Pending) = block_id {
            let block = read_pending_data(&self.pending_data, &self.storage_reader).await?.block;
            let pending_block_header = PendingBlockHeader {
                parent_hash: block.parent_block_hash,
                sequencer_address: block.sequencer_address,
                timestamp: block.timestamp,
            };
            let header = GeneralBlockHeader::PendingBlockHeader(pending_block_header);
            let client_transactions = block.transactions;
            let transaction_hashes = client_transactions
                .iter()
                .map(|transaction| transaction.transaction_hash())
                .collect();
            return Ok(Block {
                status: None,
                header,
                transactions: Transactions::Hashes(transaction_hashes),
            });
        }

        let txn = self.storage_reader.begin_ro_txn().map_err(internal_server_error)?;
        let block_number = get_block_number(&txn, block_id)?;
        let status = get_block_status(&txn, block_number)?;
        let header =
            GeneralBlockHeader::BlockHeader(get_block_header_by_number(&txn, block_number)?);
        let transaction_hashes = get_block_tx_hashes_by_number(&txn, block_number)?;

        Ok(Block {
            status: Some(status),
            header,
            transactions: Transactions::Hashes(transaction_hashes),
        })
    }

    #[instrument(skip(self), level = "debug", err, ret)]
    async fn get_block_w_full_transactions(&self, block_id: BlockId) -> RpcResult<Block> {
        if let BlockId::Tag(Tag::Pending) = block_id {
            let block = read_pending_data(&self.pending_data, &self.storage_reader).await?.block;
            let pending_block_header = PendingBlockHeader {
                parent_hash: block.parent_block_hash,
                sequencer_address: block.sequencer_address,
                timestamp: block.timestamp,
            };
            let header = GeneralBlockHeader::PendingBlockHeader(pending_block_header);
            let client_transactions = block.transactions;
            let transactions = client_transactions
                .iter()
                .map(|client_transaction| {
                    let starknet_api_transaction: StarknetApiTransaction =
                        client_transaction.clone().try_into().map_err(internal_server_error)?;
                    Ok(TransactionWithHash {
                        transaction: starknet_api_transaction
                            .try_into()
                            .map_err(internal_server_error)?,
                        transaction_hash: client_transaction.transaction_hash(),
                    })
                })
                .collect::<Result<Vec<_>, ErrorObjectOwned>>()?;
            return Ok(Block {
                status: None,
                header,
                transactions: Transactions::Full(transactions),
            });
        }

        let txn = self.storage_reader.begin_ro_txn().map_err(internal_server_error)?;
        let block_number = get_block_number(&txn, block_id)?;
        let status = get_block_status(&txn, block_number)?;
        let header =
            GeneralBlockHeader::BlockHeader(get_block_header_by_number(&txn, block_number)?);
        // TODO(dvir): consider create a vector of (transaction, transaction_index) first and get
        // the transaction hashes by the index.
        let transactions = get_block_txs_by_number(&txn, block_number)?;
        let transaction_hashes = get_block_tx_hashes_by_number(&txn, block_number)?;
        let transactions_with_hash = transactions
            .into_iter()
            .zip(transaction_hashes)
            .map(|(transaction, transaction_hash)| TransactionWithHash {
                transaction,
                transaction_hash,
            })
            .collect();

        Ok(Block {
            status: Some(status),
            header,
            transactions: Transactions::Full(transactions_with_hash),
        })
    }

    #[instrument(skip(self), level = "debug", err, ret)]
    async fn get_storage_at(
        &self,
        contract_address: ContractAddress,
        key: StorageKey,
        block_id: BlockId,
    ) -> RpcResult<StarkFelt> {
        let block_id = if let BlockId::Tag(Tag::Pending) = block_id {
            let pending_storage_diffs = read_pending_data(&self.pending_data, &self.storage_reader)
                .await?
                .state_update
                .state_diff
                .storage_diffs;
            if let Some(storage_entries) = pending_storage_diffs.get(&contract_address) {
                // iterating in reverse to get the latest value.
                for StorageEntry { key: other_key, value } in storage_entries.iter().rev() {
                    if key == *other_key {
                        return Ok(*value);
                    }
                }
            }
            BlockId::Tag(Tag::Latest)
        } else {
            block_id
        };

        let txn = self.storage_reader.begin_ro_txn().map_err(internal_server_error)?;

        // Check that the block is valid and get the state number.
        let block_number = get_block_number(&txn, block_id)?;
        let state = StateNumber::right_after_block(block_number);
        let state_reader = txn.get_state_reader().map_err(internal_server_error)?;

        let res = state_reader
            .get_storage_at(state, &contract_address, &key)
            .map_err(internal_server_error)?;
        // Contract address 0x1 is a special address, it stores the block
        // hashes. Contracts are not deployed to this address.
        if res == StarkFelt::default() && contract_address != *BLOCK_HASH_TABLE_ADDRESS {
            // check if the contract exists
            state_reader
                .get_class_hash_at(state, &contract_address)
                .map_err(internal_server_error)?
                .ok_or_else(|| ErrorObjectOwned::from(CONTRACT_NOT_FOUND))?;
        }
        Ok(res)
    }

    #[instrument(skip(self), level = "debug", err, ret)]
    async fn get_transaction_by_hash(
        &self,
        transaction_hash: TransactionHash,
    ) -> RpcResult<TransactionWithHash> {
        let txn = self.storage_reader.begin_ro_txn().map_err(internal_server_error)?;

        if let Some(transaction_index) =
            txn.get_transaction_idx_by_hash(&transaction_hash).map_err(internal_server_error)?
        {
            let transaction = txn
                .get_transaction(transaction_index)
                .map_err(internal_server_error)?
                .ok_or_else(|| ErrorObjectOwned::from(TRANSACTION_HASH_NOT_FOUND))?;

            Ok(TransactionWithHash { transaction: transaction.try_into()?, transaction_hash })
        } else {
            // The transaction is not in any non-pending block. Search for it in the pending block
            // and if it's not found, return error.
            let client_transaction = read_pending_data(&self.pending_data, &self.storage_reader)
                .await?
                .block
                .transactions
                .iter()
                .find(|transaction| transaction.transaction_hash() == transaction_hash)
                .ok_or_else(|| ErrorObjectOwned::from(TRANSACTION_HASH_NOT_FOUND))?
                .clone();

            let starknet_api_transaction: StarknetApiTransaction =
                client_transaction.try_into().map_err(internal_server_error)?;
            return Ok(TransactionWithHash {
                transaction: starknet_api_transaction.try_into()?,
                transaction_hash,
            });
        }
    }

    #[instrument(skip(self), level = "debug", err, ret)]
    async fn get_transaction_by_block_id_and_index(
        &self,
        block_id: BlockId,
        index: TransactionOffsetInBlock,
    ) -> RpcResult<TransactionWithHash> {
        let (starknet_api_transaction, transaction_hash) = if let BlockId::Tag(Tag::Pending) =
            block_id
        {
            let client_transaction = read_pending_data(&self.pending_data, &self.storage_reader)
                .await?
                .block
                .transactions
                .get(index.0)
                .ok_or_else(|| ErrorObjectOwned::from(INVALID_TRANSACTION_INDEX))?
                .clone();
            let transaction_hash = client_transaction.transaction_hash();
            (client_transaction.try_into().map_err(internal_server_error)?, transaction_hash)
        } else {
            let txn = self.storage_reader.begin_ro_txn().map_err(internal_server_error)?;
            let block_number = get_block_number(&txn, block_id)?;

            let tx_index = TransactionIndex(block_number, index);
            let transaction = txn
                .get_transaction(tx_index)
                .map_err(internal_server_error)?
                .ok_or_else(|| ErrorObjectOwned::from(INVALID_TRANSACTION_INDEX))?;
            let transaction_hash = txn
                .get_transaction_hash_by_idx(&tx_index)
                .map_err(internal_server_error)?
                .ok_or_else(|| ErrorObjectOwned::from(INVALID_TRANSACTION_INDEX))?;
            (transaction, transaction_hash)
        };

        Ok(TransactionWithHash {
            transaction: starknet_api_transaction.try_into()?,
            transaction_hash,
        })
    }

    #[instrument(skip(self), level = "debug", err, ret)]
    async fn get_block_transaction_count(&self, block_id: BlockId) -> RpcResult<usize> {
        if let BlockId::Tag(Tag::Pending) = block_id {
            let transactions_len = read_pending_data(&self.pending_data, &self.storage_reader)
                .await?
                .block
                .transactions
                .len();
            Ok(transactions_len)
        } else {
            let txn = self.storage_reader.begin_ro_txn().map_err(internal_server_error)?;
            let block_number = get_block_number(&txn, block_id)?;
            let transactions: Vec<Transaction> = get_block_txs_by_number(&txn, block_number)?;
            Ok(transactions.len())
        }
    }

    #[instrument(skip(self), level = "debug", err, ret)]
    async fn get_state_update(&self, block_id: BlockId) -> RpcResult<StateUpdate> {
        if let BlockId::Tag(Tag::Pending) = block_id {
            let state_update =
                read_pending_data(&self.pending_data, &self.storage_reader).await?.state_update;
            return Ok(StateUpdate::PendingStateUpdate(PendingStateUpdate {
                old_root: state_update.old_root,
                state_diff: state_update.state_diff.into(),
            }));
        }
        let txn = self.storage_reader.begin_ro_txn().map_err(internal_server_error)?;

        // Get the block header for the block hash and state root.
        let block_number = get_block_number(&txn, block_id)?;
        let header: BlockHeader = get_block_header_by_number(&txn, block_number)?;

        // Get the old root.
        let old_root = match get_block_number(
            &txn,
            BlockId::HashOrNumber(BlockHashOrNumber::Hash(header.parent_hash)),
        ) {
            Ok(parent_block_number) => {
                get_block_header_by_number::<_, BlockHeader>(&txn, parent_block_number)?.new_root
            }
            Err(_) => GlobalRoot(StarkHash::try_from(GENESIS_HASH).map_err(internal_server_error)?),
        };

        // Get the block state diff.
        let thin_state_diff = txn
            .get_state_diff(block_number)
            .map_err(internal_server_error)?
            .ok_or_else(|| ErrorObjectOwned::from(BLOCK_NOT_FOUND))?;

        Ok(StateUpdate::AcceptedStateUpdate(AcceptedStateUpdate {
            block_hash: header.block_hash,
            new_root: header.new_root,
            old_root,
            state_diff: thin_state_diff.into(),
        }))
    }

    #[instrument(skip(self), level = "debug", err, ret)]
    async fn get_transaction_receipt(
        &self,
        transaction_hash: TransactionHash,
    ) -> RpcResult<GeneralTransactionReceipt> {
        let txn = self.storage_reader.begin_ro_txn().map_err(internal_server_error)?;

        if let Some(transaction_index) =
            txn.get_transaction_idx_by_hash(&transaction_hash).map_err(internal_server_error)?
        {
            let block_number = transaction_index.0;
            let status = get_block_status(&txn, block_number)?;

            // rejected blocks should not be a part of the API so we early return here.
            // this assumption also holds for the conversion from block status to transaction
            // finality status where we set rejected blocks to unreachable.
            if status == BlockStatus::Rejected {
                return Err(ErrorObjectOwned::from(BLOCK_NOT_FOUND))?;
            }

            let block_hash = get_block_header_by_number::<_, BlockHeader>(&txn, block_number)
                .map_err(internal_server_error)?
                .block_hash;

            let thin_tx_output = txn
                .get_transaction_output(transaction_index)
                .map_err(internal_server_error)?
                .ok_or_else(|| ErrorObjectOwned::from(TRANSACTION_HASH_NOT_FOUND))?;

            let events = txn
                .get_transaction_events(transaction_index)
                .map_err(internal_server_error)?
                .ok_or_else(|| ErrorObjectOwned::from(TRANSACTION_HASH_NOT_FOUND))?;

            let output = TransactionOutput::from_thin_transaction_output(thin_tx_output, events);

            Ok(GeneralTransactionReceipt::TransactionReceipt(TransactionReceipt {
                finality_status: status.into(),
                transaction_hash,
                block_hash,
                block_number,
                output,
            }))
        } else {
            // The transaction is not in any non-pending block. Search for it in the pending block
            // and if it's not found, return error.

            // TODO(shahak): Consider cloning the transactions and the receipts in order to free
            // the lock sooner (Check which is better).
            let pending_block =
                read_pending_data(&self.pending_data, &self.storage_reader).await?.block;

            let client_transaction_receipt = pending_block
                .transaction_receipts
                .iter()
                .find(|receipt| receipt.transaction_hash == transaction_hash)
                .ok_or_else(|| ErrorObjectOwned::from(TRANSACTION_HASH_NOT_FOUND))?
                .clone();
            let client_transaction = &pending_block
                .transactions
                .iter()
                .find(|transaction| transaction.transaction_hash() == transaction_hash)
                .ok_or_else(|| ErrorObjectOwned::from(TRANSACTION_HASH_NOT_FOUND))?;
            let starknet_api_output =
                client_transaction_receipt.into_starknet_api_transaction_output(client_transaction);
            let output =
                PendingTransactionOutput::try_from(TransactionOutput::from(starknet_api_output))?;
            Ok(GeneralTransactionReceipt::PendingTransactionReceipt(PendingTransactionReceipt {
                // ACCEPTED_ON_L2 is the only finality status of a pending transaction.
                finality_status: PendingTransactionFinalityStatus::AcceptedOnL2,
                transaction_hash,
                output,
            }))
        }
    }

    #[instrument(skip(self), level = "debug", err, ret)]
    async fn get_class(
        &self,
        block_id: BlockId,
        class_hash: ClassHash,
    ) -> RpcResult<GatewayContractClass> {
        let block_id = if let BlockId::Tag(Tag::Pending) = block_id {
            let maybe_class = &self.pending_classes.read().await.get_class(class_hash);
            if let Some(class) = maybe_class {
                return class.clone().try_into().map_err(internal_server_error);
            } else {
                BlockId::Tag(Tag::Latest)
            }
        } else {
            block_id
        };

        let txn = self.storage_reader.begin_ro_txn().map_err(internal_server_error)?;

        let block_number = get_block_number(&txn, block_id)?;
        let state_number = StateNumber::right_after_block(block_number);
        let state_reader = txn.get_state_reader().map_err(internal_server_error)?;

        // The class might be a deprecated class. Search it first in the declared classes and if not
        // found, search in the deprecated classes.
        if let Some(class) = state_reader
            .get_class_definition_at(state_number, &class_hash)
            .map_err(internal_server_error)?
        {
            Ok(GatewayContractClass::Sierra(class.try_into().map_err(internal_server_error)?))
        } else {
            let class = state_reader
                .get_deprecated_class_definition_at(state_number, &class_hash)
                .map_err(internal_server_error)?
                .ok_or_else(|| ErrorObjectOwned::from(CLASS_HASH_NOT_FOUND))?;
            Ok(GatewayContractClass::Cairo0(class.try_into().map_err(internal_server_error)?))
        }
    }

    #[instrument(skip(self), level = "debug", err, ret)]
    async fn get_class_at(
        &self,
        block_id: BlockId,
        contract_address: ContractAddress,
    ) -> RpcResult<GatewayContractClass> {
        let class_hash = self.get_class_hash_at(block_id, contract_address).await?;
        self.get_class(block_id, class_hash).await
    }

    #[instrument(skip(self), level = "debug", err, ret)]
    async fn get_class_hash_at(
        &self,
        block_id: BlockId,
        contract_address: ContractAddress,
    ) -> RpcResult<ClassHash> {
        let block_id = if let BlockId::Tag(Tag::Pending) = block_id {
            let pending_deployed_contracts =
                read_pending_data(&self.pending_data, &self.storage_reader)
                    .await?
                    .state_update
                    .state_diff
                    .deployed_contracts;
            for DeployedContract { address, class_hash } in pending_deployed_contracts {
                if address == contract_address {
                    return Ok(class_hash);
                }
            }
            BlockId::Tag(Tag::Latest)
        } else {
            block_id
        };

        let txn = self.storage_reader.begin_ro_txn().map_err(internal_server_error)?;

        let block_number = get_block_number(&txn, block_id)?;
        let state = StateNumber::right_after_block(block_number);
        let state_reader = txn.get_state_reader().map_err(internal_server_error)?;

        state_reader
            .get_class_hash_at(state, &contract_address)
            .map_err(internal_server_error)?
            .ok_or_else(|| ErrorObjectOwned::from(CONTRACT_NOT_FOUND))
    }

    #[instrument(skip(self), level = "debug", err, ret)]
    async fn get_nonce(
        &self,
        block_id: BlockId,
        contract_address: ContractAddress,
    ) -> RpcResult<Nonce> {
        let block_id = if let BlockId::Tag(Tag::Pending) = block_id {
            let pending_nonces = read_pending_data(&self.pending_data, &self.storage_reader)
                .await?
                .state_update
                .state_diff
                .nonces;
            match pending_nonces.get(&contract_address) {
                Some(nonce) => return Ok(*nonce),
                None => BlockId::Tag(Tag::Latest),
            }
        } else {
            block_id
        };

        let txn = self.storage_reader.begin_ro_txn().map_err(internal_server_error)?;

        let block_number = get_block_number(&txn, block_id)?;
        let state = StateNumber::right_after_block(block_number);
        let state_reader = txn.get_state_reader().map_err(internal_server_error)?;

        state_reader
            .get_nonce_at(state, &contract_address)
            .map_err(internal_server_error)?
            .ok_or_else(|| ErrorObjectOwned::from(CONTRACT_NOT_FOUND))
    }

    #[instrument(skip(self), level = "debug", err, ret)]
    fn chain_id(&self) -> RpcResult<String> {
        Ok(self.chain_id.as_hex())
    }

    #[instrument(skip(self), level = "debug", err, ret)]
    async fn get_events(&self, filter: EventFilter) -> RpcResult<EventsChunk> {
        // Check the chunk size.
        if filter.chunk_size > self.max_events_chunk_size {
            return Err(ErrorObjectOwned::from(PAGE_SIZE_TOO_BIG));
        }
        // Check the number of keys.
        if filter.keys.len() > self.max_events_keys {
            return Err(ErrorObjectOwned::from(TOO_MANY_KEYS_IN_FILTER));
        }

        // Get the requested block numbers.
        let txn = self.storage_reader.begin_ro_txn().map_err(internal_server_error)?;
        let Some(latest_block_number) = get_latest_block_number(&txn)? else {
            if matches!(filter.to_block, Some(BlockId::Tag(Tag::Pending)) | None) {
                warn!(
                    "Received a request for pending events while there are no accepted blocks. \
                     This is currently unsupported. Returning no events."
                );
            }
            // There are no blocks.
            return Ok(EventsChunk { events: vec![], continuation_token: None });
        };
        let from_block_number = match filter.from_block {
            None => BlockNumber(0),
            Some(BlockId::Tag(Tag::Pending)) => latest_block_number.next(),
            Some(block_id) => get_block_number(&txn, block_id)?,
        };
        let mut to_block_number = match filter.to_block {
            Some(BlockId::Tag(Tag::Pending)) | None => latest_block_number.next(),
            Some(block_id) => get_block_number(&txn, block_id)?,
        };

        if from_block_number > to_block_number {
            return Ok(EventsChunk { events: vec![], continuation_token: None });
        }

        // Get the event index. If there's a continuation token we take the event index from there.
        // Otherwise, we take the first index in the from_block_number.
        let start_event_index = match &filter.continuation_token {
            Some(token) => token.parse()?.0,
            None => EventIndex(
                TransactionIndex(from_block_number, TransactionOffsetInBlock(0)),
                EventIndexInTransactionOutput(0),
            ),
        };

        let include_pending_block = to_block_number > latest_block_number;
        if include_pending_block {
            to_block_number = to_block_number.prev().expect(
                "A block number that's greater than another block number should have a predecessor",
            );
        }

        // Collect the requested events.
        // Once we collected enough events, we continue to check if there are any more events
        // corresponding to the requested filter. If there are, we return a continuation token
        // pointing to the next relevant event. Otherwise, we return a continuation token None.
        let mut filtered_events = vec![];
        if start_event_index.0.0 <= latest_block_number {
            for ((from_address, event_index), content) in txn
                .iter_events(filter.address, start_event_index, to_block_number)
                .map_err(internal_server_error)?
            {
                let block_number = (event_index.0).0;
                if block_number > to_block_number {
                    break;
                }
                if let Some(filter_address) = filter.address {
                    if from_address != filter_address {
                        // The iterator of this loop outputs only events that have the filter's
                        // address, unless there are no more such events and then it outputs other
                        // events, and we can stop the iteration.
                        break;
                    }
                }
                // TODO: Consider changing empty sets in the filer keys to None.
                if do_event_keys_match_filter(&content, &filter) {
                    if filtered_events.len() == filter.chunk_size {
                        return Ok(EventsChunk {
                            events: filtered_events,
                            continuation_token: Some(ContinuationToken::new(
                                ContinuationTokenAsStruct(event_index),
                            )?),
                        });
                    }
                    let header: BlockHeader = get_block_header_by_number(&txn, block_number)
                        .map_err(internal_server_error)?;
                    let transaction_hash = txn
                        .get_transaction_hash_by_idx(&event_index.0)
                        .map_err(internal_server_error)?
                        .ok_or_else(|| internal_server_error("Unknown internal error."))?;
                    let emitted_event = Event {
                        block_hash: Some(header.block_hash),
                        block_number: Some(block_number),
                        transaction_hash,
                        event: starknet_api::transaction::Event { from_address, content },
                    };
                    filtered_events.push(emitted_event);
                }
            }
        }

        if include_pending_block {
            let pending_transaction_receipts =
                read_pending_data(&self.pending_data, &self.storage_reader)
                    .await?
                    .block
                    .transaction_receipts;
            // Extract the first transaction offset and event offset from the starting EventIndex.
            let (transaction_start, event_start) = if start_event_index.0.0 > latest_block_number {
                (start_event_index.0.1.0, start_event_index.1.0)
            } else {
                (0, 0)
            };
            // TODO(shahak): Consider creating the iterator flattened and filtered.
            for (transaction_offset, receipt) in pending_transaction_receipts.iter().enumerate() {
                if transaction_offset < transaction_start {
                    continue;
                }
                for (event_offset, event) in receipt.events.iter().cloned().enumerate() {
                    if transaction_offset == transaction_start && event_offset < event_start {
                        continue;
                    }
                    if filtered_events.len() == filter.chunk_size {
                        return Ok(EventsChunk {
                            events: filtered_events,
                            continuation_token: Some(ContinuationToken::new(
                                ContinuationTokenAsStruct(EventIndex(
                                    TransactionIndex(
                                        latest_block_number.next(),
                                        TransactionOffsetInBlock(transaction_offset),
                                    ),
                                    EventIndexInTransactionOutput(event_offset),
                                )),
                            )?),
                        });
                    }
                    if !do_event_keys_match_filter(&event.content, &filter) {
                        continue;
                    }
                    if let Some(filter_address) = filter.address {
                        if event.from_address != filter_address {
                            continue;
                        }
                    }
                    filtered_events.push(Event {
                        block_hash: None,
                        block_number: None,
                        transaction_hash: receipt.transaction_hash,
                        event,
                    })
                }
            }
        }

        Ok(EventsChunk { events: filtered_events, continuation_token: None })
    }

    #[instrument(skip(self), level = "debug", err, ret)]
    async fn syncing(&self) -> RpcResult<SyncingState> {
        let Some(highest_block) = *self.shared_highest_block.read().await else {
            return Ok(SyncingState::Synced);
        };
        let current_block =
            get_last_synced_block(self.storage_reader.clone()).map_err(internal_server_error)?;
        if highest_block.block_number <= current_block.block_number {
            return Ok(SyncingState::Synced);
        }
        Ok(SyncingState::SyncStatus(SyncStatus {
            starting_block_hash: self.starting_block.block_hash,
            starting_block_num: self.starting_block.block_number,
            current_block_hash: current_block.block_hash,
            current_block_num: current_block.block_number,
            highest_block_hash: highest_block.block_hash,
            highest_block_num: highest_block.block_number,
        }))
    }

    #[instrument(skip(self), level = "debug", err, ret)]
    async fn call(
        &self,
        contract_address: ContractAddress,
        entry_point_selector: EntryPointSelector,
        calldata: Calldata,
        block_id: BlockId,
    ) -> RpcResult<Vec<StarkFelt>> {
        let block_number = get_block_number(
            &self.storage_reader.begin_ro_txn().map_err(internal_server_error)?,
            block_id,
        )?;
        let state_number = StateNumber::right_after_block(block_number);
        let block_execution_config = self
            .execution_config
            .get_execution_config_for_block(block_number)
            .map_err(|err| {
                internal_server_error(format!("Failed to get execution config: {}", err))
            })?
            .clone();
        let chain_id = self.chain_id.clone();
        let reader = self.storage_reader.clone();
        let contract_address_copy = contract_address;

        let call_result = tokio::task::spawn_blocking(move || {
            execute_call(
                reader,
                // TODO(shahak): Add pending data here.
                None,
                None,
                &chain_id,
                state_number,
                block_number,
                &contract_address_copy,
                entry_point_selector,
                calldata,
                &block_execution_config,
            )
        })
        .await
        .map_err(internal_server_error)?;

        match call_result {
            Ok(res) => Ok(res.retdata.0),
            Err(ExecutionError::StorageError(err)) => Err(internal_server_error(err)),
            Err(err) => Err(ErrorObjectOwned::from(JsonRpcError::try_from(err)?)),
        }
    }

    #[instrument(skip(self), level = "debug", err, ret)]
    async fn add_invoke_transaction(
        &self,
        invoke_transaction: InvokeTransactionV1,
    ) -> RpcResult<AddInvokeOkResult> {
        let result = self.writer_client.add_invoke_transaction(&invoke_transaction.into()).await;
        match result {
            Ok(res) => Ok(res.into()),
            Err(WriterClientError::ClientError(ClientError::StarknetError(starknet_error))) => {
                Err(ErrorObjectOwned::from(starknet_error_to_invoke_error(starknet_error)))
            }
            Err(err) => Err(internal_server_error(err)),
        }
    }

    #[instrument(skip(self), level = "debug", err, ret)]
    async fn add_deploy_account_transaction(
        &self,
        deploy_account_transaction: DeployAccountTransaction,
    ) -> RpcResult<AddDeployAccountOkResult> {
        let result = self
            .writer_client
            .add_deploy_account_transaction(&deploy_account_transaction.into())
            .await;
        match result {
            Ok(res) => Ok(res.into()),
            Err(WriterClientError::ClientError(ClientError::StarknetError(starknet_error))) => {
                Err(ErrorObjectOwned::from(starknet_error_to_deploy_account_error(starknet_error)))
            }
            Err(err) => Err(internal_server_error(err)),
        }
    }

    #[instrument(skip(self), level = "debug", err, ret)]
    async fn add_declare_transaction(
        &self,
        declare_transaction: BroadcastedDeclareTransaction,
    ) -> RpcResult<AddDeclareOkResult> {
        let result = self
            .writer_client
            .add_declare_transaction(
                &declare_transaction.try_into().map_err(internal_server_error)?,
            )
            .await;
        match result {
            Ok(res) => Ok(res.into()),
            Err(WriterClientError::ClientError(ClientError::StarknetError(starknet_error))) => {
                Err(ErrorObjectOwned::from(starknet_error_to_declare_error(starknet_error)))
            }
            Err(err) => Err(internal_server_error(err)),
        }
    }

    #[instrument(skip(self, transactions), level = "debug", err, ret)]
    async fn estimate_fee(
        &self,
        transactions: Vec<BroadcastedTransaction>,
        block_id: BlockId,
    ) -> RpcResult<Vec<FeeEstimate>> {
        trace!("Estimating fee of transactions: {:#?}", transactions);
        let executable_txns =
            transactions.into_iter().map(|tx| tx.try_into()).collect::<Result<_, _>>()?;

        let block_number = get_block_number(
            &self.storage_reader.begin_ro_txn().map_err(internal_server_error)?,
            block_id,
        )?;
        let state_number = StateNumber::right_after_block(block_number);
        let block_execution_config = self
            .execution_config
            .get_execution_config_for_block(block_number)
            .map_err(|err| {
                internal_server_error(format!("Failed to get execution config: {}", err))
            })?
            .clone();
        let chain_id = self.chain_id.clone();
        let reader = self.storage_reader.clone();

        let estimate_fee_result = tokio::task::spawn_blocking(move || {
            exec_estimate_fee(
                executable_txns,
                &chain_id,
                reader,
                // TODO(shahak): Add pending data here.
                None,
                None,
                state_number,
                block_number,
                &block_execution_config,
            )
        })
        .await
        .map_err(internal_server_error)?;

        match estimate_fee_result {
            Ok(fees) => Ok(fees
                .into_iter()
                .map(|(gas_price, fee)| FeeEstimate::from(gas_price, fee))
                .collect()),
            Err(ExecutionError::StorageError(err)) => Err(internal_server_error(err)),
            Err(err) => Err(ErrorObjectOwned::from(JsonRpcError::try_from(err)?)),
        }
    }

    #[instrument(skip(self, transactions), level = "debug", err, ret)]
    async fn simulate_transactions(
        &self,
        block_id: BlockId,
        transactions: Vec<BroadcastedTransaction>,
        simulation_flags: Vec<SimulationFlag>,
    ) -> RpcResult<Vec<SimulatedTransaction>> {
        trace!("Simulating transactions: {:#?}", transactions);
        let executable_txns =
            transactions.into_iter().map(|tx| tx.try_into()).collect::<Result<_, _>>()?;

        let block_number = get_block_number(
            &self.storage_reader.begin_ro_txn().map_err(internal_server_error)?,
            block_id,
        )?;
        let state_number = StateNumber::right_after_block(block_number);
        let block_execution_config = self
            .execution_config
            .get_execution_config_for_block(block_number)
            .map_err(|err| {
                internal_server_error(format!("Failed to get execution config: {}", err))
            })?
            .clone();
        let chain_id = self.chain_id.clone();
        let reader = self.storage_reader.clone();

        let charge_fee = !simulation_flags.contains(&SimulationFlag::SkipFeeCharge);
        let validate = !simulation_flags.contains(&SimulationFlag::SkipValidate);

        let simulate_transactions_result = tokio::task::spawn_blocking(move || {
            exec_simulate_transactions(
                executable_txns,
                None,
                &chain_id,
                reader,
                // TODO(shahak): Add pending data here.
                None,
                None,
                state_number,
                block_number,
                &block_execution_config,
                charge_fee,
                validate,
            )
        })
        .await
        .map_err(internal_server_error)?;

        match simulate_transactions_result {
            Ok(simulation_results) => Ok(simulation_results
                .into_iter()
                .map(|(transaction_trace, _, gas_price, fee)| SimulatedTransaction {
                    transaction_trace: transaction_trace.into(),
                    fee_estimation: FeeEstimate::from(gas_price, fee),
                })
                .collect()),
            Err(ExecutionError::StorageError(err)) => Err(internal_server_error(err)),
            Err(err) => Err(ErrorObjectOwned::from(JsonRpcError::try_from(err)?)),
        }
    }

    #[instrument(skip(self), level = "debug", err)]
    async fn trace_transaction(
        &self,
        transaction_hash: TransactionHash,
    ) -> RpcResult<TransactionTrace> {
        let storage_txn = self.storage_reader.begin_ro_txn().map_err(internal_server_error)?;
        let TransactionIndex(block_number, tx_offset) = storage_txn
            .get_transaction_idx_by_hash(&transaction_hash)
            .map_err(internal_server_error)?
            .ok_or(INVALID_TRANSACTION_HASH)?;

        let casm_marker = storage_txn.get_compiled_class_marker().map_err(internal_server_error)?;
        if casm_marker <= block_number {
            debug!(
                ?transaction_hash,
                ?block_number,
                ?casm_marker,
                "Transaction is in the storage, but the compiled classes are not fully synced up \
                 to its block.",
            );
            return Err(INVALID_TRANSACTION_HASH.into());
        }

        let block_transactions = storage_txn
            .get_block_transactions(block_number)
            .map_err(internal_server_error)?
            .ok_or_else(|| {
                internal_server_error(StorageError::DBInconsistency {
                    msg: format!("Missing block {block_number} transactions"),
                })
            })?;

        let tx_hashes = storage_txn
            .get_block_transaction_hashes(block_number)
            .map_err(internal_server_error)?
            .ok_or_else(|| {
                internal_server_error(StorageError::DBInconsistency {
                    msg: format!("Missing block {block_number} transactions"),
                })
            })?;

        let state_number = StateNumber::right_before_block(block_number);
        let executable_txns = block_transactions
            .into_iter()
            .take(tx_offset.0 + 1)
            .map(|tx| stored_txn_to_executable_txn(tx, &storage_txn, state_number))
            .collect::<Result<_, _>>()?;

        drop(storage_txn);

        let block_execution_config = self
            .execution_config
            .get_execution_config_for_block(block_number)
            .map_err(|err| {
                internal_server_error(format!("Failed to get execution config: {}", err))
            })?
            .clone();
        let chain_id = self.chain_id.clone();
        let reader = self.storage_reader.clone();

        let simulate_transactions_result = tokio::task::spawn_blocking(move || {
            exec_simulate_transactions(
                executable_txns,
                Some(tx_hashes),
                &chain_id,
                reader,
                // TODO(shahak): Add pending data here.
                None,
                None,
                state_number,
                block_number,
                &block_execution_config,
                true,
                true,
            )
        })
        .await
        .map_err(internal_server_error)?;

        match simulate_transactions_result {
            Ok(mut simulation_results) => Ok(simulation_results
                .pop()
                .expect("Should have transaction exeuction result")
                .0
                .into()),
            Err(ExecutionError::StorageError(err)) => Err(internal_server_error(err)),
            Err(err) => Err(ErrorObjectOwned::from(JsonRpcError::try_from(err)?)),
        }
    }

    #[instrument(skip(self), level = "debug", err)]
    async fn trace_block_transactions(
        &self,
        block_id: BlockId,
    ) -> RpcResult<Vec<TransactionTraceWithHash>> {
        let storage_txn = self.storage_reader.begin_ro_txn().map_err(internal_server_error)?;
        let block_number = get_block_number(&storage_txn, block_id)?;

        let casm_marker = storage_txn.get_compiled_class_marker().map_err(internal_server_error)?;
        if casm_marker <= block_number {
            debug!(
                ?block_id,
                ?casm_marker,
                "Block is in the storage, but the compiled classes are not fully synced.",
            );
            return Err(INVALID_BLOCK_HASH.into());
        }

        let block_transactions = storage_txn
            .get_block_transactions(block_number)
            .map_err(internal_server_error)?
            .ok_or_else(|| {
                internal_server_error(StorageError::DBInconsistency {
                    msg: format!("Missing block {block_number} transactions"),
                })
            })?;

        let tx_hashes = storage_txn
            .get_block_transaction_hashes(block_number)
            .map_err(internal_server_error)?
            .ok_or_else(|| {
                internal_server_error(StorageError::DBInconsistency {
                    msg: format!("Missing block {block_number} transactions"),
                })
            })?;

        let state_number = StateNumber::right_before_block(block_number);
        let executable_txns = block_transactions
            .into_iter()
            .map(|tx| stored_txn_to_executable_txn(tx, &storage_txn, state_number))
            .collect::<Result<_, _>>()?;

        drop(storage_txn);

        let block_execution_config = self
            .execution_config
            .get_execution_config_for_block(block_number)
            .map_err(|err| {
                internal_server_error(format!("Failed to get execution config: {}", err))
            })?
            .clone();
        let chain_id = self.chain_id.clone();
        let reader = self.storage_reader.clone();
        let tx_hashes_clone = tx_hashes.clone();

        let simulate_transactions_result = tokio::task::spawn_blocking(move || {
            exec_simulate_transactions(
                executable_txns,
                Some(tx_hashes_clone),
                &chain_id,
                reader,
                // TODO(shahak): Add pending data here.
                None,
                None,
                state_number,
                block_number,
                &block_execution_config,
                true,
                true,
            )
        })
        .await
        .map_err(internal_server_error)?;

        match simulate_transactions_result {
            Ok(simulation_results) => Ok(simulation_results
                .into_iter()
                .zip(tx_hashes)
                .map(|((trace_root, _, _, _), transaction_hash)| TransactionTraceWithHash {
                    transaction_hash,
                    trace_root: trace_root.into(),
                })
                .collect()),
            Err(ExecutionError::StorageError(err)) => Err(internal_server_error(err)),
            Err(err) => Err(ErrorObjectOwned::from(JsonRpcError::try_from(err)?)),
        }
    }
}

async fn read_pending_data(
    pending_data: &Arc<RwLock<PendingData>>,
    storage_reader: &StorageReader,
) -> RpcResult<PendingData> {
    let txn = storage_reader.begin_ro_txn().map_err(internal_server_error)?;
    let latest_header: starknet_api::block::BlockHeader = match get_latest_block_number(&txn)? {
        Some(latest_block_number) => get_block_header_by_number(&txn, latest_block_number)?,
        None => starknet_api::block::BlockHeader {
            parent_hash: BlockHash(
                StarkHash::try_from(GENESIS_HASH).map_err(internal_server_error)?,
            ),
            ..Default::default()
        },
    };
    let pending_data = &pending_data.read().await;
    if pending_data.block.parent_block_hash == latest_header.block_hash {
        Ok((*pending_data).clone())
    } else {
        Ok(PendingData {
            block: PendingBlock {
                parent_block_hash: latest_header.block_hash,
                eth_l1_gas_price: latest_header.gas_price,
                timestamp: latest_header.timestamp,
                sequencer_address: latest_header.sequencer,
                ..Default::default()
            },
            state_update: ClientPendingStateUpdate {
                old_root: latest_header.state_root,
                state_diff: Default::default(),
            },
        })
    }
}

fn do_event_keys_match_filter(event_content: &EventContent, filter: &EventFilter) -> bool {
    filter.keys.iter().enumerate().all(|(i, keys)| {
        event_content.keys.len() > i && (keys.is_empty() || keys.contains(&event_content.keys[i]))
    })
}

impl JsonRpcServerImpl for JsonRpcServerV0_4Impl {
    fn new(
        chain_id: ChainId,
        execution_config: ExecutionConfigByBlock,
        storage_reader: StorageReader,
        max_events_chunk_size: usize,
        max_events_keys: usize,
        starting_block: BlockHashAndNumber,
        shared_highest_block: Arc<RwLock<Option<BlockHashAndNumber>>>,
        pending_data: Arc<RwLock<PendingData>>,
        pending_classes: Arc<RwLock<PendingClasses>>,
        writer_client: Arc<dyn StarknetWriter>,
    ) -> Self {
        Self {
            chain_id,
            execution_config,
            storage_reader,
            max_events_chunk_size,
            max_events_keys,
            starting_block,
            shared_highest_block,
            pending_data,
            pending_classes,
            writer_client,
        }
    }

    fn into_rpc_module(self) -> RpcModule<Self> {
        self.into_rpc()
    }
}
