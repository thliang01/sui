// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

use anyhow::anyhow;
use async_trait::async_trait;
use jsonrpsee::core::RpcResult;
use move_binary_format::normalized::{Module as NormalizedModule, Type};
use move_core_types::identifier::Identifier;
use std::collections::BTreeMap;
use std::sync::Arc;
use sui_types::intent::{AppId, Intent, IntentMessage, IntentScope, IntentVersion};
use tap::TapFallible;

use crate::api::ReadApiServer;
use fastcrypto::encoding::Base64;
use jsonrpsee::RpcModule;
use sui_core::authority::AuthorityState;
use sui_json_rpc_types::{
    Checkpoint, CheckpointId, DynamicFieldPage, GetObjectDataResponse, GetPastObjectDataResponse,
    GetRawObjectDataResponse, MoveFunctionArgType, ObjectValueKind, Page,
    SuiMoveNormalizedFunction, SuiMoveNormalizedModule, SuiMoveNormalizedStruct, SuiObjectInfo,
    SuiTransactionEffects, SuiTransactionResponse, TransactionsPage,
};
use sui_open_rpc::Module;
use sui_types::base_types::SequenceNumber;
use sui_types::base_types::{ObjectID, SuiAddress, TransactionDigest, TxSequenceNumber};
use sui_types::crypto::sha3_hash;
use sui_types::messages::TransactionData;
use sui_types::messages_checkpoint::{
    CheckpointContents, CheckpointContentsDigest, CheckpointDigest, CheckpointSequenceNumber,
    CheckpointSummary,
};
use sui_types::move_package::normalize_modules;
use sui_types::object::{Data, ObjectRead};
use sui_types::query::TransactionQuery;

use sui_types::dynamic_field::DynamicFieldName;
use tracing::debug;

use crate::api::cap_page_limit;
use crate::error::Error;
use crate::SuiRpcModule;

// An implementation of the read portion of the JSON-RPC interface intended for use in
// Fullnodes.
pub struct ReadApi {
    pub state: Arc<AuthorityState>,
}

impl ReadApi {
    pub fn new(state: Arc<AuthorityState>) -> Self {
        Self { state }
    }

    fn get_checkpoint_internal(&self, id: CheckpointId) -> Result<Checkpoint, Error> {
        Ok(match id {
            CheckpointId::SequenceNumber(seq) => {
                let summary = self.state.get_checkpoint_summary_by_sequence_number(seq)?;
                let content = self.state.get_checkpoint_contents(summary.content_digest)?;
                (summary, content).into()
            }
            CheckpointId::Digest(digest) => {
                let summary = self.state.get_checkpoint_summary_by_digest(digest)?;
                let content = self.state.get_checkpoint_contents(summary.content_digest)?;
                (summary, content).into()
            }
        })
    }
}

#[async_trait]
impl ReadApiServer for ReadApi {
    async fn get_objects_owned_by_address(
        &self,
        address: SuiAddress,
    ) -> RpcResult<Vec<SuiObjectInfo>> {
        Ok(self
            .state
            .get_owner_objects(address)
            .map_err(|e| anyhow!("{e}"))?
            .into_iter()
            .map(SuiObjectInfo::from)
            .collect())
    }

    async fn get_dynamic_fields(
        &self,
        parent_object_id: ObjectID,
        cursor: Option<ObjectID>,
        limit: Option<usize>,
    ) -> RpcResult<DynamicFieldPage> {
        let limit = cap_page_limit(limit);
        let mut data = self
            .state
            .get_dynamic_fields(parent_object_id, cursor, limit + 1)
            .map_err(|e| anyhow!("{e}"))?;
        let next_cursor = data.get(limit).map(|info| info.object_id);
        data.truncate(limit);
        Ok(DynamicFieldPage { data, next_cursor })
    }

    async fn get_object(&self, object_id: ObjectID) -> RpcResult<GetObjectDataResponse> {
        Ok(self
            .state
            .get_object_read(&object_id)
            .await
            .map_err(|e| {
                debug!(?object_id, "Failed to get object: {:?}", e);
                anyhow!("{e}")
            })?
            .try_into()?)
    }

    async fn get_dynamic_field_object(
        &self,
        parent_object_id: ObjectID,
        name: DynamicFieldName,
    ) -> RpcResult<GetObjectDataResponse> {
        let id = self
            .state
            .get_dynamic_field_object_id(parent_object_id, &name)
            .map_err(|e| anyhow!("{e}"))?
            .ok_or_else(|| {
                anyhow!("Cannot find dynamic field [{name:?}] for object [{parent_object_id}].")
            })?;
        self.get_object(id).await
    }

    async fn get_total_transaction_number(&self) -> RpcResult<u64> {
        Ok(self.state.get_total_transaction_number()?)
    }

    async fn get_transactions_in_range(
        &self,
        start: TxSequenceNumber,
        end: TxSequenceNumber,
    ) -> RpcResult<Vec<TransactionDigest>> {
        Ok(self
            .state
            .get_transactions_in_range(start, end)?
            .into_iter()
            .map(|(_, digest)| digest)
            .collect())
    }

