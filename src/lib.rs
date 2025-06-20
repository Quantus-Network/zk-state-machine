// This file is part of Substrate.

// Copyright (C) Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: Apache-2.0

// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// 	http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Substrate state machine implementation.

#![warn(missing_docs)]
#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

pub mod backend;
#[cfg(feature = "std")]
mod basic;
mod error;
mod ext;
#[cfg(feature = "fuzzing")]
pub mod fuzzing;
#[cfg(feature = "std")]
mod in_memory_backend;
pub(crate) mod overlayed_changes;
#[cfg(feature = "std")]
mod read_only;
mod stats;
#[cfg(feature = "std")]
mod testing;
mod trie_backend;
mod trie_backend_essence;

pub use trie_backend::TrieCacheProvider;

#[cfg(feature = "std")]
pub use std_reexport::*;

#[cfg(feature = "std")]
pub use execution::*;
#[cfg(feature = "std")]
pub use log::{debug, error as log_error, warn};
#[cfg(feature = "std")]
pub use tracing::trace;

/// In no_std we skip logs for state_machine, this macro
/// is a noops.
#[cfg(not(feature = "std"))]
#[macro_export]
macro_rules! warn {
	(target: $target:expr, $message:expr $( , $arg:ident )* $( , )?) => {
		{
			$(
				let _ = &$arg;
			)*
		}
	};
	($message:expr, $( $arg:expr, )*) => {
		{
			$(
				let _ = &$arg;
			)*
		}
	};
}

/// In no_std we skip logs for state_machine, this macro
/// is a noops.
#[cfg(not(feature = "std"))]
#[macro_export]
macro_rules! debug {
	(target: $target:expr, $message:expr $( , $arg:ident )* $( , )?) => {
		{
			$(
				let _ = &$arg;
			)*
		}
	};
}

/// In no_std we skip logs for state_machine, this macro
/// is a noops.
#[cfg(not(feature = "std"))]
#[macro_export]
macro_rules! trace {
	(target: $target:expr, $($arg:tt)+) => {
		()
	};
	($($arg:tt)+) => {
		()
	};
}

/// In no_std we skip logs for state_machine, this macro
/// is a noops.
#[cfg(not(feature = "std"))]
#[macro_export]
macro_rules! log_error {
	(target: $target:expr, $($arg:tt)+) => {
		()
	};
	($($arg:tt)+) => {
		()
	};
}

/// Default error type to use with state machine trie backend.
#[cfg(feature = "std")]
pub type DefaultError = String;
/// Error type to use with state machine trie backend.
#[cfg(not(feature = "std"))]
#[derive(Debug, Default, Clone, Copy, Eq, PartialEq)]
pub struct DefaultError;

#[cfg(not(feature = "std"))]
impl core::fmt::Display for DefaultError {
	fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
		write!(f, "DefaultError")
	}
}

pub use crate::{
	backend::{Backend, BackendTransaction, IterArgs, KeysIter, PairsIter, StorageIterator},
	error::{Error, ExecutionError},
	ext::Ext,
	overlayed_changes::{
		ChildStorageCollection, IndexOperation, OffchainChangesCollection,
		OffchainOverlayedChanges, OverlayedChanges, StorageChanges, StorageCollection, StorageKey,
		StorageValue,
	},
	stats::{StateMachineStats, UsageInfo, UsageUnit},
	trie_backend::{TrieBackend, TrieBackendBuilder},
	trie_backend_essence::{Storage, TrieBackendStorage},
};

#[cfg(feature = "std")]
mod std_reexport {
	pub use crate::{
		basic::BasicExternalities,
		in_memory_backend::new_in_mem,
		read_only::{InspectState, ReadOnlyExternalities},
		testing::TestExternalities,
		trie_backend::create_proof_check_backend,
	};
	pub use sp_trie::{
		trie_types::{TrieDBMutV0, TrieDBMutV1},
		CompactProof, DBValue, LayoutV0, LayoutV1, MemoryDB, StorageProof, TrieMut,
	};
}

#[cfg(feature = "std")]
mod execution {
	use crate::backend::AsTrieBackend;

	use super::*;
	use codec::Codec;
	use hash_db::Hasher;
	use smallvec::SmallVec;
	use sp_core::{
		hexdisplay::HexDisplay,
		storage::{ChildInfo, ChildType, PrefixedStorageKey},
		traits::{CallContext, CodeExecutor, RuntimeCode},
	};
	use sp_externalities::Extensions;
	use sp_trie::PrefixedMemoryDB;
	use std::collections::{HashMap, HashSet};

	pub(crate) type CallResult<E> = Result<Vec<u8>, E>;

	/// Default handler of the execution manager.
	pub type DefaultHandler<E> = fn(CallResult<E>, CallResult<E>) -> CallResult<E>;

	/// Trie backend with in-memory storage.
	pub type InMemoryBackend<H> = TrieBackend<PrefixedMemoryDB<H>, H>;

	/// Storage backend trust level.
	#[derive(Debug, Clone)]
	pub enum BackendTrustLevel {
		/// Panics from trusted backends are considered justified, and never caught.
		Trusted,
		/// Panics from untrusted backend are caught and interpreted as runtime error.
		/// Untrusted backend may be missing some parts of the trie, so panics are not considered
		/// fatal.
		Untrusted,
	}

	/// The substrate state machine.
	pub struct StateMachine<'a, B, H, Exec>
	where
		H: Hasher,
		B: Backend<H>,
	{
		backend: &'a B,
		exec: &'a Exec,
		method: &'a str,
		call_data: &'a [u8],
		overlay: &'a mut OverlayedChanges<H>,
		extensions: &'a mut Extensions,
		runtime_code: &'a RuntimeCode<'a>,
		stats: StateMachineStats,
		/// The hash of the block the state machine will be executed on.
		///
		/// Used for logging.
		parent_hash: Option<H::Out>,
		context: CallContext,
	}

	impl<'a, B, H, Exec> Drop for StateMachine<'a, B, H, Exec>
	where
		H: Hasher,
		B: Backend<H>,
	{
		fn drop(&mut self) {
			self.backend.register_overlay_stats(&self.stats);
		}
	}

	impl<'a, B, H, Exec> StateMachine<'a, B, H, Exec>
	where
		H: Hasher,
		H::Out: Ord + 'static + codec::Codec,
		Exec: CodeExecutor + Clone + 'static,
		B: Backend<H>,
	{
		/// Creates new substrate state machine.
		pub fn new(
			backend: &'a B,
			overlay: &'a mut OverlayedChanges<H>,
			exec: &'a Exec,
			method: &'a str,
			call_data: &'a [u8],
			extensions: &'a mut Extensions,
			runtime_code: &'a RuntimeCode,
			context: CallContext,
		) -> Self {
			Self {
				backend,
				exec,
				method,
				call_data,
				extensions,
				overlay,
				runtime_code,
				stats: StateMachineStats::default(),
				parent_hash: None,
				context,
			}
		}

		/// Set the given `parent_hash` as the hash of the parent block.
		///
		/// This will be used for improved logging.
		pub fn set_parent_hash(mut self, parent_hash: H::Out) -> Self {
			self.parent_hash = Some(parent_hash);
			self
		}

		/// Execute a call using the given state backend, overlayed changes, and call executor.
		///
		/// On an error, no prospective changes are written to the overlay.
		///
		/// Note: changes to code will be in place if this call is made again. For running partial
		/// blocks (e.g. a transaction at a time), ensure a different method is used.
		///
		/// Returns the SCALE encoded result of the executed function.
		pub fn execute(&mut self) -> Result<Vec<u8>, Box<dyn Error>> {
			self.overlay
				.enter_runtime()
				.expect("StateMachine is never called from the runtime; qed");

			let mut ext = Ext::new(self.overlay, self.backend, Some(self.extensions));

			let ext_id = ext.id;

			trace!(
				target: "state",
				ext_id = %HexDisplay::from(&ext_id.to_le_bytes()),
				method = %self.method,
				parent_hash = %self.parent_hash.map(|h| format!("{:?}", h)).unwrap_or_else(|| String::from("None")),
				input = ?HexDisplay::from(&self.call_data),
				"Call",
			);

			let result = self
				.exec
				.call(&mut ext, self.runtime_code, self.method, self.call_data, self.context)
				.0;

			self.overlay
				.exit_runtime()
				.expect("Runtime is not able to call this function in the overlay; qed");

			trace!(
				target: "state",
				ext_id = %HexDisplay::from(&ext_id.to_le_bytes()),
				?result,
				"Return",
			);

			result.map_err(|e| Box::new(e) as Box<_>)
		}
	}

