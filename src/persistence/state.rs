// RGB standard library for working with smart contracts on Bitcoin & Lightning
//
// SPDX-License-Identifier: Apache-2.0
//
// Written in 2019-2024 by
//     Dr Maxim Orlovsky <orlovsky@lnp-bp.org>
//
// Copyright (C) 2019-2024 LNP/BP Standards Association. All rights reserved.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt::Debug;
use std::iter;

use invoice::Amount;
use rgb::validation::{ResolveWitness, WitnessResolverError};
use rgb::vm::{ContractStateAccess, WitnessOrd};
use rgb::{
    AssetTag, AttachState, BlindingFactor, ContractId, DataState, Extension, Genesis, Operation,
    RevealedAttach, RevealedData, RevealedValue, Schema, SchemaId, Transition, TransitionBundle,
    VoidState, XWitnessId,
};

use crate::containers::{ConsignmentExt, ToWitnessId};
use crate::contract::OutputAssignment;
use crate::persistence::{StoreTransaction, UpdateRes};

#[derive(Clone, PartialEq, Eq, Debug, Display, Error, From)]
#[display(inner)]
pub enum StateError<P: StateProvider> {
    /// Connectivity errors which may be recoverable and temporary.
    ReadProvider(<P as StateReadProvider>::Error),

    /// Connectivity errors which may be recoverable and temporary.
    WriteProvider(<P as StateWriteProvider>::Error),

    /// witness {0} can't be resolved: {1}
    #[display(doc_comments)]
    Resolver(XWitnessId, WitnessResolverError),

    /// {0}
    ///
    /// It may happen due to RGB standard library bug, or indicate internal
    /// stash inconsistency and compromised stash data storage.
    #[from]
    #[display(doc_comments)]
    Inconsistency(StateInconsistency),
}

#[derive(Clone, PartialEq, Eq, Debug, Display, Error)]
#[display(doc_comments)]
pub enum StateInconsistency {
    /// contract state {0} is not known.
    UnknownContract(ContractId),
}

#[derive(Clone, Eq, PartialEq, Debug, Hash)]
pub enum PersistedState {
    Void,
    Amount(Amount, BlindingFactor, AssetTag),
    // TODO: Use RevealedData
    Data(DataState, u128),
    // TODO: Use RevealedAttach
    Attachment(AttachState, u64),
}

impl PersistedState {
    pub(crate) fn update_blinding(&mut self, blinding: BlindingFactor) {
        match self {
            PersistedState::Void => {}
            PersistedState::Amount(_, b, _) => *b = blinding,
            PersistedState::Data(_, _) => {}
            PersistedState::Attachment(_, _) => {}
        }
    }
}

#[derive(Clone, Debug)]
pub struct State<P: StateProvider> {
    provider: P,
}

impl<P: StateProvider> Default for State<P>
where P: Default
{
    fn default() -> Self {
        Self {
            provider: default!(),
        }
    }
}

impl<P: StateProvider> State<P> {
    pub(super) fn new(provider: P) -> Self { Self { provider } }

    #[doc(hidden)]
    pub fn as_provider(&self) -> &P { &self.provider }

    #[doc(hidden)]
    pub(super) fn as_provider_mut(&mut self) -> &mut P { &mut self.provider }