    async fn get_transaction(
        &self,
        digest: TransactionDigest,
    ) -> RpcResult<SuiTransactionResponse> {
        let (transaction, effects) = self
            .state
            .get_executed_transaction_and_effects(digest)
            .await
            .tap_err(|err| debug!(tx_digest=?digest, "Failed to get transaction: {:?}", err))?;
        let checkpoint = self
            .state
            .database
            .get_transaction_checkpoint(&digest)
            .map_err(|e| anyhow!("{e}"))?;
        Ok(SuiTransactionResponse {
            transaction: transaction.into_message().try_into()?,
            effects: SuiTransactionEffects::try_from(effects, self.state.module_cache.as_ref())?,
            timestamp_ms: self.state.get_timestamp_ms(&digest).await?,
            confirmed_local_execution: None,
            checkpoint: checkpoint.map(|(_epoch, checkpoint)| checkpoint),
        })
    }

    async fn get_normalized_move_modules_by_package(
        &self,
        package: ObjectID,
    ) -> RpcResult<BTreeMap<String, SuiMoveNormalizedModule>> {
        let modules = get_move_modules_by_package(self, package).await?;
        Ok(modules
            .into_iter()
            .map(|(name, module)| (name, module.into()))
            .collect::<BTreeMap<String, SuiMoveNormalizedModule>>())
    }

    async fn get_normalized_move_module(
        &self,
        package: ObjectID,
        module_name: String,
    ) -> RpcResult<SuiMoveNormalizedModule> {
        let module = get_move_module(self, package, module_name).await?;
        Ok(module.into())
    }

    async fn get_normalized_move_struct(
        &self,
        package: ObjectID,
        module_name: String,
        struct_name: String,
    ) -> RpcResult<SuiMoveNormalizedStruct> {
        let module = get_move_module(self, package, module_name).await?;
        let structs = module.structs;
        let identifier = Identifier::new(struct_name.as_str()).map_err(|e| anyhow!("{e}"))?;
        Ok(match structs.get(&identifier) {
            Some(struct_) => Ok(struct_.clone().into()),
            None => Err(anyhow!(
                "No struct was found with struct name {}",
                struct_name
            )),
        }?)
    }

    async fn get_normalized_move_function(
        &self,
        package: ObjectID,
        module_name: String,
        function_name: String,
    ) -> RpcResult<SuiMoveNormalizedFunction> {
        let module = get_move_module(self, package, module_name).await?;
        let functions = module.exposed_functions;
        let identifier = Identifier::new(function_name.as_str()).map_err(|e| anyhow!("{e}"))?;
        Ok(match functions.get(&identifier) {
            Some(function) => Ok(function.clone().into()),
            None => Err(anyhow!(
                "No function was found with function name {}",
                function_name
            )),
        }?)
    }

    async fn get_move_function_arg_types(
        &self,
        package: ObjectID,
        module: String,
        function: String,
    ) -> RpcResult<Vec<MoveFunctionArgType>> {
        let object_read = self
            .state
            .get_object_read(&package)
            .await
            .map_err(|e| anyhow!("{e}"))?;

        let normalized = match object_read {
            ObjectRead::Exists(_obj_ref, object, _layout) => match object.data {
                Data::Package(p) => normalize_modules(p.serialized_module_map().values())
                    .map_err(|e| anyhow!("{e}")),
                _ => Err(anyhow!("Object is not a package with ID {}", package)),
            },
            _ => Err(anyhow!("Package object does not exist with ID {}", package)),
        }?;

        let identifier = Identifier::new(function.as_str()).map_err(|e| anyhow!("{e}"))?;
        let parameters = normalized.get(&module).and_then(|m| {
            m.exposed_functions
                .get(&identifier)
                .map(|f| f.parameters.clone())
        });

        Ok(match parameters {
            Some(parameters) => Ok(parameters
                .iter()
                .map(|p| match p {
                    Type::Struct {
                        address: _,
                        module: _,
                        name: _,
                        type_arguments: _,
                    } => MoveFunctionArgType::Object(ObjectValueKind::ByValue),
                    Type::Reference(_) => {
                        MoveFunctionArgType::Object(ObjectValueKind::ByImmutableReference)
                    }
                    Type::MutableReference(_) => {
                        MoveFunctionArgType::Object(ObjectValueKind::ByMutableReference)
                    }
                    _ => MoveFunctionArgType::Pure,
                })
                .collect::<Vec<MoveFunctionArgType>>()),
            None => Err(anyhow!("No parameters found for function {}", function)),
        }?)
    }

    async fn get_transactions(
        &self,
        query: TransactionQuery,
        cursor: Option<TransactionDigest>,
        limit: Option<usize>,
        descending_order: Option<bool>,
    ) -> RpcResult<TransactionsPage> {
        let limit = cap_page_limit(limit);
        let descending = descending_order.unwrap_or_default();

        // Retrieve 1 extra item for next cursor
        let mut data = self
            .state
            .get_transactions(query, cursor, Some(limit + 1), descending)?;

        // extract next cursor
        let next_cursor = data.get(limit).cloned();
        data.truncate(limit);
        Ok(Page { data, next_cursor })
    }