	/// Prove execution using the given state backend, overlayed changes, and call executor.
	pub fn prove_execution<B, H, Exec>(
		backend: &mut B,
		overlay: &mut OverlayedChanges<H>,
		exec: &Exec,
		method: &str,
		call_data: &[u8],
		runtime_code: &RuntimeCode,
	) -> Result<(Vec<u8>, StorageProof), Box<dyn Error>>
	where
		B: AsTrieBackend<H>,
		H: Hasher,
		H::Out: Ord + 'static + codec::Codec,
		Exec: CodeExecutor + Clone + 'static,
	{
		let trie_backend = backend.as_trie_backend();
		prove_execution_on_trie_backend::<_, _, _>(
			trie_backend,
			overlay,
			exec,
			method,
			call_data,
			runtime_code,
			&mut Default::default(),
		)
	}

	/// Prove execution using the given trie backend, overlayed changes, and call executor.
	/// Produces a state-backend-specific "transaction" which can be used to apply the changes
	/// to the backing store, such as the disk.
	/// Execution proof is the set of all 'touched' storage DBValues from the backend.
	///
	/// On an error, no prospective changes are written to the overlay.
	///
	/// Note: changes to code will be in place if this call is made again. For running partial
	/// blocks (e.g. a transaction at a time), ensure a different method is used.
	pub fn prove_execution_on_trie_backend<S, H, Exec>(
		trie_backend: &TrieBackend<S, H>,
		overlay: &mut OverlayedChanges<H>,
		exec: &Exec,
		method: &str,
		call_data: &[u8],
		runtime_code: &RuntimeCode,
		extensions: &mut Extensions,
	) -> Result<(Vec<u8>, StorageProof), Box<dyn Error>>
	where
		S: trie_backend_essence::TrieBackendStorage<H>,
		H: Hasher,
		H::Out: Ord + 'static + codec::Codec,
		Exec: CodeExecutor + 'static + Clone,
	{
		let proving_backend =
			TrieBackendBuilder::wrap(trie_backend).with_recorder(Default::default()).build();

		let result = StateMachine::<_, H, Exec>::new(
			&proving_backend,
			overlay,
			exec,
			method,
			call_data,
			extensions,
			runtime_code,
			CallContext::Offchain,
		)
		.execute()?;

		let proof = proving_backend
			.extract_proof()
			.expect("A recorder was set and thus, a storage proof can be extracted; qed");

		Ok((result, proof))
	}

	/// Check execution proof, generated by `prove_execution` call.
	pub fn execution_proof_check<H, Exec>(
		root: H::Out,
		proof: StorageProof,
		overlay: &mut OverlayedChanges<H>,
		exec: &Exec,
		method: &str,
		call_data: &[u8],
		runtime_code: &RuntimeCode,
	) -> Result<Vec<u8>, Box<dyn Error>>
	where
		H: Hasher + 'static,
		Exec: CodeExecutor + Clone + 'static,
		H::Out: Ord + 'static + codec::Codec,
	{
		let trie_backend = create_proof_check_backend::<H>(root, proof)?;
		execution_proof_check_on_trie_backend::<_, _>(
			&trie_backend,
			overlay,
			exec,
			method,
			call_data,
			runtime_code,
		)
	}

	/// Check execution proof on proving backend, generated by `prove_execution` call.
	pub fn execution_proof_check_on_trie_backend<H, Exec>(
		trie_backend: &TrieBackend<MemoryDB<H>, H>,
		overlay: &mut OverlayedChanges<H>,
		exec: &Exec,
		method: &str,
		call_data: &[u8],
		runtime_code: &RuntimeCode,
	) -> Result<Vec<u8>, Box<dyn Error>>
	where
		H: Hasher,
		H::Out: Ord + 'static + codec::Codec,
		Exec: CodeExecutor + Clone + 'static,
	{
		StateMachine::<_, H, Exec>::new(
			trie_backend,
			overlay,
			exec,
			method,
			call_data,
			&mut Extensions::default(),
			runtime_code,
			CallContext::Offchain,
		)
		.execute()
	}

	/// Generate storage read proof.
	pub fn prove_read<B, H, I>(backend: B, keys: I) -> Result<StorageProof, Box<dyn Error>>
	where
		B: AsTrieBackend<H>,
		H: Hasher,
		H::Out: Ord + Codec,
		I: IntoIterator,
		I::Item: AsRef<[u8]>,
	{
		let trie_backend = backend.as_trie_backend();
		prove_read_on_trie_backend(trie_backend, keys)
	}

	/// State machine only allows a single level
	/// of child trie.
	pub const MAX_NESTED_TRIE_DEPTH: usize = 2;

	/// Multiple key value state.
	/// States are ordered by root storage key.
	#[derive(PartialEq, Eq, Clone)]
	pub struct KeyValueStates(pub Vec<KeyValueStorageLevel>);

	/// A key value state at any storage level.
	#[derive(PartialEq, Eq, Clone)]
	pub struct KeyValueStorageLevel {
		/// State root of the level, for
		/// top trie it is as an empty byte array.
		pub state_root: Vec<u8>,
		/// Storage of parents, empty for top root or
		/// when exporting (building proof).
		pub parent_storage_keys: Vec<Vec<u8>>,
		/// Pair of key and values from this state.
		pub key_values: Vec<(Vec<u8>, Vec<u8>)>,
	}

	impl<I> From<I> for KeyValueStates
	where
		I: IntoIterator<Item = (Vec<u8>, (Vec<(Vec<u8>, Vec<u8>)>, Vec<Vec<u8>>))>,
	{
		fn from(b: I) -> Self {
			let mut result = Vec::new();
			for (state_root, (key_values, storage_paths)) in b.into_iter() {
				result.push(KeyValueStorageLevel {
					state_root,
					key_values,
					parent_storage_keys: storage_paths,
				})
			}
			KeyValueStates(result)
		}
	}

	impl KeyValueStates {
		/// Return total number of key values in states.
		pub fn len(&self) -> usize {
			self.0.iter().fold(0, |nb, state| nb + state.key_values.len())
		}

		/// Update last keys accessed from this state.
		pub fn update_last_key(
			&self,
			stopped_at: usize,
			last: &mut SmallVec<[Vec<u8>; 2]>,
		) -> bool {
			if stopped_at == 0 || stopped_at > MAX_NESTED_TRIE_DEPTH {
				return false
			}
			match stopped_at {
				1 => {
					let top_last =
						self.0.get(0).and_then(|s| s.key_values.last().map(|kv| kv.0.clone()));
					if let Some(top_last) = top_last {
						match last.len() {
							0 => {
								last.push(top_last);
								return true
							},
							2 => {
								last.pop();
							},
							_ => (),
						}
						// update top trie access.
						last[0] = top_last;
						return true
					} else {
						// No change in top trie accesses.
						// Indicates end of reading of a child trie.
						last.truncate(1);
						return true
					}
				},
				2 => {
					let top_last =
						self.0.get(0).and_then(|s| s.key_values.last().map(|kv| kv.0.clone()));
					let child_last =
						self.0.last().and_then(|s| s.key_values.last().map(|kv| kv.0.clone()));

					if let Some(child_last) = child_last {
						if last.is_empty() {
							if let Some(top_last) = top_last {
								last.push(top_last)
							} else {
								return false
							}
						} else if let Some(top_last) = top_last {
							last[0] = top_last;
						}
						if last.len() == 2 {
							last.pop();
						}
						last.push(child_last);
						return true
					} else {
						// stopped at level 2 so child last is define.
						return false
					}
				},
				_ => (),
			}
			false
		}
	}

	/// Generate range storage read proof, with child tries
	/// content.
	/// A size limit is applied to the proof with the
	/// exception that `start_at` and its following element
	/// are always part of the proof.
	/// If a key different than `start_at` is a child trie root,
	/// the child trie content will be included in the proof.
	pub fn prove_range_read_with_child_with_size<B, H>(
		backend: B,
		size_limit: usize,
		start_at: &[Vec<u8>],
	) -> Result<(StorageProof, u32), Box<dyn Error>>
	where
		B: AsTrieBackend<H>,
		H: Hasher,
		H::Out: Ord + Codec,
	{
		let trie_backend = backend.as_trie_backend();
		prove_range_read_with_child_with_size_on_trie_backend(trie_backend, size_limit, start_at)
	}

