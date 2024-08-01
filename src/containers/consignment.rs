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

use std::collections::BTreeSet;
use std::fmt;
use std::fmt::{Display, Formatter};
use std::ops::Deref;
use std::str::FromStr;

use aluvm::library::Lib;
use amplify::confinement::{
    Confined, LargeOrdSet, MediumBlob, SmallOrdMap, SmallOrdSet, TinyOrdMap, TinyOrdSet,
};
use amplify::{ByteArray, Bytes32};
use armor::{ArmorHeader, AsciiArmor, StrictArmor};
use baid64::{Baid64ParseError, DisplayBaid64, FromBaid64Str};
use commit_verify::{CommitEncode, CommitEngine, CommitId, CommitmentId, DigestExt, Sha256};
use rgb::validation::{ResolveWitness, Validator, Validity, Warning, CONSIGNMENT_MAX_LIBS};
use rgb::{
    impl_serde_baid64, validation, AttachId, BundleId, ContractId, Extension, Genesis, GraphSeal,
    Operation, Schema, SchemaId, XChain,
};
use rgbcore::validation::ConsignmentApi;
use strict_encoding::{StrictDeserialize, StrictDumb, StrictSerialize};
use strict_types::TypeSystem;

use super::{
    BundledWitness, ContainerVer, ContentId, ContentSigs, IndexedConsignment, Supplement,
    ASCII_ARMOR_CONSIGNMENT_TYPE, ASCII_ARMOR_CONTRACT, ASCII_ARMOR_IFACE, ASCII_ARMOR_SCHEMA,
    ASCII_ARMOR_TERMINAL, ASCII_ARMOR_VERSION,
};
use crate::interface::{Iface, IfaceImpl};
use crate::persistence::{MemContract, MemContractState};
use crate::resolvers::ConsignmentResolver;
use crate::{BundleExt, SecretSeal, LIB_NAME_RGB_STD};

pub type Transfer = Consignment<true>;
pub type Contract = Consignment<false>;

pub trait ConsignmentExt {
    fn contract_id(&self) -> ContractId;
    fn schema_id(&self) -> SchemaId;
    fn schema(&self) -> &Schema;
    fn genesis(&self) -> &Genesis;
    fn extensions(&self) -> impl Iterator<Item = &Extension>;
    fn bundled_witnesses(&self) -> impl Iterator<Item = &BundledWitness>;
}

impl<C: ConsignmentExt> ConsignmentExt for &C {
    #[inline]
    fn contract_id(&self) -> ContractId { (*self).contract_id() }

    #[inline]
    fn schema_id(&self) -> SchemaId { (*self).schema_id() }

    #[inline]
    fn schema(&self) -> &Schema { (*self).schema() }

    #[inline]
    fn genesis(&self) -> &Genesis { (*self).genesis() }

    #[inline]
    fn extensions(&self) -> impl Iterator<Item = &Extension> { (*self).extensions() }

    #[inline]
    fn bundled_witnesses(&self) -> impl Iterator<Item = &BundledWitness> {
        (*self).bundled_witnesses()
    }
}

/// Interface identifier.
///
/// Interface identifier commits to all the interface data.
#[derive(Wrapper, Copy, Clone, Ord, PartialOrd, Eq, PartialEq, Hash, Debug, From)]
#[wrapper(Deref, BorrowSlice, Hex, Index, RangeOps)]
#[derive(StrictType, StrictDumb, StrictEncode, StrictDecode)]
#[strict_type(lib = LIB_NAME_RGB_STD)]
pub struct ConsignmentId(
    #[from]
    #[from([u8; 32])]
    Bytes32,
);

impl From<Sha256> for ConsignmentId {
    fn from(hasher: Sha256) -> Self { hasher.finish().into() }
}

impl CommitmentId for ConsignmentId {
    const TAG: &'static str = "urn:lnp-bp:rgb:consignment#2024-03-11";
}

impl DisplayBaid64 for ConsignmentId {
    const HRI: &'static str = "rgb:csg";
    const CHUNKING: bool = true;
    const PREFIX: bool = true;
    const EMBED_CHECKSUM: bool = false;
    const MNEMONIC: bool = true;
    fn to_baid64_payload(&self) -> [u8; 32] { self.to_byte_array() }
}
impl FromBaid64Str for ConsignmentId {}
impl FromStr for ConsignmentId {
    type Err = Baid64ParseError;
    fn from_str(s: &str) -> Result<Self, Self::Err> { Self::from_baid64_str(s) }
}
impl Display for ConsignmentId {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result { self.fmt_baid64(f) }
}

