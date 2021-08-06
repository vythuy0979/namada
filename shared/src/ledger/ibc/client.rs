//! IBC validity predicate for client module

use std::str::FromStr;

use borsh::BorshDeserialize;
use ibc::ics02_client::client_consensus::AnyConsensusState;
use ibc::ics02_client::client_def::{AnyClient, ClientDef};
use ibc::ics02_client::client_state::AnyClientState;
use ibc::ics02_client::client_type::ClientType;
use ibc::ics02_client::context::ClientReader;
use ibc::ics02_client::height::Height;
use ibc::ics24_host::identifier::ClientId;
use ibc::ics24_host::Path;
use tendermint_proto::Protobuf;
use thiserror::Error;

use super::{Ibc, StateChange};
use crate::ledger::storage::{self, StorageHasher};
use crate::types::ibc::{
    ClientUpdateData, ClientUpgradeData, Error as IbcDataError,
};
use crate::types::storage::{Key, KeySeg};

#[allow(missing_docs)]
#[derive(Error, Debug)]
pub enum Error {
    #[error("Key error: {0}")]
    KeyError(String),
    #[error("State change error: {0}")]
    StateChangeError(String),
    #[error("Client error: {0}")]
    ClientError(String),
    #[error("Header error: {0}")]
    HeaderError(String),
    #[error("Proof verification error: {0}")]
    ProofVerificationError(String),
    #[error("Decoding TX data error: {0}")]
    DecodingTxDataError(std::io::Error),
    #[error("IBC data error: {0}")]
    IbcDataError(IbcDataError),
}

/// IBC client functions result
pub type Result<T> = std::result::Result<T, Error>;