    #[inline]
    pub fn contract_state(
        &self,
        contract_id: ContractId,
    ) -> Result<P::ContractRead<'_>, StateError<P>> {
        self.provider
            .contract_state(contract_id)
            .map_err(StateError::ReadProvider)
    }

    pub fn update_from_bundle<R: ResolveWitness>(
        &mut self,
        contract_id: ContractId,
        bundle: &TransitionBundle,
        witness_id: XWitnessId,
        resolver: R,
    ) -> Result<(), StateError<P>> {
        let mut updater = self
            .as_provider_mut()
            .update_contract(contract_id)
            .map_err(StateError::WriteProvider)?
            .ok_or(StateInconsistency::UnknownContract(contract_id))?;
        for transition in bundle.known_transitions.values() {
            let ord = resolver
                .resolve_pub_witness_ord(witness_id)
                .map_err(|e| StateError::Resolver(witness_id, e))?;
            updater
                .add_transition(transition, WitnessOrd::with(witness_id, ord))
                .map_err(StateError::WriteProvider)?;
        }
        Ok(())
    }

    pub fn update_from_consignment<R: ResolveWitness>(
        &mut self,
        consignment: impl ConsignmentExt,
        resolver: R,
    ) -> Result<(), StateError<P>> {
        let mut state = self
            .as_provider_mut()
            .register_contract(consignment.schema(), consignment.genesis())
            .map_err(StateError::WriteProvider)?;
        let mut extension_idx = consignment
            .extensions()
            .map(Extension::id)
            .zip(iter::repeat(false))
            .collect::<BTreeMap<_, _>>();
        let mut ordered_extensions = BTreeMap::new();
        for bundled_witness in consignment.bundled_witnesses() {
            for bundle in bundled_witness.anchored_bundles.bundles() {
                for transition in bundle.known_transitions.values() {
                    let witness_id = bundled_witness.pub_witness.to_witness_id();
                    let ord = resolver
                        .resolve_pub_witness_ord(witness_id)
                        .map_err(|e| StateError::Resolver(witness_id, e))?;
                    let witness_ord = WitnessOrd::with(witness_id, ord);

                    state
                        .add_transition(transition, witness_ord)
                        .map_err(StateError::WriteProvider)?;
                    for (id, used) in &mut extension_idx {
                        if *used {
                            continue;
                        }
                        for input in &transition.inputs {
                            if input.prev_out.op == *id {
                                *used = true;
                                if let Some(witness_ord2) = ordered_extensions.get_mut(id) {
                                    if *witness_ord2 > witness_ord {
                                        *witness_ord2 = witness_ord;
                                    }
                                } else {
                                    ordered_extensions.insert(*id, witness_ord);
                                }
                            }
                        }
                    }
                }
            }
        }
        for extension in consignment.extensions() {
            if let Some(witness_ord) = ordered_extensions.get(&extension.id()) {
                state
                    .add_extension(extension, *witness_ord)
                    .map_err(StateError::WriteProvider)?;
            }
        }

        Ok(())
    }

    pub fn update_witnesses(
        &mut self,
        resolver: impl ResolveWitness,
        after_height: u32,
    ) -> Result<UpdateRes, StateError<P>> {
        self.provider
            .update_witnesses(resolver, after_height)
            .map_err(StateError::WriteProvider)
    }
}

impl<P: StateProvider> StoreTransaction for State<P> {
    type TransactionErr = StateError<P>;

    fn begin_transaction(&mut self) -> Result<(), Self::TransactionErr> {
        self.provider
            .begin_transaction()
            .map_err(StateError::WriteProvider)
    }

    fn commit_transaction(&mut self) -> Result<(), Self::TransactionErr> {
        self.provider
            .commit_transaction()
            .map_err(StateError::WriteProvider)
    }

    fn rollback_transaction(&mut self) { self.provider.rollback_transaction() }
}

pub trait StateProvider: Debug + StateReadProvider + StateWriteProvider {}

pub trait StateReadProvider {
    type ContractRead<'a>: ContractStateRead
    where Self: 'a;
    type Error: Clone + Eq + Error;

    fn contract_state(
        &self,
        contract_id: ContractId,
    ) -> Result<Self::ContractRead<'_>, Self::Error>;
}

pub trait StateWriteProvider: StoreTransaction<TransactionErr = Self::Error> {
    type ContractWrite<'a>: ContractStateWrite<Error = Self::Error>
    where Self: 'a;
    type Error: Clone + Eq + Error;

    fn register_contract(
        &mut self,
        schema: &Schema,
        genesis: &Genesis,
    ) -> Result<Self::ContractWrite<'_>, Self::Error>;

    fn update_contract(
        &mut self,
        contract_id: ContractId,
    ) -> Result<Option<Self::ContractWrite<'_>>, Self::Error>;

    fn update_witnesses(
        &mut self,
        resolver: impl ResolveWitness,
        after_height: u32,
    ) -> Result<UpdateRes, Self::Error>;
}

pub trait ContractStateRead: ContractStateAccess {
    fn contract_id(&self) -> ContractId;
    fn schema_id(&self) -> SchemaId;
    fn rights_all(&self) -> impl Iterator<Item = &OutputAssignment<VoidState>>;
    fn fungible_all(&self) -> impl Iterator<Item = &OutputAssignment<RevealedValue>>;
    fn data_all(&self) -> impl Iterator<Item = &OutputAssignment<RevealedData>>;
    fn attach_all(&self) -> impl Iterator<Item = &OutputAssignment<RevealedAttach>>;
}

pub trait ContractStateWrite {
    type Error: Clone + Eq + Error;

    fn add_genesis(&mut self, genesis: &Genesis) -> Result<(), Self::Error>;

    fn add_transition(
        &mut self,
        transition: &Transition,
        witness_ord: WitnessOrd,
    ) -> Result<(), Self::Error>;

    fn add_extension(
        &mut self,
        extension: &Extension,
        witness_ord: WitnessOrd,
    ) -> Result<(), Self::Error>;
}