	/// Generate range storage read proof, with child tries
	/// content.
	/// See `prove_range_read_with_child_with_size`.
	pub fn prove_range_read_with_child_with_size_on_trie_backend<S, H>(
		trie_backend: &TrieBackend<S, H>,
		size_limit: usize,
		start_at: &[Vec<u8>],
	) -> Result<(StorageProof, u32), Box<dyn Error>>
	where
		S: trie_backend_essence::TrieBackendStorage<H>,
		H: Hasher,
		H::Out: Ord + Codec,
	{
		if start_at.len() > MAX_NESTED_TRIE_DEPTH {
			return Err(Box::new("Invalid start of range."))
		}

		let recorder = sp_trie::recorder::Recorder::default();
		let proving_backend =
			TrieBackendBuilder::wrap(trie_backend).with_recorder(recorder.clone()).build();
		let mut count = 0;

		let mut child_roots = HashSet::new();
		let (mut child_key, mut start_at) = if start_at.len() == 2 {
			let storage_key = start_at.get(0).expect("Checked length.").clone();
			if let Some(state_root) = proving_backend
				.storage(&storage_key)
				.map_err(|e| Box::new(e) as Box<dyn Error>)?
			{
				child_roots.insert(state_root);
			} else {
				return Err(Box::new("Invalid range start child trie key."))
			}

			(Some(storage_key), start_at.get(1).cloned())
		} else {
			(None, start_at.get(0).cloned())
		};

		loop {
			let (child_info, depth) = if let Some(storage_key) = child_key.as_ref() {
				let storage_key = PrefixedStorageKey::new_ref(storage_key);
				(
					Some(match ChildType::from_prefixed_key(storage_key) {
						Some((ChildType::ParentKeyId, storage_key)) =>
							ChildInfo::new_default(storage_key),
						None => return Err(Box::new("Invalid range start child trie key.")),
					}),
					2,
				)
			} else {
				(None, 1)
			};

			let start_at_ref = start_at.as_ref().map(AsRef::as_ref);
			let mut switch_child_key = None;
			let mut iter = proving_backend
				.pairs(IterArgs {
					child_info,
					start_at: start_at_ref,
					start_at_exclusive: true,
					..IterArgs::default()
				})
				.map_err(|e| Box::new(e) as Box<dyn Error>)?;

			while let Some(item) = iter.next() {
				let (key, value) = item.map_err(|e| Box::new(e) as Box<dyn Error>)?;

				if depth < MAX_NESTED_TRIE_DEPTH &&
					sp_core::storage::well_known_keys::is_child_storage_key(key.as_slice())
				{
					count += 1;
					// do not add two child trie with same root
					if !child_roots.contains(value.as_slice()) {
						child_roots.insert(value);
						switch_child_key = Some(key);
						break
					}
				} else if recorder.estimate_encoded_size() <= size_limit {
					count += 1;
				} else {
					break
				}
			}

			let completed = iter.was_complete();

			if switch_child_key.is_none() {
				if depth == 1 {
					break
				} else if completed {
					start_at = child_key.take();
				} else {
					break
				}
			} else {
				child_key = switch_child_key;
				start_at = None;
			}
		}

		let proof = proving_backend
			.extract_proof()
			.expect("A recorder was set and thus, a storage proof can be extracted; qed");
		Ok((proof, count))
	}

	/// Generate range storage read proof.
	pub fn prove_range_read_with_size<B, H>(
		backend: B,
		child_info: Option<&ChildInfo>,
		prefix: Option<&[u8]>,
		size_limit: usize,
		start_at: Option<&[u8]>,
	) -> Result<(StorageProof, u32), Box<dyn Error>>
	where
		B: AsTrieBackend<H>,
		H: Hasher,
		H::Out: Ord + Codec,
	{
		let trie_backend = backend.as_trie_backend();
		prove_range_read_with_size_on_trie_backend(
			trie_backend,
			child_info,
			prefix,
			size_limit,
			start_at,
		)
	}

	/// Generate range storage read proof on an existing trie backend.
	pub fn prove_range_read_with_size_on_trie_backend<S, H>(
		trie_backend: &TrieBackend<S, H>,
		child_info: Option<&ChildInfo>,
		prefix: Option<&[u8]>,
		size_limit: usize,
		start_at: Option<&[u8]>,
	) -> Result<(StorageProof, u32), Box<dyn Error>>
	where
		S: trie_backend_essence::TrieBackendStorage<H>,
		H: Hasher,
		H::Out: Ord + Codec,
	{
		let recorder = sp_trie::recorder::Recorder::default();
		let proving_backend =
			TrieBackendBuilder::wrap(trie_backend).with_recorder(recorder.clone()).build();
		let mut count = 0;
		let iter = proving_backend
			// NOTE: Even though the loop below doesn't use these values
			//       this *must* fetch both the keys and the values so that
			//       the proof is correct.
			.pairs(IterArgs {
				child_info: child_info.cloned(),
				prefix,
				start_at,
				..IterArgs::default()
			})
			.map_err(|e| Box::new(e) as Box<dyn Error>)?;

		for item in iter {
			item.map_err(|e| Box::new(e) as Box<dyn Error>)?;
			if count == 0 || recorder.estimate_encoded_size() <= size_limit {
				count += 1;
			} else {
				break
			}
		}

		let proof = proving_backend
			.extract_proof()
			.expect("A recorder was set and thus, a storage proof can be extracted; qed");
		Ok((proof, count))
	}

	/// Generate child storage read proof.
	pub fn prove_child_read<B, H, I>(
		backend: B,
		child_info: &ChildInfo,
		keys: I,
	) -> Result<StorageProof, Box<dyn Error>>
	where
		B: AsTrieBackend<H>,
		H: Hasher,
		H::Out: Ord + Codec,
		I: IntoIterator,
		I::Item: AsRef<[u8]>,
	{
		let trie_backend = backend.as_trie_backend();
		prove_child_read_on_trie_backend(trie_backend, child_info, keys)
	}

	/// Generate storage read proof on pre-created trie backend.
	pub fn prove_read_on_trie_backend<S, H, I>(
		trie_backend: &TrieBackend<S, H>,
		keys: I,
	) -> Result<StorageProof, Box<dyn Error>>
	where
		S: trie_backend_essence::TrieBackendStorage<H>,
		H: Hasher,
		H::Out: Ord + Codec,
		I: IntoIterator,
		I::Item: AsRef<[u8]>,
	{
		let proving_backend =
			TrieBackendBuilder::wrap(trie_backend).with_recorder(Default::default()).build();
		for key in keys.into_iter() {
			proving_backend
				.storage(key.as_ref())
				.map_err(|e| Box::new(e) as Box<dyn Error>)?;
		}

		Ok(proving_backend
			.extract_proof()
			.expect("A recorder was set and thus, a storage proof can be extracted; qed"))
	}

	/// Generate storage read proof on pre-created trie backend.
	pub fn prove_child_read_on_trie_backend<S, H, I>(
		trie_backend: &TrieBackend<S, H>,
		child_info: &ChildInfo,
		keys: I,
	) -> Result<StorageProof, Box<dyn Error>>
	where
		S: trie_backend_essence::TrieBackendStorage<H>,
		H: Hasher,
		H::Out: Ord + Codec,
		I: IntoIterator,
		I::Item: AsRef<[u8]>,
	{
		let proving_backend =
			TrieBackendBuilder::wrap(trie_backend).with_recorder(Default::default()).build();
		for key in keys.into_iter() {
			proving_backend
				.child_storage(child_info, key.as_ref())
				.map_err(|e| Box::new(e) as Box<dyn Error>)?;
		}

		Ok(proving_backend
			.extract_proof()
			.expect("A recorder was set and thus, a storage proof can be extracted; qed"))
	}

	/// Check storage read proof, generated by `prove_read` call.
	pub fn read_proof_check<H, I>(
		root: H::Out,
		proof: StorageProof,
		keys: I,
	) -> Result<HashMap<Vec<u8>, Option<Vec<u8>>>, Box<dyn Error>>
	where
		H: Hasher + 'static,
		H::Out: Ord + Codec,
		I: IntoIterator,
		I::Item: AsRef<[u8]>,
	{
		let proving_backend = create_proof_check_backend::<H>(root, proof)?;
		let mut result = HashMap::new();
		for key in keys.into_iter() {
			let value = read_proof_check_on_proving_backend(&proving_backend, key.as_ref())?;
			result.insert(key.as_ref().to_vec(), value);
		}
		Ok(result)
	}

	/// Check storage range proof with child trie included, generated by
	/// `prove_range_read_with_child_with_size` call.
	///
	/// Returns key values contents and the depth of the pending state iteration
	/// (0 if completed).
	pub fn read_range_proof_check_with_child<H>(
		root: H::Out,
		proof: StorageProof,
		start_at: &[Vec<u8>],
	) -> Result<(KeyValueStates, usize), Box<dyn Error>>
	where
		H: Hasher + 'static,
		H::Out: Ord + Codec,
	{
		let proving_backend = create_proof_check_backend::<H>(root, proof)?;
		read_range_proof_check_with_child_on_proving_backend(&proving_backend, start_at)
	}

	/// Check child storage range proof, generated by `prove_range_read_with_size` call.
	pub fn read_range_proof_check<H>(
		root: H::Out,
		proof: StorageProof,
		child_info: Option<&ChildInfo>,
		prefix: Option<&[u8]>,
		count: Option<u32>,
		start_at: Option<&[u8]>,
	) -> Result<(Vec<(Vec<u8>, Vec<u8>)>, bool), Box<dyn Error>>
	where
		H: Hasher + 'static,
		H::Out: Ord + Codec,
	{
		let proving_backend = create_proof_check_backend::<H>(root, proof)?;
		read_range_proof_check_on_proving_backend(
			&proving_backend,
			child_info,
			prefix,
			count,
			start_at,
		)
	}

