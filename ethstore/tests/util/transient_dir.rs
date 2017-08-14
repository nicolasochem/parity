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

extern crate tempdir;
use ethstore::dir::{KeyDirectory, RootDiskDirectory};
use ethstore::{Error, SafeAccount};
use self::tempdir::TempDir;

pub struct TransientDir {
	dir: RootDiskDirectory,
	pub temp_dir: TempDir
}

impl TransientDir {
	pub fn create() -> Result<Self, Error> {
        let temp_dir = TempDir::new("").unwrap();
		let result = TransientDir {
			dir: RootDiskDirectory::at(&temp_dir),
			temp_dir: temp_dir
		};

		Ok(result)
	}

	pub fn open() -> Self {
        let temp_dir = TempDir::new("").unwrap();
		TransientDir {
			dir: RootDiskDirectory::at(&temp_dir),
                        temp_dir: temp_dir
		}
	}
}

impl KeyDirectory for TransientDir {
	fn load(&self) -> Result<Vec<SafeAccount>, Error> {
		self.dir.load()
	}

	fn update(&self, account: SafeAccount) -> Result<SafeAccount, Error> {
		self.dir.update(account)
	}

	fn insert(&self, account: SafeAccount) -> Result<SafeAccount, Error> {
		self.dir.insert(account)
	}

	fn remove(&self, account: &SafeAccount) -> Result<(), Error> {
		self.dir.remove(account)
	}

	fn unique_repr(&self) -> Result<u64, Error> {
		self.dir.unique_repr()
	}
}
