// Copyright 2015-2017 Parity Technologies (UK) Ltd.
// This file is part of Parity.

// Parity is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Parity is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Parity.  If not, see <http://www.gnu.org/licenses/>.

#![warn(missing_docs)]
#![cfg_attr(feature="benches", feature(test))]
#![cfg_attr(feature="dev", feature(plugin))]
#![cfg_attr(feature="dev", plugin(clippy))]

// Clippy settings
// Most of the time much more readable
#![cfg_attr(feature="dev", allow(needless_range_loop))]
// Shorter than if-else
#![cfg_attr(feature="dev", allow(match_bool))]
// Keeps consistency (all lines with `.clone()`).
#![cfg_attr(feature="dev", allow(clone_on_copy))]
// Complains on Box<E> when implementing From<Box<E>>
#![cfg_attr(feature="dev", allow(boxed_local))]
// Complains about nested modules with same name as parent
#![cfg_attr(feature="dev", allow(module_inception))]
// TODO [todr] a lot of warnings to be fixed
#![cfg_attr(feature="dev", allow(assign_op_pattern))]


//! Ethcore library
//!
//! ### Rust version:
//! - nightly
//!
//! ### Supported platforms:
//! - OSX
//! - Linux
//!
//! ### Building:
//!
//! - Ubuntu 14.04 and later:
//!
//!   ```bash
//!
//!   # install rustup
//!   curl https://sh.rustup.rs -sSf | sh
//!
//!   # download and build parity
//!   git clone https://github.com/paritytech/parity
//!   cd parity
//!   cargo build --release
//!   ```
//!
//! - OSX:
//!
//!   ```bash
//!   # install rocksdb && rustup
//!   brew update
//!   curl https://sh.rustup.rs -sSf | sh
//!
//!   # download and build parity
//!   git clone https://github.com/paritytech/parity
//!   cd parity
//!   cargo build --release
//!   ```

extern crate crossbeam;
extern crate ethcore;
extern crate ethcore_io as io;
extern crate ethcore_util as util;
extern crate rlp;

#[macro_use]
extern crate log;


#[cfg(feature = "jit" )]
extern crate evmjit;

pub extern crate ethstore;

pub mod executive;

#[cfg(test)]
mod tests;
#[cfg(test)]
#[cfg(feature="json-tests")]
mod json_tests;

//pub use types::*;
pub use executive::contract_address;
pub use ethcore::evm::CreateContractAddress;