	/// Check child storage read proof, generated by `prove_child_read` call.
	pub fn read_child_proof_check<H, I>(
		root: H::Out,
		proof: StorageProof,
		child_info: &ChildInfo,
		keys: I,
	) -> Result<HashMap<Vec<u8>, Option<Vec<u8>>>, Box<dyn Error>>
	where
		H: Hasher + 'static,
		H::Out: Ord + Codec,
		I: IntoIterator,
		I::Item: AsRef<[u8]>,
	{
		let proving_backend = create_proof_check_backend::<H>(root, proof)?;
		let mut result = HashMap::new();
		for key in keys.into_iter() {
			let value = read_child_proof_check_on_proving_backend(
				&proving_backend,
				child_info,
				key.as_ref(),
			)?;
			result.insert(key.as_ref().to_vec(), value);
		}
		Ok(result)
	}

	/// Check storage read proof on pre-created proving backend.
	pub fn read_proof_check_on_proving_backend<H>(
		proving_backend: &TrieBackend<MemoryDB<H>, H>,
		key: &[u8],
	) -> Result<Option<Vec<u8>>, Box<dyn Error>>
	where
		H: Hasher,
		H::Out: Ord + Codec,
	{
		proving_backend.storage(key).map_err(|e| Box::new(e) as Box<dyn Error>)
	}

	/// Check child storage read proof on pre-created proving backend.
	pub fn read_child_proof_check_on_proving_backend<H>(
		proving_backend: &TrieBackend<MemoryDB<H>, H>,
		child_info: &ChildInfo,
		key: &[u8],
	) -> Result<Option<Vec<u8>>, Box<dyn Error>>
	where
		H: Hasher,
		H::Out: Ord + Codec,
	{
		proving_backend
			.child_storage(child_info, key)
			.map_err(|e| Box::new(e) as Box<dyn Error>)
	}

	/// Check storage range proof on pre-created proving backend.
	///
	/// Returns a vector with the read `key => value` pairs and a `bool` that is set to `true` when
	/// all `key => value` pairs could be read and no more are left.
	pub fn read_range_proof_check_on_proving_backend<H>(
		proving_backend: &TrieBackend<MemoryDB<H>, H>,
		child_info: Option<&ChildInfo>,
		prefix: Option<&[u8]>,
		count: Option<u32>,
		start_at: Option<&[u8]>,
	) -> Result<(Vec<(Vec<u8>, Vec<u8>)>, bool), Box<dyn Error>>
	where
		H: Hasher,
		H::Out: Ord + Codec,
	{
		let mut values = Vec::new();
		let mut iter = proving_backend
			.pairs(IterArgs {
				child_info: child_info.cloned(),
				prefix,
				start_at,
				stop_on_incomplete_database: true,
				..IterArgs::default()
			})
			.map_err(|e| Box::new(e) as Box<dyn Error>)?;

		while let Some(item) = iter.next() {
			let (key, value) = item.map_err(|e| Box::new(e) as Box<dyn Error>)?;
			values.push((key, value));
			if !count.as_ref().map_or(true, |c| (values.len() as u32) < *c) {
				break
			}
		}

		Ok((values, iter.was_complete()))
	}

	/// Check storage range proof on pre-created proving backend.
	///
	/// See `read_range_proof_check_with_child`.
	pub fn read_range_proof_check_with_child_on_proving_backend<H>(
		proving_backend: &TrieBackend<MemoryDB<H>, H>,
		start_at: &[Vec<u8>],
	) -> Result<(KeyValueStates, usize), Box<dyn Error>>
	where
		H: Hasher,
		H::Out: Ord + Codec,
	{
		let mut result = vec![KeyValueStorageLevel {
			state_root: Default::default(),
			key_values: Default::default(),
			parent_storage_keys: Default::default(),
		}];
		if start_at.len() > MAX_NESTED_TRIE_DEPTH {
			return Err(Box::new("Invalid start of range."))
		}

		let mut child_roots = HashSet::new();
		let (mut child_key, mut start_at) = if start_at.len() == 2 {
			let storage_key = start_at.get(0).expect("Checked length.").clone();
			let child_key = if let Some(state_root) = proving_backend
				.storage(&storage_key)
				.map_err(|e| Box::new(e) as Box<dyn Error>)?
			{
				child_roots.insert(state_root.clone());
				Some((storage_key, state_root))
			} else {
				return Err(Box::new("Invalid range start child trie key."))
			};

			(child_key, start_at.get(1).cloned())
		} else {
			(None, start_at.get(0).cloned())
		};

		let completed = loop {
			let (child_info, depth) = if let Some((storage_key, state_root)) = child_key.as_ref() {
				result.push(KeyValueStorageLevel {
					state_root: state_root.clone(),
					key_values: Default::default(),
					parent_storage_keys: Default::default(),
				});

				let storage_key = PrefixedStorageKey::new_ref(storage_key);
				(
					Some(match ChildType::from_prefixed_key(storage_key) {
						Some((ChildType::ParentKeyId, storage_key)) =>
							ChildInfo::new_default(storage_key),
						None => return Err(Box::new("Invalid range start child trie key.")),
					}),
					2,
				)
			} else {
				(None, 1)
			};

			let values = if child_info.is_some() {
				&mut result.last_mut().expect("Added above").key_values
			} else {
				&mut result[0].key_values
			};
			let start_at_ref = start_at.as_ref().map(AsRef::as_ref);
			let mut switch_child_key = None;

			let mut iter = proving_backend
				.pairs(IterArgs {
					child_info,
					start_at: start_at_ref,
					start_at_exclusive: true,
					stop_on_incomplete_database: true,
					..IterArgs::default()
				})
				.map_err(|e| Box::new(e) as Box<dyn Error>)?;

			while let Some(item) = iter.next() {
				let (key, value) = item.map_err(|e| Box::new(e) as Box<dyn Error>)?;
				values.push((key.to_vec(), value.to_vec()));

				if depth < MAX_NESTED_TRIE_DEPTH &&
					sp_core::storage::well_known_keys::is_child_storage_key(key.as_slice())
				{
					// Do not add two chid trie with same root.
					if !child_roots.contains(value.as_slice()) {
						child_roots.insert(value.clone());
						switch_child_key = Some((key, value));
						break
					}
				}
			}

			let completed = iter.was_complete();

			if switch_child_key.is_none() {
				if !completed {
					break depth
				}
				if depth == 1 {
					break 0
				} else {
					start_at = child_key.take().map(|entry| entry.0);
				}
			} else {
				child_key = switch_child_key;
				start_at = None;
			}
		};
		Ok((KeyValueStates(result), completed))
	}
}

#[cfg(test)]
mod tests {
	use super::{backend::AsTrieBackend, ext::Ext, *};
	use crate::{execution::CallResult, in_memory_backend::new_in_mem};
	use assert_matches::assert_matches;
	use codec::Encode;
	use sp_core::{
		map,
		storage::{ChildInfo, StateVersion},
		traits::{CallContext, CodeExecutor, Externalities, RuntimeCode},
		H256,
	};
	use sp_runtime::traits::BlakeTwo256;
	use sp_trie::{
		trie_types::{TrieDBMutBuilderV0, TrieDBMutBuilderV1},
		KeySpacedDBMut, PrefixedMemoryDB,
	};
	use std::collections::{BTreeMap, HashMap};

	#[derive(Clone)]
	struct DummyCodeExecutor {
		native_available: bool,
		native_succeeds: bool,
		fallback_succeeds: bool,
	}

	impl CodeExecutor for DummyCodeExecutor {
		type Error = u8;

		fn call(
			&self,
			ext: &mut dyn Externalities,
			_: &RuntimeCode,
			_method: &str,
			_data: &[u8],
			_: CallContext,
		) -> (CallResult<Self::Error>, bool) {
			let using_native = self.native_available;
			match (using_native, self.native_succeeds, self.fallback_succeeds) {
				(true, true, _) | (false, _, true) => (
					Ok(vec![
						ext.storage(b"value1").unwrap()[0] + ext.storage(b"value2").unwrap()[0],
					]),
					using_native,
				),
				_ => (Err(0), using_native),
			}
		}
	}

	impl sp_core::traits::ReadRuntimeVersion for DummyCodeExecutor {
		fn read_runtime_version(
			&self,
			_: &[u8],
			_: &mut dyn Externalities,
		) -> std::result::Result<Vec<u8>, String> {
			unimplemented!("Not required in tests.")
		}
	}

	#[test]
	fn execute_works() {
		execute_works_inner(StateVersion::V0);
		execute_works_inner(StateVersion::V1);
	}
	fn execute_works_inner(state_version: StateVersion) {
		let backend = trie_backend::tests::test_trie(state_version, None, None);
		let mut overlayed_changes = Default::default();
		let wasm_code = RuntimeCode::empty();
		let mut execution_extensions = &mut Default::default();

		let mut state_machine = StateMachine::new(
			&backend,
			&mut overlayed_changes,
			&DummyCodeExecutor {
				native_available: true,
				native_succeeds: true,
				fallback_succeeds: true,
			},
			"test",
			&[],
			&mut execution_extensions,
			&wasm_code,
			CallContext::Offchain,
		);

		assert_eq!(state_machine.execute().unwrap(), vec![66]);
	}