impl_serde_baid64!(ConsignmentId);

impl ConsignmentId {
    pub const fn from_array(id: [u8; 32]) -> Self { Self(Bytes32::from_array(id)) }
}

pub type ValidContract = ValidConsignment<false>;
pub type ValidTransfer = ValidConsignment<true>;

#[derive(Clone, Debug, Display)]
#[display("{consignment}")]
pub struct ValidConsignment<const TRANSFER: bool> {
    /// Status of the latest validation.
    validation_status: validation::Status,
    consignment: Consignment<TRANSFER>,
}

impl<const TRANSFER: bool> ValidConsignment<TRANSFER> {
    pub fn validation_status(&self) -> &validation::Status { &self.validation_status }

    pub fn into_consignment(self) -> Consignment<TRANSFER> { self.consignment }

    pub fn into_validation_status(self) -> validation::Status { self.validation_status }

    pub fn split(self) -> (Consignment<TRANSFER>, validation::Status) {
        (self.consignment, self.validation_status)
    }
}

impl<const TRANSFER: bool> Deref for ValidConsignment<TRANSFER> {
    type Target = Consignment<TRANSFER>;

    fn deref(&self) -> &Self::Target { &self.consignment }
}

/// Consignment represents contract-specific data, always starting with genesis,
/// which must be valid under client-side-validation rules (i.e. internally
/// consistent and properly committed into the commitment layer, like bitcoin
/// blockchain or current state of the lightning channel).
///
/// All consignments-related procedures, including validation or merging
/// consignments data into stash or schema-specific data storage, must start
/// with `endpoints` and process up to the genesis.
#[derive(Clone, Debug, Display)]
#[display(AsciiArmor::to_ascii_armored_string)]
#[derive(StrictType, StrictDumb, StrictEncode, StrictDecode, PartialEq)]
#[strict_type(lib = LIB_NAME_RGB_STD)]
#[cfg_attr(
    feature = "serde",
    derive(Serialize, Deserialize),
    serde(crate = "serde_crate", rename_all = "camelCase")
)]
pub struct Consignment<const TRANSFER: bool> {
    /// Version.
    pub version: ContainerVer,

    /// Specifies whether the consignment contains information about state
    /// transfer (true), or it is just a consignment with an information about a
    /// contract.
    pub transfer: bool,

    /// Set of secret seals which are history terminals.
    pub terminals: SmallOrdMap<BundleId, XChain<SecretSeal>>,

    /// Genesis data.
    pub genesis: Genesis,

    /// All state extensions contained in the consignment.
    pub extensions: LargeOrdSet<Extension>,

    /// All bundled state transitions contained in the consignment, together
    /// with their witness data.
    pub bundles: LargeOrdSet<BundledWitness>,

    /// Schema (plus root schema, if any) under which contract is issued.
    pub schema: Schema,

    /// Interfaces supported by the contract.
    pub ifaces: TinyOrdMap<Iface, IfaceImpl>,

    /// Known supplements.
    pub supplements: TinyOrdSet<Supplement>,

    /// Type system covering all types used in schema, interfaces and
    /// implementations.
    pub types: TypeSystem,

    /// Collection of scripts used across consignment.
    pub scripts: Confined<BTreeSet<Lib>, 0, CONSIGNMENT_MAX_LIBS>,

    /// Data containers coming with this consignment. For the purposes of
    /// in-memory consignments we are restricting the size of the containers to
    /// 24 bit value (RGB allows containers up to 32-bit values in size).
    pub attachments: SmallOrdMap<AttachId, MediumBlob>,

    /// Signatures on the pieces of content which are the part of the
    /// consignment.
    pub signatures: TinyOrdMap<ContentId, ContentSigs>,
}

impl<const TRANSFER: bool> StrictSerialize for Consignment<TRANSFER> {}
impl<const TRANSFER: bool> StrictDeserialize for Consignment<TRANSFER> {}

impl<const TRANSFER: bool> CommitEncode for Consignment<TRANSFER> {
    type CommitmentId = ConsignmentId;

