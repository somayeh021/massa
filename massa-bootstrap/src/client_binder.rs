// Copyright (c) 2022 MASSA LABS <info@massa.net>

use crate::error::BootstrapError;
use crate::establisher::types::Duplex;
use crate::messages::{
    BootstrapClientMessage, BootstrapClientMessageSerializer, BootstrapServerMessage,
    BootstrapServerMessageDeserializer,
};
use async_speed_limit::clock::StandardClock;
use async_speed_limit::{Limiter, Resource};
use massa_hash::{Hash, HASH_SIZE_BYTES};
use massa_models::constants::default::{
    MAX_BOOTSTRAP_ASYNC_POOL_CHANGES, MAX_BOOTSTRAP_FINAL_STATE_PARTS_SIZE,
    MAX_DATASTORE_ENTRY_COUNT, MAX_DATASTORE_KEY_LENGTH, MAX_DATASTORE_VALUE_LENGTH,
    MAX_DATA_ASYNC_MESSAGE, MAX_LEDGER_CHANGES_COUNT, MAX_LEDGER_CHANGES_PER_SLOT,
    MAX_PRODUCTION_EVENTS_PER_BLOCK, MAX_PRODUCTION_STATS_LENGTH, MAX_RNG_SEED_LENGTH,
    MAX_ROLLS_COUNTS_LENGTH, MAX_ROLLS_UPDATE_LENGTH,
};
use massa_models::constants::{
    ENDORSEMENT_COUNT, MAX_ADVERTISE_LENGTH, MAX_BOOTSTRAP_BLOCKS, MAX_BOOTSTRAP_CHILDREN,
    MAX_BOOTSTRAP_CLIQUES, MAX_BOOTSTRAP_DEPS, MAX_BOOTSTRAP_POS_CYCLES, MAX_BOOTSTRAP_POS_ENTRIES,
    MAX_OPERATIONS_PER_BLOCK, THREAD_COUNT,
};
use massa_models::Version;
use massa_models::{
    constants::BOOTSTRAP_RANDOMNESS_SIZE_BYTES, DeserializeMinBEInt, SerializeMinBEInt,
    VersionSerializer,
};
use massa_serialization::{DeserializeError, Deserializer, Serializer};
use massa_signature::{PublicKey, Signature, SIGNATURE_SIZE_BYTES};
use rand::{rngs::StdRng, RngCore, SeedableRng};
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;

/// Bootstrap client binder
pub struct BootstrapClientBinder {
    max_bootstrap_message_size: u32,
    size_field_len: usize,
    remote_pubkey: PublicKey,
    duplex: Resource<Duplex, StandardClock>,
    prev_message: Option<Hash>,
    version_serializer: VersionSerializer,
}

impl BootstrapClientBinder {
    /// Creates a new `WriteBinder`.
    ///
    /// # Argument
    /// * duplex: duplex stream.
    /// * limit: limit max bytes per second (up and down)
    pub fn new(
        duplex: Duplex,
        remote_pubkey: PublicKey,
        limit: f64,
        max_bootstrap_message_size: u32,
    ) -> Self {
        let size_field_len = u32::be_bytes_min_length(max_bootstrap_message_size);
        BootstrapClientBinder {
            max_bootstrap_message_size,
            size_field_len,
            remote_pubkey,
            duplex: <Limiter>::new(limit).limit(duplex),
            prev_message: None,
            version_serializer: VersionSerializer::new(),
        }
    }
}

impl BootstrapClientBinder {
    /// Performs a handshake. Should be called after connection
    /// NOT cancel-safe
    pub async fn handshake(&mut self, version: Version) -> Result<(), BootstrapError> {
        // send version and randomn bytes
        let msg_hash = {
            let mut version_ser = Vec::new();
            self.version_serializer
                .serialize(&version, &mut version_ser)?;
            let mut version_random_bytes =
                vec![0u8; version_ser.len() + BOOTSTRAP_RANDOMNESS_SIZE_BYTES];
            version_random_bytes[..version_ser.len()].clone_from_slice(&version_ser);
            StdRng::from_entropy().fill_bytes(&mut version_random_bytes[version_ser.len()..]);
            self.duplex.write_all(&version_random_bytes).await?;
            Hash::compute_from(&version_random_bytes)
        };

        self.prev_message = Some(msg_hash);

        Ok(())
    }