	#[test]
	fn execute_works_with_native_else_wasm() {
		execute_works_with_native_else_wasm_inner(StateVersion::V0);
		execute_works_with_native_else_wasm_inner(StateVersion::V1);
	}
	fn execute_works_with_native_else_wasm_inner(state_version: StateVersion) {
		let backend = trie_backend::tests::test_trie(state_version, None, None);
		let mut overlayed_changes = Default::default();
		let wasm_code = RuntimeCode::empty();
		let mut execution_extensions = &mut Default::default();

		let mut state_machine = StateMachine::new(
			&backend,
			&mut overlayed_changes,
			&DummyCodeExecutor {
				native_available: true,
				native_succeeds: true,
				fallback_succeeds: true,
			},
			"test",
			&[],
			&mut execution_extensions,
			&wasm_code,
			CallContext::Offchain,
		);

		assert_eq!(state_machine.execute().unwrap(), vec![66]);
	}

	#[test]
	fn prove_execution_and_proof_check_works() {
		prove_execution_and_proof_check_works_inner(StateVersion::V0);
		prove_execution_and_proof_check_works_inner(StateVersion::V1);
	}
	fn prove_execution_and_proof_check_works_inner(state_version: StateVersion) {
		let executor = DummyCodeExecutor {
			native_available: true,
			native_succeeds: true,
			fallback_succeeds: true,
		};

		// fetch execution proof from 'remote' full node
		let mut remote_backend = trie_backend::tests::test_trie(state_version, None, None);
		let remote_root = remote_backend.storage_root(std::iter::empty(), state_version).0;
		let (remote_result, remote_proof) = prove_execution(
			&mut remote_backend,
			&mut Default::default(),
			&executor,
			"test",
			&[],
			&RuntimeCode::empty(),
		)
		.unwrap();

		// check proof locally
		let local_result = execution_proof_check::<BlakeTwo256, _>(
			remote_root,
			remote_proof,
			&mut Default::default(),
			&executor,
			"test",
			&[],
			&RuntimeCode::empty(),
		)
		.unwrap();

		// check that both results are correct
		assert_eq!(remote_result, vec![66]);
		assert_eq!(remote_result, local_result);
	}

	#[test]
	fn clear_prefix_in_ext_works() {
		let initial: BTreeMap<_, _> = map![
			b"aaa".to_vec() => b"0".to_vec(),
			b"abb".to_vec() => b"1".to_vec(),
			b"abc".to_vec() => b"2".to_vec(),
			b"bbb".to_vec() => b"3".to_vec()
		];
		let state = InMemoryBackend::<BlakeTwo256>::from((initial, StateVersion::default()));
		let backend = state.as_trie_backend();

		let mut overlay = OverlayedChanges::default();
		overlay.set_storage(b"aba".to_vec(), Some(b"1312".to_vec()));
		overlay.set_storage(b"bab".to_vec(), Some(b"228".to_vec()));
		overlay.start_transaction();
		overlay.set_storage(b"abd".to_vec(), Some(b"69".to_vec()));
		overlay.set_storage(b"bbd".to_vec(), Some(b"42".to_vec()));

		let overlay_limit = overlay.clone();
		{
			let mut ext = Ext::new(&mut overlay, backend, None);
			let _ = ext.clear_prefix(b"ab", None, None);
		}
		overlay.commit_transaction().unwrap();

		assert_eq!(
			overlay
				.changes_mut()
				.map(|(k, v)| (k.clone(), v.value().cloned()))
				.collect::<HashMap<_, _>>(),
			map![
				b"abc".to_vec() => None,
				b"abb".to_vec() => None,
				b"aba".to_vec() => None,
				b"abd".to_vec() => None,

				b"bab".to_vec() => Some(b"228".to_vec()),
				b"bbd".to_vec() => Some(b"42".to_vec())
			],
		);

		let mut overlay = overlay_limit;
		{
			let mut ext = Ext::new(&mut overlay, backend, None);
			assert_matches!(
				ext.clear_prefix(b"ab", Some(1), None).deconstruct(),
				(Some(_), 1, 3, 1)
			);
		}
		overlay.commit_transaction().unwrap();

		assert_eq!(
			overlay
				.changes_mut()
				.map(|(k, v)| (k.clone(), v.value().cloned()))
				.collect::<HashMap<_, _>>(),
			map![
				b"abb".to_vec() => None,
				b"aba".to_vec() => None,
				b"abd".to_vec() => None,

				b"bab".to_vec() => Some(b"228".to_vec()),
				b"bbd".to_vec() => Some(b"42".to_vec())
			],
		);
	}

	#[test]
	fn limited_child_kill_works() {
		let child_info = ChildInfo::new_default(b"sub1");
		let initial: HashMap<_, BTreeMap<_, _>> = map![
			Some(child_info.clone()) => map![
				b"a".to_vec() => b"0".to_vec(),
				b"b".to_vec() => b"1".to_vec(),
				b"c".to_vec() => b"2".to_vec(),
				b"d".to_vec() => b"3".to_vec()
			],
		];
		let backend = InMemoryBackend::<BlakeTwo256>::from((initial, StateVersion::default()));

		let mut overlay = OverlayedChanges::default();
		overlay.set_child_storage(&child_info, b"1".to_vec(), Some(b"1312".to_vec()));
		overlay.set_child_storage(&child_info, b"2".to_vec(), Some(b"1312".to_vec()));
		overlay.set_child_storage(&child_info, b"3".to_vec(), Some(b"1312".to_vec()));
		overlay.set_child_storage(&child_info, b"4".to_vec(), Some(b"1312".to_vec()));

		{
			let mut ext = Ext::new(&mut overlay, &backend, None);
			let r = ext.kill_child_storage(&child_info, Some(2), None);
			assert_matches!(r.deconstruct(), (Some(_), 2, 6, 2));
		}

		assert_eq!(
			overlay
				.children_mut()
				.flat_map(|(iter, _child_info)| iter)
				.map(|(k, v)| (k.clone(), v.value()))
				.collect::<BTreeMap<_, _>>(),
			map![
				b"1".to_vec() => None,
				b"2".to_vec() => None,
				b"3".to_vec() => None,
				b"4".to_vec() => None,
				b"a".to_vec() => None,
				b"b".to_vec() => None,
			],
		);
	}

	#[test]
	fn limited_child_kill_off_by_one_works() {
		let child_info = ChildInfo::new_default(b"sub1");
		let initial: HashMap<_, BTreeMap<_, _>> = map![
			Some(child_info.clone()) => map![
				b"a".to_vec() => b"0".to_vec(),
				b"b".to_vec() => b"1".to_vec(),
				b"c".to_vec() => b"2".to_vec(),
				b"d".to_vec() => b"3".to_vec()
			],
		];
		let backend = InMemoryBackend::<BlakeTwo256>::from((initial, StateVersion::default()));
		let mut overlay = OverlayedChanges::default();
		let mut ext = Ext::new(&mut overlay, &backend, None);
		let r = ext.kill_child_storage(&child_info, Some(0), None).deconstruct();
		assert_matches!(r, (Some(_), 0, 0, 0));
		let r = ext
			.kill_child_storage(&child_info, Some(1), r.0.as_ref().map(|x| &x[..]))
			.deconstruct();
		assert_matches!(r, (Some(_), 1, 1, 1));
		let r = ext
			.kill_child_storage(&child_info, Some(4), r.0.as_ref().map(|x| &x[..]))
			.deconstruct();
		// Only 3 items remaining to remove
		assert_matches!(r, (None, 3, 3, 3));
		let r = ext.kill_child_storage(&child_info, Some(1), None).deconstruct();
		assert_matches!(r, (Some(_), 0, 0, 1));
	}

	#[test]
	fn limited_child_kill_off_by_one_works_without_limit() {
		let child_info = ChildInfo::new_default(b"sub1");
		let initial: HashMap<_, BTreeMap<_, _>> = map![
			Some(child_info.clone()) => map![
				b"a".to_vec() => b"0".to_vec(),
				b"b".to_vec() => b"1".to_vec(),
				b"c".to_vec() => b"2".to_vec(),
				b"d".to_vec() => b"3".to_vec()
			],
		];
		let backend = InMemoryBackend::<BlakeTwo256>::from((initial, StateVersion::default()));
		let mut overlay = OverlayedChanges::default();
		let mut ext = Ext::new(&mut overlay, &backend, None);
		assert_eq!(ext.kill_child_storage(&child_info, None, None).deconstruct(), (None, 4, 4, 4));
	}