    fn commit_encode(&self, e: &mut CommitEngine) {
        e.commit_to_serialized(&self.version);
        e.commit_to_serialized(&self.transfer);

        e.commit_to_serialized(&self.contract_id());
        e.commit_to_serialized(&self.genesis.disclose_hash());
        e.commit_to_set(&TinyOrdSet::from_iter_unsafe(
            self.ifaces.values().map(|iimpl| iimpl.impl_id()),
        ));

        e.commit_to_set(&LargeOrdSet::from_iter_unsafe(
            self.bundles.iter().map(BundledWitness::disclose_hash),
        ));
        e.commit_to_set(&LargeOrdSet::from_iter_unsafe(
            self.extensions.iter().map(Extension::disclose_hash),
        ));
        e.commit_to_map(&self.terminals);

        e.commit_to_set(&SmallOrdSet::from_iter_unsafe(self.attachments.keys().copied()));
        e.commit_to_set(&TinyOrdSet::from_iter_unsafe(
            self.supplements.iter().map(|suppl| suppl.suppl_id()),
        ));

        e.commit_to_serialized(&self.types.id());
        e.commit_to_set(&SmallOrdSet::from_iter_unsafe(self.scripts.iter().map(|lib| lib.id())));

        e.commit_to_map(&self.signatures);
    }
}

impl<const TRANSFER: bool> ConsignmentExt for Consignment<TRANSFER> {
    #[inline]
    fn contract_id(&self) -> ContractId { self.genesis.contract_id() }

    #[inline]
    fn schema_id(&self) -> SchemaId { self.schema.schema_id() }

    #[inline]
    fn schema(&self) -> &Schema { &self.schema }

    #[inline]
    fn genesis(&self) -> &Genesis { &self.genesis }

    #[inline]
    fn extensions(&self) -> impl Iterator<Item = &Extension> { self.extensions.iter() }

    #[inline]
    fn bundled_witnesses(&self) -> impl Iterator<Item = &BundledWitness> { self.bundles.iter() }
}

impl<const TRANSFER: bool> Consignment<TRANSFER> {
    #[inline]
    pub fn consignment_id(&self) -> ConsignmentId { self.commit_id() }

    #[inline]
    pub fn schema_id(&self) -> SchemaId { self.schema.schema_id() }

    pub fn reveal_terminal_seals<E>(
        mut self,
        f: impl Fn(XChain<SecretSeal>) -> Result<Option<XChain<GraphSeal>>, E>,
    ) -> Result<Self, E> {
        // We need to clone since ordered set does not allow us to mutate members.
        let mut bundles = LargeOrdSet::with_capacity(self.bundles.len());
        for mut bundled_witness in self.bundles {
            for (bundle_id, secret) in &self.terminals {
                if let Some(seal) = f(*secret)? {
                    for bundle in bundled_witness.anchored_bundles.bundles_mut() {
                        if bundle.bundle_id() == *bundle_id {
                            bundle.reveal_seal(seal);
                            break;
                        }
                    }
                }
            }
            bundles.push(bundled_witness).ok();
        }
        self.bundles = bundles;
        Ok(self)
    }

    pub fn into_contract(self) -> Contract {
        Contract {
            version: self.version,
            transfer: false,
            schema: self.schema,
            ifaces: self.ifaces,
            supplements: self.supplements,
            types: self.types,
            genesis: self.genesis,
            terminals: self.terminals,
            bundles: self.bundles,
            extensions: self.extensions,
            attachments: self.attachments,
            signatures: self.signatures,
            scripts: self.scripts,
        }
    }

    pub fn validate(
        self,
        resolver: &impl ResolveWitness,
        // TODO: Add sig validator
        //_: &impl SigValidator,
        testnet: bool,
    ) -> Result<ValidConsignment<TRANSFER>, (validation::Status, Consignment<TRANSFER>)> {
        let index = IndexedConsignment::new(&self);
        let resolver = ConsignmentResolver {
            consignment: &index,
            fallback: resolver,
        };
        let mut status = Validator::<MemContract<MemContractState>, _, _>::validate(
            &index,
            &resolver,
            testnet,
            (&self.schema, self.contract_id()),
        );

        let validity = status.validity();

        if self.transfer != TRANSFER {
            status.add_warning(Warning::Custom(s!("invalid consignment type")));
        }
        // check ifaceid match implementation
        for (iface, iimpl) in self.ifaces.iter() {
            if iface.iface_id() != iimpl.iface_id {
                status.add_warning(Warning::Custom(format!(
                    "implementation {} targets different interface {} than expected {}",
                    iimpl.impl_id(),
                    iimpl.iface_id,
                    iface.iface_id()
                )));
            }
        }

        // check bundle ids listed in terminals are present in the consignment
        for bundle_id in self.terminals.keys() {
            if !index.bundle_ids().any(|id| id == *bundle_id) {
                status.add_warning(Warning::Custom(format!(
                    "terminal bundle id {bundle_id} is not present in the consignment"
                )));
            }
        }
        // TODO: check attach ids from data containers are present in operations
        // TODO: validate sigs and remove untrusted
        // TODO: Check that all extensions present in the consignment are used by state
        // transitions

        if validity != Validity::Valid {
            Err((status, self))
        } else {
            Ok(ValidConsignment {
                validation_status: status,
                consignment: self,
            })
        }
    }
}