    async fn try_get_past_object(
        &self,
        object_id: ObjectID,
        version: SequenceNumber,
    ) -> RpcResult<GetPastObjectDataResponse> {
        Ok(self
            .state
            .get_past_object_read(&object_id, version)
            .await
            .map_err(|e| anyhow!("{e}"))?
            .try_into()?)
    }

    async fn get_latest_checkpoint_sequence_number(&self) -> RpcResult<CheckpointSequenceNumber> {
        Ok(self
            .state
            .get_latest_checkpoint_sequence_number()
            .map_err(|e| {
                anyhow!("Latest checkpoint sequence number was not found with error :{e}")
            })?)
    }

    async fn get_checkpoint(&self, id: CheckpointId) -> RpcResult<Checkpoint> {
        Ok(self.get_checkpoint_internal(id)?)
    }

    async fn get_checkpoint_summary_by_digest(
        &self,
        digest: CheckpointDigest,
    ) -> RpcResult<CheckpointSummary> {
        Ok(self
            .state
            .get_checkpoint_summary_by_digest(digest)
            .map_err(|e| {
                anyhow!(
                    "Checkpoint summary based on digest: {digest:?} were not found with error: {e}"
                )
            })?)
    }

    async fn get_checkpoint_summary(
        &self,
        sequence_number: CheckpointSequenceNumber,
    ) -> RpcResult<CheckpointSummary> {
        Ok(self.state.get_checkpoint_summary_by_sequence_number(sequence_number)
            .map_err(|e| anyhow!("Checkpoint summary based on sequence number: {sequence_number} was not found with error :{e}"))?)
    }

    async fn get_checkpoint_contents_by_digest(
        &self,
        digest: CheckpointContentsDigest,
    ) -> RpcResult<CheckpointContents> {
        Ok(self.state.get_checkpoint_contents(digest).map_err(|e| {
            anyhow!(
                "Checkpoint contents based on digest: {digest:?} were not found with error: {e}"
            )
        })?)
    }

    async fn get_checkpoint_contents(
        &self,
        sequence_number: CheckpointSequenceNumber,
    ) -> RpcResult<CheckpointContents> {
        Ok(self
            .state
            .get_checkpoint_contents_by_sequence_number(sequence_number)
            .map_err(|e| anyhow!("Checkpoint contents based on seq number: {sequence_number} were not found with error: {e}"))?)
    }

    async fn get_raw_object(&self, object_id: ObjectID) -> RpcResult<GetRawObjectDataResponse> {
        Ok(self
            .state
            .get_object_read(&object_id)
            .await
            .map_err(|e| anyhow!("{e}"))?
            .try_into()?)
    }
}

impl SuiRpcModule for ReadApi {
    fn rpc(self) -> RpcModule<Self> {
        self.into_rpc()
    }

    fn rpc_doc_module() -> Module {
        crate::api::ReadApiOpenRpc::module_doc()
    }
}

pub async fn get_move_module(
    fullnode_api: &ReadApi,
    package: ObjectID,
    module_name: String,
) -> RpcResult<NormalizedModule> {
    let normalized = get_move_modules_by_package(fullnode_api, package).await?;
    Ok(match normalized.get(&module_name) {
        Some(module) => Ok(module.clone()),
        None => Err(anyhow!("No module found with module name {}", module_name)),
    }?)
}

pub async fn get_move_modules_by_package(
    fullnode_api: &ReadApi,
    package: ObjectID,
) -> RpcResult<BTreeMap<String, NormalizedModule>> {
    let object_read = fullnode_api
        .state
        .get_object_read(&package)
        .await
        .map_err(|e| anyhow!("{e}"))?;

    Ok(match object_read {
        ObjectRead::Exists(_obj_ref, object, _layout) => match object.data {
            Data::Package(p) => {
                normalize_modules(p.serialized_module_map().values()).map_err(|e| anyhow!("{e}"))
            }
            _ => Err(anyhow!("Object is not a package with ID {}", package)),
        },
        _ => Err(anyhow!("Package object does not exist with ID {}", package)),
    }?)
}

pub fn get_transaction_data_and_digest(
    tx_bytes: Base64,
) -> RpcResult<(TransactionData, TransactionDigest)> {
    let tx_data =
        bcs::from_bytes(&tx_bytes.to_vec().map_err(|e| anyhow!(e))?).map_err(|e| anyhow!(e))?;
    let intent_msg = IntentMessage::new(
        Intent {
            version: IntentVersion::V0,
            scope: IntentScope::TransactionData,
            app_id: AppId::Sui,
        },
        tx_data,
    );
    let txn_digest = TransactionDigest::new(sha3_hash(&intent_msg.value));
    Ok((intent_msg.value, txn_digest))
}