    /// Reads the next message. NOT cancel-safe
    pub async fn next(&mut self) -> Result<BootstrapServerMessage, BootstrapError> {
        // read signature
        let sig = {
            let mut sig_bytes = [0u8; SIGNATURE_SIZE_BYTES];
            self.duplex.read_exact(&mut sig_bytes).await?;
            Signature::from_bytes(&sig_bytes)?
        };

        // read message length
        let msg_len = {
            let mut msg_len_bytes = vec![0u8; self.size_field_len];
            self.duplex.read_exact(&mut msg_len_bytes[..]).await?;
            u32::from_be_bytes_min(&msg_len_bytes, self.max_bootstrap_message_size)?.0
        };

        // read message, check signature and check signature of the message sent just before then deserialize it
        let message_deserializer = BootstrapServerMessageDeserializer::new(
            THREAD_COUNT,
            ENDORSEMENT_COUNT,
            MAX_ADVERTISE_LENGTH,
            MAX_BOOTSTRAP_BLOCKS,
            MAX_BOOTSTRAP_CLIQUES,
            MAX_BOOTSTRAP_CHILDREN,
            MAX_BOOTSTRAP_DEPS,
            MAX_BOOTSTRAP_POS_CYCLES,
            MAX_BOOTSTRAP_POS_ENTRIES,
            MAX_OPERATIONS_PER_BLOCK,
            MAX_BOOTSTRAP_FINAL_STATE_PARTS_SIZE,
            MAX_RNG_SEED_LENGTH,
            MAX_ROLLS_UPDATE_LENGTH,
            MAX_ROLLS_COUNTS_LENGTH,
            MAX_PRODUCTION_STATS_LENGTH,
            MAX_BOOTSTRAP_ASYNC_POOL_CHANGES,
            MAX_DATA_ASYNC_MESSAGE,
            MAX_LEDGER_CHANGES_COUNT,
            MAX_DATASTORE_KEY_LENGTH as u64,
            MAX_DATASTORE_VALUE_LENGTH,
            MAX_DATASTORE_ENTRY_COUNT,
            MAX_LEDGER_CHANGES_PER_SLOT,
            MAX_PRODUCTION_EVENTS_PER_BLOCK,
        );
        let message = {
            if let Some(prev_message) = self.prev_message {
                self.prev_message = Some(Hash::compute_from(&sig.to_bytes()));
                let mut sig_msg_bytes = vec![0u8; HASH_SIZE_BYTES + (msg_len as usize)];
                sig_msg_bytes[..HASH_SIZE_BYTES].copy_from_slice(prev_message.to_bytes());
                self.duplex
                    .read_exact(&mut sig_msg_bytes[HASH_SIZE_BYTES..])
                    .await?;
                let msg_hash = Hash::compute_from(&sig_msg_bytes);
                self.remote_pubkey.verify_signature(&msg_hash, &sig)?;
                let (_, msg) = message_deserializer
                    .deserialize::<DeserializeError>(&sig_msg_bytes[HASH_SIZE_BYTES..])
                    .map_err(|err| BootstrapError::GeneralError(format!("{}", err)))?;
                msg
            } else {
                self.prev_message = Some(Hash::compute_from(&sig.to_bytes()));
                let mut sig_msg_bytes = vec![0u8; msg_len as usize];
                self.duplex.read_exact(&mut sig_msg_bytes[..]).await?;
                let msg_hash = Hash::compute_from(&sig_msg_bytes);
                self.remote_pubkey.verify_signature(&msg_hash, &sig)?;
                let (_, msg) = message_deserializer
                    .deserialize::<DeserializeError>(&sig_msg_bytes[..])
                    .map_err(|err| BootstrapError::GeneralError(format!("{}", err)))?;
                msg
            }
        };
        Ok(message)
    }

    #[allow(dead_code)]
    /// Send a message to the bootstrap server
    pub async fn send(&mut self, msg: &BootstrapClientMessage) -> Result<(), BootstrapError> {
        let mut msg_bytes = Vec::new();
        let message_serializer = BootstrapClientMessageSerializer::new();
        message_serializer.serialize(msg, &mut msg_bytes)?;
        let msg_len: u32 = msg_bytes.len().try_into().map_err(|e| {
            BootstrapError::GeneralError(format!("bootstrap message too large to encode: {}", e))
        })?;

        if let Some(prev_message) = self.prev_message {
            // there was a previous message
            let prev_message = prev_message.to_bytes();

            // update current previous message to be hash(prev_msg_hash + msg)
            let mut hash_data =
                Vec::with_capacity(prev_message.len().saturating_add(msg_bytes.len()));
            hash_data.extend(prev_message);
            hash_data.extend(&msg_bytes);
            self.prev_message = Some(Hash::compute_from(&hash_data));

            // send old previous message
            self.duplex.write_all(prev_message).await?;
        } else {
            // there was no previous message

            //update current previous message
            self.prev_message = Some(Hash::compute_from(&msg_bytes));
        }

        // send message length
        {
            let msg_len_bytes = msg_len.to_be_bytes_min(self.max_bootstrap_message_size)?;
            self.duplex.write_all(&msg_len_bytes).await?;
        }

        // send message
        self.duplex.write_all(&msg_bytes).await?;
        Ok(())
    }
}