impl<const TRANSFER: bool> StrictArmor for Consignment<TRANSFER> {
    type Id = ConsignmentId;
    const PLATE_TITLE: &'static str = "RGB CONSIGNMENT";

    fn armor_id(&self) -> Self::Id { self.commit_id() }
    fn armor_headers(&self) -> Vec<ArmorHeader> {
        let mut headers = vec![
            ArmorHeader::new(ASCII_ARMOR_VERSION, format!("{:#}", self.version)),
            ArmorHeader::new(
                ASCII_ARMOR_CONSIGNMENT_TYPE,
                if self.transfer {
                    s!("transfer")
                } else {
                    s!("contract")
                },
            ),
            ArmorHeader::new(ASCII_ARMOR_CONTRACT, self.contract_id().to_string()),
            ArmorHeader::new(ASCII_ARMOR_SCHEMA, self.schema.schema_id().to_string()),
        ];
        if !self.ifaces.is_empty() {
            headers.push(ArmorHeader::with(
                ASCII_ARMOR_IFACE,
                self.ifaces.keys().map(|iface| iface.name.to_string()),
            ));
        }
        if !self.terminals.is_empty() {
            headers.push(ArmorHeader::with(
                ASCII_ARMOR_TERMINAL,
                self.terminals.keys().map(BundleId::to_string),
            ));
        }
        headers
    }
}

impl<const TRANSFER: bool> FromStr for Consignment<TRANSFER> {
    type Err = armor::StrictArmorError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::from_ascii_armored_str(s)

        // TODO: Ensure that contract.transfer == TRANSFER
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn contract_str_round_trip() {
        let contract = Contract::from_str(include_str!("../../asset/armored_contract.default"))
            .expect("contract from str should work");
        assert_eq!(
            contract.to_string(),
            include_str!("../../asset/armored_contract.default"),
            "contract string round trip fails"
        );
    }