	#[test]
	fn set_child_storage_works() {
		let child_info = ChildInfo::new_default(b"sub1");
		let child_info = &child_info;
		let state = new_in_mem::<BlakeTwo256>();
		let backend = state.as_trie_backend();
		let mut overlay = OverlayedChanges::default();
		let mut ext = Ext::new(&mut overlay, backend, None);

		ext.set_child_storage(child_info, b"abc".to_vec(), b"def".to_vec());
		assert_eq!(ext.child_storage(child_info, b"abc"), Some(b"def".to_vec()));
		let _ = ext.kill_child_storage(child_info, None, None);
		assert_eq!(ext.child_storage(child_info, b"abc"), None);
	}

	#[test]
	fn append_storage_works() {
		let reference_data = vec![b"data1".to_vec(), b"2".to_vec(), b"D3".to_vec(), b"d4".to_vec()];
		let key = b"key".to_vec();
		let state = new_in_mem::<BlakeTwo256>();
		let backend = state.as_trie_backend();
		let mut overlay = OverlayedChanges::default();
		{
			let mut ext = Ext::new(&mut overlay, backend, None);

			ext.storage_append(key.clone(), reference_data[0].encode());
			assert_eq!(ext.storage(key.as_slice()), Some(vec![reference_data[0].clone()].encode()));
		}
		overlay.start_transaction();
		{
			let mut ext = Ext::new(&mut overlay, backend, None);

			for i in reference_data.iter().skip(1) {
				ext.storage_append(key.clone(), i.encode());
			}
			assert_eq!(ext.storage(key.as_slice()), Some(reference_data.encode()));
		}
		overlay.rollback_transaction().unwrap();
		{
			let mut ext = Ext::new(&mut overlay, backend, None);
			assert_eq!(ext.storage(key.as_slice()), Some(vec![reference_data[0].clone()].encode()));
		}
	}

	// Test that we can append twice to a key, then perform a remove operation.
	// The test checks specifically that the append is merged with its parent transaction
	// on commit.
	#[test]
	fn commit_merges_append_with_parent() {
		#[derive(codec::Encode, codec::Decode)]
		enum Item {
			Item1,
			Item2,
		}

		let key = b"events".to_vec();
		let state = new_in_mem::<BlakeTwo256>();
		let backend = state.as_trie_backend();
		let mut overlay = OverlayedChanges::default();

		// Append first item
		overlay.start_transaction();
		{
			let mut ext = Ext::new(&mut overlay, backend, None);
			ext.clear_storage(key.as_slice());
			ext.storage_append(key.clone(), Item::Item1.encode());
		}

		// Append second item
		overlay.start_transaction();
		{
			let mut ext = Ext::new(&mut overlay, backend, None);

			assert_eq!(ext.storage(key.as_slice()), Some(vec![Item::Item1].encode()));

			ext.storage_append(key.clone(), Item::Item2.encode());

			assert_eq!(ext.storage(key.as_slice()), Some(vec![Item::Item1, Item::Item2].encode()),);
		}

		// Remove item
		overlay.start_transaction();
		{
			let mut ext = Ext::new(&mut overlay, backend, None);

			ext.place_storage(key.clone(), None);

			assert_eq!(ext.storage(key.as_slice()), None);
		}

		// Remove gets commited and merged into previous transaction
		overlay.commit_transaction().unwrap();
		{
			let mut ext = Ext::new(&mut overlay, backend, None);
			assert_eq!(ext.storage(key.as_slice()), None,);
		}

		// Remove gets rolled back, we should see the initial append again.
		overlay.rollback_transaction().unwrap();
		{
			let mut ext = Ext::new(&mut overlay, backend, None);
			assert_eq!(ext.storage(key.as_slice()), Some(vec![Item::Item1].encode()));
		}

		overlay.commit_transaction().unwrap();
		{
			let mut ext = Ext::new(&mut overlay, backend, None);
			assert_eq!(ext.storage(key.as_slice()), Some(vec![Item::Item1].encode()));
		}
	}

	#[test]
	fn remove_with_append_then_rollback_appended_then_append_again() {
		#[derive(codec::Encode, codec::Decode)]
		enum Item {
			InitializationItem,
			DiscardedItem,
			CommittedItem,
		}

		let key = b"events".to_vec();
		let state = new_in_mem::<BlakeTwo256>();
		let backend = state.as_trie_backend();
		let mut overlay = OverlayedChanges::default();

		// For example, block initialization with event.
		{
			let mut ext = Ext::new(&mut overlay, backend, None);
			ext.clear_storage(key.as_slice());
			ext.storage_append(key.clone(), Item::InitializationItem.encode());
		}
		overlay.start_transaction();

		// For example, first transaction resulted in panic during block building
		{
			let mut ext = Ext::new(&mut overlay, backend, None);

			assert_eq!(ext.storage(key.as_slice()), Some(vec![Item::InitializationItem].encode()));

			ext.storage_append(key.clone(), Item::DiscardedItem.encode());

			assert_eq!(
				ext.storage(key.as_slice()),
				Some(vec![Item::InitializationItem, Item::DiscardedItem].encode()),
			);
		}
		overlay.rollback_transaction().unwrap();

		// Then we apply next transaction which is valid this time.
		{
			let mut ext = Ext::new(&mut overlay, backend, None);

			assert_eq!(ext.storage(key.as_slice()), Some(vec![Item::InitializationItem].encode()));

			ext.storage_append(key.clone(), Item::CommittedItem.encode());

			assert_eq!(
				ext.storage(key.as_slice()),
				Some(vec![Item::InitializationItem, Item::CommittedItem].encode()),
			);
		}
		overlay.start_transaction();

		// Then only initialization item and second (committed) item should persist.
		{
			let mut ext = Ext::new(&mut overlay, backend, None);
			assert_eq!(
				ext.storage(key.as_slice()),
				Some(vec![Item::InitializationItem, Item::CommittedItem].encode()),
			);
		}
	}

	fn test_compact(remote_proof: StorageProof, _remote_root: &sp_core::H256) -> StorageProof {
		// Simply return the original storage proof since we're not using compact proofs for the zk-circuit
		remote_proof
	}

	#[test]
	fn prove_read_and_proof_check_works() {
		prove_read_and_proof_check_works_inner(StateVersion::V0);
		prove_read_and_proof_check_works_inner(StateVersion::V1);
	}
	fn prove_read_and_proof_check_works_inner(state_version: StateVersion) {
		let child_info = ChildInfo::new_default(b"sub1");
		let missing_child_info = ChildInfo::new_default(b"sub1sub2"); // key will include other child root to proof.
		let child_info = &child_info;
		let missing_child_info = &missing_child_info;
		// fetch read proof from 'remote' full node
		let remote_backend = trie_backend::tests::test_trie(state_version, None, None);
		let remote_root = remote_backend.storage_root(std::iter::empty(), state_version).0;
		let remote_proof = prove_read(remote_backend, &[b"value2"]).unwrap();
		let remote_proof = test_compact(remote_proof, &remote_root);
		// check proof locally
		let local_result1 =
			read_proof_check::<BlakeTwo256, _>(remote_root, remote_proof.clone(), &[b"value2"])
				.unwrap();
		let local_result2 =
			read_proof_check::<BlakeTwo256, _>(remote_root, remote_proof, &[&[0xff]]).is_ok();
		// check that results are correct
		assert_eq!(
			local_result1.into_iter().collect::<Vec<_>>(),
			vec![(b"value2".to_vec(), Some(vec![24]))],
		);
		assert_eq!(local_result2, false);
		// on child trie
		let remote_backend = trie_backend::tests::test_trie(state_version, None, None);
		let remote_root = remote_backend.storage_root(std::iter::empty(), state_version).0;
		let remote_proof = prove_child_read(remote_backend, child_info, &[b"value3"]).unwrap();
		let remote_proof = test_compact(remote_proof, &remote_root);
		let local_result1 = read_child_proof_check::<BlakeTwo256, _>(
			remote_root,
			remote_proof.clone(),
			child_info,
			&[b"value3"],
		)
		.unwrap();
		let local_result2 = read_child_proof_check::<BlakeTwo256, _>(
			remote_root,
			remote_proof.clone(),
			child_info,
			&[b"value2"],
		)
		.unwrap();
		let local_result3 = read_child_proof_check::<BlakeTwo256, _>(
			remote_root,
			remote_proof,
			missing_child_info,
			&[b"dummy"],
		)
		.unwrap();

		assert_eq!(
			local_result1.into_iter().collect::<Vec<_>>(),
			vec![(b"value3".to_vec(), Some(vec![142; 33]))],
		);
		assert_eq!(local_result2.into_iter().collect::<Vec<_>>(), vec![(b"value2".to_vec(), None)]);
		assert_eq!(local_result3.into_iter().collect::<Vec<_>>(), vec![(b"dummy".to_vec(), None)]);
	}