impl<'a, DB, H> Ibc<'a, DB, H>
where
    DB: 'static + storage::DB + for<'iter> storage::DBIter<'iter>,
    H: 'static + StorageHasher,
{
    pub(super) fn validate_client(
        &self,
        client_id: &ClientId,
        tx_data: &[u8],
    ) -> Result<bool> {
        match self.get_client_state_change(client_id)? {
            StateChange::Created => self.validate_created_client(client_id),
            StateChange::Updated => {
                self.validate_updated_client(client_id, tx_data)
            }
            _ => Err(Error::StateChangeError(format!(
                "The state change of the client is invalid: ID {}",
                client_id
            ))),
        }
    }

    /// Returns the client ID after #IBC/clients
    pub(super) fn get_client_id(key: &Key) -> Result<ClientId> {
        match key.segments.get(2) {
            Some(id) => ClientId::from_str(&id.raw())
                .map_err(|e| Error::KeyError(e.to_string())),
            None => Err(Error::KeyError(format!(
                "The key doesn't have a client ID: {}",
                key
            ))),
        }
    }

    fn get_client_state_change(
        &self,
        client_id: &ClientId,
    ) -> Result<StateChange> {
        let path = Path::ClientState(client_id.clone()).to_string();
        let key = Key::ibc_key(path)
            .expect("Creating a key for a client type failed");
        self.get_state_change(&key)
            .map_err(|e| Error::StateChangeError(e.to_string()))
    }

    fn validate_created_client(&self, client_id: &ClientId) -> Result<bool> {
        let client_type = match self.client_type(client_id) {
            Some(t) => t,
            None => {
                return Err(Error::ClientError(format!(
                    "The client type doesn't exist: ID {}",
                    client_id
                )));
            }
        };
        let client_state = match ClientReader::client_state(self, client_id) {
            Some(s) => s,
            None => {
                return Err(Error::ClientError(format!(
                    "The client state doesn't exist: ID {}",
                    client_id
                )));
            }
        };
        let height = client_state.latest_height();
        let consensus_state = match self.consensus_state(client_id, height) {
            Some(c) => c,
            None => {
                return Err(Error::ClientError(format!(
                    "The consensus state doesn't exist: ID {}, Height {}",
                    client_id, height
                )));
            }
        };
        Ok(client_type == client_state.client_type()
            && client_type == consensus_state.client_type())
    }

    fn validate_updated_client(
        &self,
        client_id: &ClientId,
        tx_data: &[u8],
    ) -> Result<bool> {
        // check the type of data in tx_data
        match ClientUpdateData::try_from_slice(tx_data) {
            Ok(data) => {
                // "UpdateClient"
                self.verify_update_client(client_id, data)
            }
            Err(_) => {
                // "UpgradeClient"
                let data = ClientUpgradeData::try_from_slice(tx_data)?;
                self.verify_upgrade_client(client_id, data)
            }
        }
    }

    fn verify_update_client(
        &self,
        client_id: &ClientId,
        data: ClientUpdateData,
    ) -> Result<bool> {
        let id = data.client_id()?;
        if id != *client_id {
            return Err(Error::ClientError(format!(
                "The client ID is mismatched: {} in the tx data, {} in the key",
                id, client_id,
            )));
        }

        // check the posterior states
        let client_state = match ClientReader::client_state(self, client_id) {
            Some(s) => s,
            None => {
                return Err(Error::ClientError(format!(
                    "The client state doesn't exist: ID {}",
                    client_id
                )));
            }
        };
        let height = client_state.latest_height();
        let consensus_state = match self.consensus_state(client_id, height) {
            Some(s) => s,
            None => {
                return Err(Error::ClientError(format!(
                    "The consensus state doesn't exist: ID {}, Height {}",
                    client_id, height
                )));
            }
        };
        // check the prior states
        let prev_client_state = self.client_state_pre(client_id)?;
        let prev_consensus_state = self.consensus_state_pre(
            client_id,
            prev_client_state.latest_height(),
        )?;

        let client = AnyClient::from_client_type(client_state.client_type());
        let headers = data.headers()?;
        let updated = headers.iter().try_fold(
            (prev_client_state, prev_consensus_state),
            |(new_client_state, _), header| {
                client.check_header_and_update_state(
                    new_client_state,
                    header.clone(),
                )
            },
        );
        match updated {
            Ok((new_client_state, new_consensus_state)) => Ok(new_client_state
                == client_state
                && new_consensus_state == consensus_state),
            Err(e) => Err(Error::HeaderError(format!(
                "The header is invalid: ID {}, {}",
                client_id, e,
            ))),
        }
    }

    fn verify_upgrade_client(
        &self,
        client_id: &ClientId,
        data: ClientUpgradeData,
    ) -> Result<bool> {
        let id = data.client_id()?;
        if id != *client_id {
            return Err(Error::ClientError(format!(
                "The client ID is mismatched: {} in the tx data, {} in the key",
                id, client_id,
            )));
        }

        // check the posterior states
        let client_state = match ClientReader::client_state(self, client_id) {
            Some(s) => s,
            None => {
                return Err(Error::ClientError(format!(
                    "The client state doesn't exist: ID {}",
                    client_id
                )));
            }
        };
        let height = client_state.latest_height();
        let consensus_state = match self.consensus_state(client_id, height) {
            Some(s) => s,
            None => {
                return Err(Error::ClientError(format!(
                    "The consensus state doesn't exist: ID {}, Height {}",
                    client_id, height
                )));
            }
        };
        // check the prior client state
        let pre_client_state = self.client_state_pre(client_id)?;
        // get proofs
        let client_proof = data.proof_client()?;
        let consensus_proof = data.proof_consensus_state()?;

        let client = AnyClient::from_client_type(client_state.client_type());
        match client.verify_upgrade_and_update_state(
            &pre_client_state,
            &consensus_state,
            client_proof,
            consensus_proof,
        ) {
            Ok((new_client_state, new_consensus_state)) => Ok(new_client_state
                == client_state
                && new_consensus_state == consensus_state),
            Err(e) => Err(Error::ProofVerificationError(e.to_string())),
        }
    }

    fn client_state_pre(&self, client_id: &ClientId) -> Result<AnyClientState> {
        let path = Path::ClientState(client_id.clone()).to_string();
        let key = Key::ibc_key(path)
            .expect("Creating a key for a client state shouldn't fail");
        match self.ctx.read_pre(&key) {
            Ok(Some(value)) => {
                AnyClientState::decode_vec(&value).map_err(|e| {
                    Error::ClientError(format!(
                        "Decoding the client state failed: ID {}, {}",
                        client_id, e
                    ))
                })
            }
            _ => Err(Error::ClientError(format!(
                "The prior client state doesn't exist: ID {}",
                client_id
            ))),
        }
    }

    pub(super) fn client_counter_pre(&self) -> Result<u64> {
        let key = Key::ibc_client_counter();
        match self.ctx.read_post(&key) {
            Ok(Some(value)) => storage::types::decode(&value).map_err(|e| {
                Error::ClientError(format!(
                    "Decoding the client counter failed: {}",
                    e
                ))
            }),
            _ => Err(Error::ClientError(
                "The client counter doesn't exist".to_owned(),
            )),
        }
    }

    fn consensus_state_pre(
        &self,
        client_id: &ClientId,
        height: Height,
    ) -> Result<AnyConsensusState> {
        let path = Path::ClientConsensusState {
            client_id: client_id.clone(),
            epoch: height.revision_number,
            height: height.revision_height,
        }
        .to_string();
        let key = Key::ibc_key(path)
            .expect("Creating a key for a consensus state shouldn't fail");
        match self.ctx.read_pre(&key) {
            Ok(Some(value)) => {
                AnyConsensusState::decode_vec(&value).map_err(|e| {
                    Error::ClientError(format!(
                        "Decoding the consensus state failed: ID {}, Height \
                         {}, {}",
                        client_id, height, e
                    ))
                })
            }
            _ => Err(Error::ClientError(format!(
                "The prior consensus state doesn't exist: ID {}, Height {}",
                client_id, height
            ))),
        }
    }
}