    #[test]
    fn error_contract_strs() {
        assert!(
            Contract::from_str(
                r#"-----BEGIN RGB CONSIGNMENT-----
Id: rgb:csg:Kej4ueIS-4VLOUQe-SpFUOCe-BzEZ$Lt-C4PNmJC-J0Um7WM#cigar-pretend-mayor
Version: 2
Type: contract
Contract: rgb:T24t0N1D-eiInTgb-BXlrrXz-$7OgV6n-WJWHPUD-BWNuqZw
Schema: rgb:sch:CyqM42yAdM1moWyNZPQedAYt73BM$k9z$dKLUXY1voA#cello-global-deluxe
Check-SHA256: 181748dae0c83cbb44f6ccfdaddf6faca0bc4122a9f35fef47bab9aea023e4a1

0ssI2000000000000000000000000000000000000000000000000000000D0CRI`I$>^aZh38Qb#nj!
0000000000000000000000d59ZDjxe00000000dDb8~4rVQz13d2MfXa{vGU00000000000000000000
0000000000000

-----END RGB CONSIGNMENT-----"#
            )
            .is_ok()
        );

        // Wrong Id
        assert!(
            Contract::from_str(
                r#"-----BEGIN RGB CONSIGNMENT-----
Id: rgb:csg:aaaaaaaa-aaaaaaa-aaaaaaa-aaaaaaa-aaaaaaa-aaaaaaa#guide-campus-arctic
Version: 2
Type: contract
Contract: rgb:qm7P!06T-uuBQT56-ovwOLzx-9Gka7Nb-84Nwo8g-blLb8kw
Schema: rgb:sch:CyqM42yAdM1moWyNZPQedAYt73BM$k9z$dKLUXY1voA#cello-global-deluxe
Check-SHA256: 181748dae0c83cbb44f6ccfdaddf6faca0bc4122a9f35fef47bab9aea023e4a1

0ssI2000000000000000000000000000000000000000000000000000000D0CRI`I$>^aZh38Qb#nj!
0000000000000000000000d59ZDjxe00000000dDb8~4rVQz13d2MfXa{vGU00000000000000000000
0000000000000

-----END RGB CONSIGNMENT-----"#
            )
            .is_err()
        );

        // Wrong checksum
        assert!(
            Contract::from_str(
                r#"-----BEGIN RGB CONSIGNMENT-----
Id: rgb:csg:poAMvm9j-NdapxqA-MJ!5dwP-d!IIt2A-T!5OiXE-Tl54Yew#guide-campus-arctic
Version: 2
Type: contract
Contract: rgb:qm7P!06T-uuBQT56-ovwOLzx-9Gka7Nb-84Nwo8g-blLb8kw
Schema: rgb:sch:CyqM42yAdM1moWyNZPQedAYt73BM$k9z$dKLUXY1voA#cello-global-deluxe
Check-SHA256: aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa

0ssI2000000000000000000000000000000000000000000000000000000D0CRI`I$>^aZh38Qb#nj!
0000000000000000000000d59ZDjxe00000000dDb8~4rVQz13d2MfXa{vGU00000000000000000000
0000000000000

-----END RGB CONSIGNMENT-----"#
            )
            .is_err()
        );
    }

    #[test]
    fn transfer_str_round_trip() {
        let transfer = Transfer::from_str(include_str!("../../asset/armored_transfer.default"))
            .expect("transfer from str should work");
        assert_eq!(
            transfer.to_string(),
            include_str!("../../asset/armored_transfer.default"),
            "transfer string round trip fails"
        );
    }

    #[test]
    fn error_transfer_strs() {
        assert!(
            Transfer::from_str(
                r#"-----BEGIN RGB CONSIGNMENT-----
Id: rgb:csg:Kej4ueIS-4VLOUQe-SpFUOCe-BzEZ$Lt-C4PNmJC-J0Um7WM#cigar-pretend-mayor
Version: 2
Type: transfer
Contract: rgb:T24t0N1D-eiInTgb-BXlrrXz-$7OgV6n-WJWHPUD-BWNuqZw
Schema: rgb:sch:CyqM42yAdM1moWyNZPQedAYt73BM$k9z$dKLUXY1voA#cello-global-deluxe
Check-SHA256: 181748dae0c83cbb44f6ccfdaddf6faca0bc4122a9f35fef47bab9aea023e4a1

0ssI2000000000000000000000000000000000000000000000000000000D0CRI`I$>^aZh38Qb#nj!
0000000000000000000000d59ZDjxe00000000dDb8~4rVQz13d2MfXa{vGU00000000000000000000
0000000000000

-----END RGB CONSIGNMENT-----"#
            )
            .is_ok()
        );

        // Wrong Id
        assert!(
            Transfer::from_str(
                r#"-----BEGIN RGB CONSIGNMENT-----
Id: rgb:csg:aaaaaaaa-aaaaaaa-aaaaaaa-aaaaaaa-aaaaaaa-aaaaaaa#guide-campus-arctic
Version: 2
Type: transfer
Contract: rgb:qm7P!06T-uuBQT56-ovwOLzx-9Gka7Nb-84Nwo8g-blLb8kw
Schema: rgb:sch:CyqM42yAdM1moWyNZPQedAYt73BM$k9z$dKLUXY1voA#cello-global-deluxe
Check-SHA256: 181748dae0c83cbb44f6ccfdaddf6faca0bc4122a9f35fef47bab9aea023e4a1

0ssI2000000000000000000000000000000000000000000000000000000D0CRI`I$>^aZh38Qb#nj!
0000000000000000000000d59ZDjxe00000000dDb8~4rVQz13d2MfXa{vGU00000000000000000000
0000000000000

-----END RGB CONSIGNMENT-----"#
            )
            .is_err()
        );

        // Wrong checksum
        assert!(
            Transfer::from_str(
                r#"-----BEGIN RGB CONSIGNMENT-----
Id: rgb:csg:poAMvm9j-NdapxqA-MJ!5dwP-d!IIt2A-T!5OiXE-Tl54Yew#guide-campus-arctic
Version: 2
Type: transfer
Contract: rgb:qm7P!06T-uuBQT56-ovwOLzx-9Gka7Nb-84Nwo8g-blLb8kw
Schema: rgb:sch:CyqM42yAdM1moWyNZPQedAYt73BM$k9z$dKLUXY1voA#cello-global-deluxe
Check-SHA256: aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa

0ssI2000000000000000000000000000000000000000000000000000000D0CRI`I$>^aZh38Qb#nj!
0000000000000000000000d59ZDjxe00000000dDb8~4rVQz13d2MfXa{vGU00000000000000000000
0000000000000

-----END RGB CONSIGNMENT-----"#
            )
            .is_err()
        );
    }
}