	#[test]
	fn child_read_compact_minimal_repro() {
		// Reproduce the failing case with fixed seed
		use rand::{rngs::SmallRng, RngCore, SeedableRng};
		let mut storage: HashMap<Option<ChildInfo>, BTreeMap<StorageKey, StorageValue>> =
			Default::default();
		let mut seed = [0; 32];

		// Use seed that causes failure (iteration 1)
		let i = 1u32;
		let seed_partial = &mut seed[0..4];
		seed_partial.copy_from_slice(&i.to_be_bytes()[..]);
		let mut rand = SmallRng::from_seed(seed);

		let nb_child_trie = rand.next_u32() as usize % 25;
		println!("Creating {} child tries", nb_child_trie);

		let mut child_infos = Vec::new();
		for child_idx in 0..nb_child_trie {
			let key_len = 1 + (rand.next_u32() % 10);
			let mut key = vec![0; key_len as usize];
			rand.fill_bytes(&mut key[..]);
			let child_info = ChildInfo::new_default(key.as_slice());
			println!("Child {} info: {:?}", child_idx, child_info.storage_key());

			let nb_item = 1 + rand.next_u32() % 25;
			let mut items = BTreeMap::new();
			for item in 0..nb_item {
				let key_len = 1 + (rand.next_u32() % 10);
				let mut key = vec![0; key_len as usize];
				rand.fill_bytes(&mut key[..]);
				let value = vec![item as u8; item as usize + 28];
				items.insert(key, value);
			}
			child_infos.push(child_info.clone());
			storage.insert(Some(child_info), items);
		}

		let trie: InMemoryBackend<BlakeTwo256> =
			(storage.clone(), StateVersion::default()).into();
		let trie_root = *trie.root();
		println!("Trie root: {:?}", trie_root);

		let backend = TrieBackendBuilder::wrap(&trie).with_recorder(Default::default()).build();
		let mut queries = Vec::new();

		// Make some queries to generate proof
		for c in 0..(5 + nb_child_trie / 2) {
			let child_info = if c < 5 {
				let key_len = 1 + (rand.next_u32() % 10);
				let mut key = vec![0; key_len as usize];
				rand.fill_bytes(&mut key[..]);
				ChildInfo::new_default(key.as_slice())
			} else {
				child_infos[rand.next_u32() as usize % nb_child_trie].clone()
			};

			if let Some(values) = storage.get(&Some(child_info.clone())) {
				for _ in 0..(1 + values.len() / 2) {
					let ix = rand.next_u32() as usize % values.len();
					for (i, (key, value)) in values.iter().enumerate() {
						if i == ix {
							let _ = backend.child_storage(&child_info, key.as_slice()).unwrap();
							queries.push((child_info.clone(), key.clone(), Some(value.clone())));
							break
						}
					}
				}
			}
			for _ in 0..4 {
				let key_len = 1 + (rand.next_u32() % 10);
				let mut key = vec![0; key_len as usize];
				rand.fill_bytes(&mut key[..]);
				let result = backend.child_storage(&child_info, key.as_slice()).unwrap();
				queries.push((child_info.clone(), key, result));
			}
		}

		let storage_proof = backend.extract_proof().expect("Failed to extract proof");
		println!("Proof generated, attempting compact conversion...");
		let _remote_proof = test_compact(storage_proof, &trie_root);
	}

	#[test]
	fn child_read_compact_stress_test() {
		use rand::{rngs::SmallRng, RngCore, SeedableRng};
		let mut storage: HashMap<Option<ChildInfo>, BTreeMap<StorageKey, StorageValue>> =
			Default::default();
		let mut seed = [0; 32];
		for i in 0..50u32 {
			let mut child_infos = Vec::new();
			let seed_partial = &mut seed[0..4];
			seed_partial.copy_from_slice(&i.to_be_bytes()[..]);
			let mut rand = SmallRng::from_seed(seed);

			let nb_child_trie = rand.next_u32() as usize % 25;
			for _ in 0..nb_child_trie {
				let key_len = 1 + (rand.next_u32() % 10);
				let mut key = vec![0; key_len as usize];
				rand.fill_bytes(&mut key[..]);
				let child_info = ChildInfo::new_default(key.as_slice());
				let nb_item = 1 + rand.next_u32() % 25;
				let mut items = BTreeMap::new();
				for item in 0..nb_item {
					let key_len = 1 + (rand.next_u32() % 10);
					let mut key = vec![0; key_len as usize];
					rand.fill_bytes(&mut key[..]);
					let value = vec![item as u8; item as usize + 28];
					items.insert(key, value);
				}
				child_infos.push(child_info.clone());
				storage.insert(Some(child_info), items);
			}

			let trie: InMemoryBackend<BlakeTwo256> =
				(storage.clone(), StateVersion::default()).into();
			let trie_root = *trie.root();
			let backend = TrieBackendBuilder::wrap(&trie).with_recorder(Default::default()).build();
			let mut queries = Vec::new();
			for c in 0..(5 + nb_child_trie / 2) {
				// random existing query
				let child_info = if c < 5 {
					// 4 missing child trie
					let key_len = 1 + (rand.next_u32() % 10);
					let mut key = vec![0; key_len as usize];
					rand.fill_bytes(&mut key[..]);
					ChildInfo::new_default(key.as_slice())
				} else {
					child_infos[rand.next_u32() as usize % nb_child_trie].clone()
				};

				if let Some(values) = storage.get(&Some(child_info.clone())) {
					for _ in 0..(1 + values.len() / 2) {
						let ix = rand.next_u32() as usize % values.len();
						for (i, (key, value)) in values.iter().enumerate() {
							if i == ix {
								assert_eq!(
									&backend
										.child_storage(&child_info, key.as_slice())
										.unwrap()
										.unwrap(),
									value
								);
								queries.push((
									child_info.clone(),
									key.clone(),
									Some(value.clone()),
								));
								break
							}
						}
					}
				}
				for _ in 0..4 {
					let key_len = 1 + (rand.next_u32() % 10);
					let mut key = vec![0; key_len as usize];
					rand.fill_bytes(&mut key[..]);
					let result = backend.child_storage(&child_info, key.as_slice()).unwrap();
					queries.push((child_info.clone(), key, result));
				}
			}

			let storage_proof = backend.extract_proof().expect("Failed to extract proof");
			let remote_proof = test_compact(storage_proof, &trie_root);
			let proof_check =
				create_proof_check_backend::<BlakeTwo256>(trie_root, remote_proof).unwrap();

			for (child_info, key, expected) in queries {
				assert_eq!(
					proof_check.child_storage(&child_info, key.as_slice()).unwrap(),
					expected,
				);
			}
		}
	}

	#[test]
	fn prove_read_with_size_limit_works() {
		let state_version = StateVersion::V0;
		let remote_backend = trie_backend::tests::test_trie(state_version, None, None);
		let remote_root = remote_backend.storage_root(::std::iter::empty(), state_version).0;
		let (proof, count) =
			prove_range_read_with_size(remote_backend, None, None, 0, None).unwrap();
		// Always contains at least some nodes.
		assert_eq!(proof.to_memory_db::<BlakeTwo256>().drain().len(), 5);
		assert_eq!(count, 1);
		assert_eq!(proof.encoded_size(), 705);

		let remote_backend = trie_backend::tests::test_trie(state_version, None, None);
		let (proof, count) =
			prove_range_read_with_size(remote_backend, None, None, 800, Some(&[])).unwrap();
		assert_eq!(proof.to_memory_db::<BlakeTwo256>().drain().len(), 7);
		assert_eq!(count, 3);
		assert_eq!(proof.encoded_size(), 828);
		let (results, completed) = read_range_proof_check::<BlakeTwo256>(
			remote_root,
			proof.clone(),
			None,
			None,
			Some(count),
			None,
		)
		.unwrap();
		assert_eq!(results.len() as u32, count);
		assert_eq!(completed, false);
		// When checking without count limit, proof may actually contain extra values.
		let (results, completed) =
			read_range_proof_check::<BlakeTwo256>(remote_root, proof, None, None, None, None)
				.unwrap();
		assert_eq!(results.len() as u32, 5);
		assert_eq!(completed, false);

		let remote_backend = trie_backend::tests::test_trie(state_version, None, None);
		let (proof, count) =
			prove_range_read_with_size(remote_backend, None, None, 50000, Some(&[])).unwrap();
		assert_eq!(proof.to_memory_db::<BlakeTwo256>().drain().len(), 15);
		assert_eq!(count, 132);
		assert_eq!(proof.encoded_size(), 5036);

		let (results, completed) =
			read_range_proof_check::<BlakeTwo256>(remote_root, proof, None, None, None, None)
				.unwrap();
		assert_eq!(results.len() as u32, count);
		assert_eq!(completed, true);
	}

