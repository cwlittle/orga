pub mod amount;
use std::{fmt::Display, str::FromStr};

pub use amount::*;

pub mod symbol;
pub use symbol::*;

pub mod coin;
pub use coin::*;

pub mod share;
pub use share::*;

pub mod multishare;
pub use multishare::*;

pub mod give;
pub use give::*;

pub mod take;
pub use take::*;

pub mod transfer;
pub use transfer::*;

pub mod pool;
pub use pool::*;

pub mod staking;
pub use staking::*;

pub mod accounts;
pub use accounts::*;

pub mod adjust;
pub use adjust::*;

pub mod balance;
pub use balance::*;

pub mod decimal;
pub use decimal::Decimal;

pub mod math;
pub use math::*;

pub mod faucet;
pub use faucet::*;

mod ops;
pub use ops::*;

use bech32::{self, encode_to_fmt, FromBase32, ToBase32, Variant};

use crate::client::Client;
use crate::collections::Next;
use crate::describe::Describe;
use crate::macros::State;
use crate::query::Query;
use crate::{call::Call, migrate::MigrateFrom};
use ed::{Decode, Encode};
use ripemd::{Digest as _, Ripemd160};
use serde::{Deserialize, Serialize};
use sha2::Sha256;

#[derive(
    Encode,
    Decode,
    State,
    Next,
    Query,
    Clone,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Debug,
    Copy,
    Client,
    Call,
    Default,
    Describe,
    MigrateFrom,
)]
pub struct Address {
    bytes: [u8; Address::LENGTH],
}

impl Address {
    pub const LENGTH: usize = 20;
    pub const NULL: Self = Address {
        bytes: [0; Self::LENGTH],
    };

    pub fn from_pubkey(bytes: [u8; 33]) -> Self {
        let mut sha = Sha256::new();
        sha.update(bytes);
        let hash = sha.finalize();

        let mut ripemd = Ripemd160::new();
        ripemd.update(hash);
        let hash = ripemd.finalize();

        let mut bytes = [0; Address::LENGTH];
        bytes.copy_from_slice(hash.as_slice());

        Self { bytes }
    }

    pub fn from_pubkey_eth(bytes: [u8; 64]) -> Self {
        use sha3::{Digest, Keccak256};
        let mut hasher = Keccak256::new();
        hasher.update(bytes);
        let hash = hasher.finalize();

        let mut bytes = [0; Address::LENGTH];
        bytes.copy_from_slice(&hash[12..]);

        Self { bytes }
    }

    pub fn bytes(&self) -> [u8; Address::LENGTH] {
        self.bytes
    }

    pub fn is_null(&self) -> bool {
        *self == Self::NULL
    }
}

impl Display for Address {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        encode_to_fmt(f, "nomic", self.bytes.to_base32(), Variant::Bech32).unwrap()
    }
}

impl FromStr for Address {
    type Err = bech32::Error;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (hrp, data, variant) = bech32::decode(s)?;
        if hrp != "nomic" {
            return Err(bech32::Error::MissingSeparator);
        }
        if variant != Variant::Bech32 {
            return Err(bech32::Error::InvalidData(0));
        }
        let data: Vec<u8> = FromBase32::from_base32(&data)?;

        if data.len() != Address::LENGTH {
            return Err(bech32::Error::InvalidData(1));
        }
        let mut bytes = [0u8; Address::LENGTH];
        bytes.copy_from_slice(&data);

        Ok(Address { bytes })
    }
}

impl Serialize for Address {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(self.to_string().as_str())
    }
}

impl<'de> Deserialize<'de> for Address {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct AddressVisitor;

        impl<'de> serde::de::Visitor<'de> for AddressVisitor {
            type Value = Address;

            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter.write_str("a bech32-encoded string")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                value.parse().map_err(serde::de::Error::custom)
            }

            fn visit_string<E>(self, value: String) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                value.parse().map_err(serde::de::Error::custom)
            }
        }

        deserializer.deserialize_str(AddressVisitor)
    }
}

impl From<[u8; Address::LENGTH]> for Address {
    fn from(bytes: [u8; Address::LENGTH]) -> Self {
        Address { bytes }
    }
}

impl From<Address> for [u8; Address::LENGTH] {
    fn from(addr: Address) -> Self {
        addr.bytes()
    }
}
