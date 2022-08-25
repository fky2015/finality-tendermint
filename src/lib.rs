#![warn(missing_docs)]
#![cfg_attr(not(feature = "std"), no_std)]

#[cfg(not(feature = "std"))]
#[macro_use]
extern crate alloc;

#[cfg(feature = "std")]
extern crate std;

#[cfg(not(feature = "std"))]
mod std {
    pub use core::{cmp, hash, iter, mem, num, ops};

    pub mod vec {
        pub use alloc::vec::Vec;
    }

    pub mod collections {
        pub use alloc::collections::{
            btree_map::{self, BTreeMap},
            btree_set::{self, BTreeSet},
        };
    }

    pub mod fmt {
        pub use core::fmt::{Display, Formatter, Result};

        pub trait Debug {}
        impl<T> Debug for T {}
    }
}

/// Arithmetic necessary for a block number.
pub trait BlockNumberOps:
    std::fmt::Debug
    + std::cmp::Ord
    + std::ops::Add<Output = Self>
    + std::ops::Sub<Output = Self>
    + num::One
    + num::Zero
    + num::AsPrimitive<usize>
{
}

impl BlockNumberOps for u64 {}

/// Error for Tendermint
#[derive(Clone, PartialEq)]
#[cfg_attr(any(feature = "std", test), derive(Debug))]
pub enum Error {}

#[cfg(feature = "std")]
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match *self {
            _ => write!(f, "not implemented"),
        }
    }
}

#[cfg(feature = "std")]
impl std::error::Error for Error {}

#[cfg(feature = "std")]
mod messages;

#[cfg(feature = "std")]
mod environment;

#[cfg(feature = "std")]
mod voter;

// #[cfg(feature = "std")]
// mod rpc;

#[cfg(all(test, feature = "std"))]
mod testing;