	#[test]
	fn prove_read_with_size_limit_proof_size() {
		let mut root = H256::default();
		let mut mdb = PrefixedMemoryDB::<BlakeTwo256>::default();
		{
			let mut mdb = KeySpacedDBMut::new(&mut mdb, b"");
			let mut trie = TrieDBMutBuilderV1::new(&mut mdb, &mut root).build();
			trie.insert(b"value1", &[123; 1]).unwrap();
			trie.insert(b"value2", &[123; 10]).unwrap();
			trie.insert(b"value3", &[123; 100]).unwrap();
			trie.insert(b"value4", &[123; 1000]).unwrap();
		}

		let remote_backend: TrieBackend<PrefixedMemoryDB<BlakeTwo256>, BlakeTwo256> =
			TrieBackendBuilder::new(mdb, root)
				.with_optional_cache(None)
				.with_optional_recorder(None)
				.build();

		let (proof, count) =
			prove_range_read_with_size(remote_backend, None, None, 1000, None).unwrap();

		assert_eq!(proof.encoded_size(), 1398);
		assert_eq!(count, 3);
	}

	#[test]
	fn inner_state_versioning_switch_proofs() {
		let mut state_version = StateVersion::V0;
		let (mut mdb, mut root) = trie_backend::tests::test_db(state_version);
		{
			let mut trie = TrieDBMutBuilderV0::from_existing(&mut mdb, &mut root).build();
			trie.insert(b"foo", vec![1u8; 1_000].as_slice()) // big inner hash
				.expect("insert failed");
			trie.insert(b"foo2", vec![3u8; 16].as_slice()) // no inner hash
				.expect("insert failed");
			trie.insert(b"foo222", vec![5u8; 100].as_slice()) // inner hash
				.expect("insert failed");
		}

		let check_proof = |mdb, root, state_version| -> StorageProof {
			let remote_backend = TrieBackendBuilder::new(mdb, root).build();
			let remote_root = remote_backend.storage_root(std::iter::empty(), state_version).0;
			let remote_proof = prove_read(remote_backend, &[b"foo222"]).unwrap();
			// check proof locally
			let local_result1 =
				read_proof_check::<BlakeTwo256, _>(remote_root, remote_proof.clone(), &[b"foo222"])
					.unwrap();
			// check that results are correct
			assert_eq!(
				local_result1.into_iter().collect::<Vec<_>>(),
				vec![(b"foo222".to_vec(), Some(vec![5u8; 100]))],
			);
			remote_proof
		};

		let remote_proof = check_proof(mdb.clone(), root, state_version);
		// check full values in proof
		assert!(remote_proof.encode().len() > 800);
		assert!(remote_proof.encoded_size() > 800);
		let root1 = root;

		// do switch
		state_version = StateVersion::V1;
		{
			let mut trie = TrieDBMutBuilderV1::from_existing(&mut mdb, &mut root).build();
			trie.insert(b"foo222", vec![5u8; 100].as_slice()) // inner hash
				.expect("insert failed");
			// update with same value do change
			trie.insert(b"foo", vec![1u8; 1000].as_slice()) // inner hash
				.expect("insert failed");
		}
		let root3 = root;
		// assert!(root1 != root3); // ZK-trie may handle state versioning differently
		let remote_proof = check_proof(mdb.clone(), root, state_version);
		// nodes foo is replaced by its hashed value form.
		assert!(remote_proof.encode().len() < 1000);
		assert!(remote_proof.encoded_size() < 1000);
		assert_eq!(remote_proof.encode().len(), remote_proof.encoded_size());
	}

	#[test]
	fn prove_range_with_child_works() {
		let state_version = StateVersion::V0;
		let remote_backend = trie_backend::tests::test_trie(state_version, None, None);
		let remote_root = remote_backend.storage_root(std::iter::empty(), state_version).0;
		let mut start_at = smallvec::SmallVec::<[Vec<u8>; 2]>::new();
		let trie_backend = remote_backend.as_trie_backend();
		let max_iter = 1000;
		let mut nb_loop = 0;
		loop {
			nb_loop += 1;
			if max_iter == nb_loop {
				panic!("Too many loop in prove range");
			}
			let (proof, count) = prove_range_read_with_child_with_size_on_trie_backend(
				trie_backend,
				1,
				start_at.as_slice(),
			)
			.unwrap();
			// Always contains at least some nodes.
			assert!(proof.to_memory_db::<BlakeTwo256>().drain().len() > 0);
			assert!(count < 3); // when doing child we include parent and first child key.

			let (result, completed_depth) = read_range_proof_check_with_child::<BlakeTwo256>(
				remote_root,
				proof.clone(),
				start_at.as_slice(),
			)
			.unwrap();

			if completed_depth == 0 {
				break
			}
			assert!(result.update_last_key(completed_depth, &mut start_at));
		}

		assert_eq!(nb_loop, 13);
	}


	fn compact_multiple_child_trie_inner(state_version: StateVersion) -> usize {
		// this root will be queried
		let child_info1 = ChildInfo::new_default(b"sub1");
		// this root will not be include in proof
		let child_info2 = ChildInfo::new_default(b"sub2");
		// this root will be include in proof
		let child_info3 = ChildInfo::new_default(b"sub");
		let remote_backend = trie_backend::tests::test_trie(state_version, None, None);
		let long_vec: Vec<u8> = (0..1024usize).map(|_| 8u8).collect();
		let (remote_root, transaction) = remote_backend.full_storage_root(
			std::iter::empty(),
			vec![
				(
					&child_info1,
					vec![
						// a inner hashable node
						(&b"k"[..], Some(&long_vec[..])),
						// need to ensure this is not an inline node
						// otherwise we do not know what is accessed when
						// storing proof.
						(&b"key1"[..], Some(&vec![5u8; 32][..])),
						(&b"key2"[..], Some(&b"val3"[..])),
					]
					.into_iter(),
				),
				(
					&child_info2,
					vec![(&b"key3"[..], Some(&b"val4"[..])), (&b"key4"[..], Some(&b"val5"[..]))]
						.into_iter(),
				),
				(
					&child_info3,
					vec![(&b"key5"[..], Some(&b"val6"[..])), (&b"key6"[..], Some(&b"val7"[..]))]
						.into_iter(),
				),
			]
			.into_iter(),
			state_version,
		);
		let mut remote_storage = remote_backend.backend_storage().clone();
		remote_storage.consolidate(transaction);
		let remote_backend = TrieBackendBuilder::new(remote_storage, remote_root).build();
		let remote_proof = prove_child_read(remote_backend, &child_info1, &[b"key1"]).unwrap();
		let size = remote_proof.encoded_size();
		let remote_proof = test_compact(remote_proof, &remote_root);
		let local_result1 = read_child_proof_check::<BlakeTwo256, _>(
			remote_root,
			remote_proof,
			&child_info1,
			&[b"key1"],
		)
		.unwrap();
		assert_eq!(local_result1.len(), 1);
		assert_eq!(local_result1.get(&b"key1"[..]), Some(&Some(vec![5u8; 32])));
		size
	}

	#[test]
	fn child_storage_uuid() {
		let state_version = StateVersion::V0;
		let child_info_1 = ChildInfo::new_default(b"sub_test1");
		let child_info_2 = ChildInfo::new_default(b"sub_test2");

		use crate::trie_backend::tests::test_trie;
		let mut overlay = OverlayedChanges::default();

		let mut transaction = {
			let backend = test_trie(state_version, None, None);
			let mut ext = Ext::new(&mut overlay, &backend, None);
			ext.set_child_storage(&child_info_1, b"abc".to_vec(), b"def".to_vec());
			ext.set_child_storage(&child_info_2, b"abc".to_vec(), b"def".to_vec());
			ext.storage_root(state_version);
			overlay.drain_storage_changes(&backend, state_version).unwrap().transaction
		};
		let mut duplicate = false;
		for (k, (value, rc)) in transaction.drain().iter() {
			// look for a key inserted twice: transaction rc is 2
			if *rc == 2 {
				duplicate = true;
				println!("test duplicate for {:?} {:?}", k, value);
			}
		}
		assert!(!duplicate);
	}

	#[test]
	fn set_storage_empty_allowed() {
		let initial: BTreeMap<_, _> = map![
			b"aaa".to_vec() => b"0".to_vec(),
			b"bbb".to_vec() => b"".to_vec()
		];
		let state = InMemoryBackend::<BlakeTwo256>::from((initial, StateVersion::default()));
		let backend = state.as_trie_backend();

		let mut overlay = OverlayedChanges::default();
		overlay.start_transaction();
		overlay.set_storage(b"ccc".to_vec(), Some(b"".to_vec()));
		assert_eq!(overlay.storage(b"ccc"), Some(Some(&[][..])));
		overlay.commit_transaction().unwrap();
		overlay.start_transaction();
		assert_eq!(overlay.storage(b"ccc"), Some(Some(&[][..])));
		assert_eq!(overlay.storage(b"bbb"), None);

		{
			let mut ext = Ext::new(&mut overlay, backend, None);
			assert_eq!(ext.storage(b"bbb"), Some(vec![]));
			assert_eq!(ext.storage(b"ccc"), Some(vec![]));
			ext.clear_storage(b"ccc");
			assert_eq!(ext.storage(b"ccc"), None);
		}
		overlay.commit_transaction().unwrap();
		assert_eq!(overlay.storage(b"ccc"), Some(None));
	}
}