/// Load the posterior client state
impl<'a, DB, H> ClientReader for Ibc<'a, DB, H>
where
    DB: 'static + storage::DB + for<'iter> storage::DBIter<'iter>,
    H: 'static + StorageHasher,
{
    fn client_type(&self, client_id: &ClientId) -> Option<ClientType> {
        let path = Path::ClientType(client_id.clone()).to_string();
        let key = Key::ibc_key(path)
            .expect("Creating a key for a client type shouldn't fail");
        match self.ctx.read_post(&key) {
            Ok(Some(value)) => {
                let s: String = storage::types::decode(&value).ok()?;
                Some(ClientType::from_str(&s).ok()?)
            }
            // returns None even if DB read fails
            _ => None,
        }
    }

    fn client_state(&self, client_id: &ClientId) -> Option<AnyClientState> {
        let path = Path::ClientState(client_id.clone()).to_string();
        let key = Key::ibc_key(path)
            .expect("Creating a key for a client state shouldn't fail");
        match self.ctx.read_post(&key) {
            Ok(Some(value)) => AnyClientState::decode_vec(&value).ok(),
            // returns None even if DB read fails
            _ => None,
        }
    }

    fn consensus_state(
        &self,
        client_id: &ClientId,
        height: Height,
    ) -> Option<AnyConsensusState> {
        let path = Path::ClientConsensusState {
            client_id: client_id.clone(),
            epoch: height.revision_number,
            height: height.revision_height,
        }
        .to_string();
        let key = Key::ibc_key(path)
            .expect("Creating a key for a consensus state shouldn't fail");
        match self.ctx.read_post(&key) {
            Ok(Some(value)) => AnyConsensusState::decode_vec(&value).ok(),
            // returns None even if DB read fails
            _ => None,
        }
    }

    fn client_counter(&self) -> u64 {
        let key = Key::ibc_client_counter();
        match self.ctx.read_post(&key) {
            Ok(Some(value)) => match storage::types::decode(&value) {
                Ok(c) => c,
                Err(e) => {
                    tracing::error!("decoding a client counter failed: {}", e);
                    u64::MIN
                }
            },
            _ => {
                tracing::error!("client counter doesn't exist");
                unreachable!();
            }
        }
    }
}

impl From<IbcDataError> for Error {
    fn from(err: IbcDataError) -> Self {
        Self::IbcDataError(err)
    }
}

impl From<std::io::Error> for Error {
    fn from(err: std::io::Error) -> Self {
        Self::DecodingTxDataError(err)
    }
}
