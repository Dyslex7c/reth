use crate::{
    hashed_cursor::{HashedCursorFactory, HashedStorageCursor},
    node_iter::{TrieElement, TrieNodeIter},
    prefix_set::{PrefixSetMut, TriePrefixSetsMut},
    trie_cursor::TrieCursorFactory,
    walker::TrieWalker,
    HashBuilder, Nibbles, TRIE_ACCOUNT_RLP_MAX_SIZE,
};
use alloy_primitives::{
    keccak256,
    map::{B256Map, B256Set, HashMap, HashSet},
    Address, B256,
};
use alloy_rlp::{BufMut, Encodable};
use reth_execution_errors::trie::StateProofError;
use reth_trie_common::{
    proof::ProofRetainer, AccountProof, MultiProof, MultiProofTargets, StorageMultiProof,
};

mod trie_node;
pub use trie_node::*;

/// A struct for generating merkle proofs.
///
/// Proof generator adds the target address and slots to the prefix set, enables the proof retainer
/// on the hash builder and follows the same algorithm as the state root calculator.
/// See `StateRoot::root` for more info.
#[derive(Debug)]
pub struct Proof<T, H> {
    /// The factory for traversing trie nodes.
    trie_cursor_factory: T,
    /// The factory for hashed cursors.
    hashed_cursor_factory: H,
    /// A set of prefix sets that have changes.
    prefix_sets: TriePrefixSetsMut,
    /// Flag indicating whether to include branch node masks in the proof.
    collect_branch_node_masks: bool,
}

impl<T, H> Proof<T, H> {
    /// Create a new [`Proof`] instance.
    pub fn new(t: T, h: H) -> Self {
        Self {
            trie_cursor_factory: t,
            hashed_cursor_factory: h,
            prefix_sets: TriePrefixSetsMut::default(),
            collect_branch_node_masks: false,
        }
    }

    /// Set the trie cursor factory.
    pub fn with_trie_cursor_factory<TF>(self, trie_cursor_factory: TF) -> Proof<TF, H> {
        Proof {
            trie_cursor_factory,
            hashed_cursor_factory: self.hashed_cursor_factory,
            prefix_sets: self.prefix_sets,
            collect_branch_node_masks: self.collect_branch_node_masks,
        }
    }

    /// Set the hashed cursor factory.
    pub fn with_hashed_cursor_factory<HF>(self, hashed_cursor_factory: HF) -> Proof<T, HF> {
        Proof {
            trie_cursor_factory: self.trie_cursor_factory,
            hashed_cursor_factory,
            prefix_sets: self.prefix_sets,
            collect_branch_node_masks: self.collect_branch_node_masks,
        }
    }

    /// Set the prefix sets. They have to be mutable in order to allow extension with proof target.
    pub fn with_prefix_sets_mut(mut self, prefix_sets: TriePrefixSetsMut) -> Self {
        self.prefix_sets = prefix_sets;
        self
    }

    /// Set the flag indicating whether to include branch node masks in the proof.
    pub const fn with_branch_node_masks(mut self, branch_node_masks: bool) -> Self {
        self.collect_branch_node_masks = branch_node_masks;
        self
    }
}

impl<T, H> Proof<T, H>
where
    T: TrieCursorFactory + Clone,
    H: HashedCursorFactory + Clone,
{
    /// Generate an account proof from intermediate nodes.
    pub fn account_proof(
        self,
        address: Address,
        slots: &[B256],
    ) -> Result<AccountProof, StateProofError> {
        Ok(self
            .multiproof(MultiProofTargets::from_iter([(
                keccak256(address),
                slots.iter().map(keccak256).collect(),
            )]))?
            .account_proof(address, slots)?)
    }

    /// Generate a state multiproof according to specified targets.
    pub fn multiproof(
        mut self,
        mut targets: MultiProofTargets,
    ) -> Result<MultiProof, StateProofError> {
        let hashed_account_cursor = self.hashed_cursor_factory.hashed_account_cursor()?;
        let trie_cursor = self.trie_cursor_factory.account_trie_cursor()?;

        // Create the walker.
        let mut prefix_set = self.prefix_sets.account_prefix_set.clone();
        prefix_set.extend_keys(targets.keys().map(Nibbles::unpack));
        let walker = TrieWalker::state_trie(trie_cursor, prefix_set.freeze());

        // Create a hash builder to rebuild the root node since it is not available in the database.
        let retainer = targets.keys().map(Nibbles::unpack).collect();
        let mut hash_builder = HashBuilder::default()
            .with_proof_retainer(retainer)
            .with_updates(self.collect_branch_node_masks);

        // Initialize all storage multiproofs as empty.
        // Storage multiproofs for non empty tries will be overwritten if necessary.
        let mut storages: B256Map<_> =
            targets.keys().map(|key| (*key, StorageMultiProof::empty())).collect();
        let mut account_rlp = Vec::with_capacity(TRIE_ACCOUNT_RLP_MAX_SIZE);
        let mut account_node_iter = TrieNodeIter::state_trie(walker, hashed_account_cursor);
        while let Some(account_node) = account_node_iter.try_next()? {
            match account_node {
                TrieElement::Branch(node) => {
                    hash_builder.add_branch(node.key, node.value, node.children_are_in_trie);
                }
                TrieElement::Leaf(hashed_address, account) => {
                    let proof_targets = targets.remove(&hashed_address);
                    let leaf_is_proof_target = proof_targets.is_some();
                    let storage_prefix_set = self
                        .prefix_sets
                        .storage_prefix_sets
                        .remove(&hashed_address)
                        .unwrap_or_default();
                    let storage_multiproof = StorageProof::new_hashed(
                        self.trie_cursor_factory.clone(),
                        self.hashed_cursor_factory.clone(),
                        hashed_address,
                    )
                    .with_prefix_set_mut(storage_prefix_set)
                    .with_branch_node_masks(self.collect_branch_node_masks)
                    .storage_multiproof(proof_targets.unwrap_or_default())?;

                    // Encode account
                    account_rlp.clear();
                    let account = account.into_trie_account(storage_multiproof.root);
                    account.encode(&mut account_rlp as &mut dyn BufMut);

                    hash_builder.add_leaf(Nibbles::unpack(hashed_address), &account_rlp);

                    // We might be adding leaves that are not necessarily our proof targets.
                    if leaf_is_proof_target {
                        // Overwrite storage multiproof.
                        storages.insert(hashed_address, storage_multiproof);
                    }
                }
            }
        }
        let _ = hash_builder.root();
        let account_subtree = hash_builder.take_proof_nodes();
        let (branch_node_hash_masks, branch_node_tree_masks) = if self.collect_branch_node_masks {
            let updated_branch_nodes = hash_builder.updated_branch_nodes.unwrap_or_default();
            (
                updated_branch_nodes.iter().map(|(path, node)| (*path, node.hash_mask)).collect(),
                updated_branch_nodes
                    .into_iter()
                    .map(|(path, node)| (path, node.tree_mask))
                    .collect(),
            )
        } else {
            (HashMap::default(), HashMap::default())
        };

        Ok(MultiProof { account_subtree, branch_node_hash_masks, branch_node_tree_masks, storages })
    }
}

/// Generates storage merkle proofs.
#[derive(Debug)]
pub struct StorageProof<T, H> {
    /// The factory for traversing trie nodes.
    trie_cursor_factory: T,
    /// The factory for hashed cursors.
    hashed_cursor_factory: H,
    /// The hashed address of an account.
    hashed_address: B256,
    /// The set of storage slot prefixes that have changed.
    prefix_set: PrefixSetMut,
    /// Flag indicating whether to include branch node masks in the proof.
    collect_branch_node_masks: bool,
}

impl<T, H> StorageProof<T, H> {
    /// Create a new [`StorageProof`] instance.
    pub fn new(t: T, h: H, address: Address) -> Self {
        Self::new_hashed(t, h, keccak256(address))
    }

    /// Create a new [`StorageProof`] instance with hashed address.
    pub fn new_hashed(t: T, h: H, hashed_address: B256) -> Self {
        Self {
            trie_cursor_factory: t,
            hashed_cursor_factory: h,
            hashed_address,
            prefix_set: PrefixSetMut::default(),
            collect_branch_node_masks: false,
        }
    }

    /// Set the trie cursor factory.
    pub fn with_trie_cursor_factory<TF>(self, trie_cursor_factory: TF) -> StorageProof<TF, H> {
        StorageProof {
            trie_cursor_factory,
            hashed_cursor_factory: self.hashed_cursor_factory,
            hashed_address: self.hashed_address,
            prefix_set: self.prefix_set,
            collect_branch_node_masks: self.collect_branch_node_masks,
        }
    }

    /// Set the hashed cursor factory.
    pub fn with_hashed_cursor_factory<HF>(self, hashed_cursor_factory: HF) -> StorageProof<T, HF> {
        StorageProof {
            trie_cursor_factory: self.trie_cursor_factory,
            hashed_cursor_factory,
            hashed_address: self.hashed_address,
            prefix_set: self.prefix_set,
            collect_branch_node_masks: self.collect_branch_node_masks,
        }
    }

    /// Set the changed prefixes.
    pub fn with_prefix_set_mut(mut self, prefix_set: PrefixSetMut) -> Self {
        self.prefix_set = prefix_set;
        self
    }

    /// Set the flag indicating whether to include branch node masks in the proof.
    pub const fn with_branch_node_masks(mut self, branch_node_masks: bool) -> Self {
        self.collect_branch_node_masks = branch_node_masks;
        self
    }
}

impl<T, H> StorageProof<T, H>
where
    T: TrieCursorFactory,
    H: HashedCursorFactory,
{
    /// Generate an account proof from intermediate nodes.
    pub fn storage_proof(
        self,
        slot: B256,
    ) -> Result<reth_trie_common::StorageProof, StateProofError> {
        let targets = HashSet::from_iter([keccak256(slot)]);
        Ok(self.storage_multiproof(targets)?.storage_proof(slot)?)
    }

    /// Generate storage proof.
    pub fn storage_multiproof(
        mut self,
        targets: B256Set,
    ) -> Result<StorageMultiProof, StateProofError> {
        let mut hashed_storage_cursor =
            self.hashed_cursor_factory.hashed_storage_cursor(self.hashed_address)?;

        // short circuit on empty storage
        if hashed_storage_cursor.is_storage_empty()? {
            return Ok(StorageMultiProof::empty())
        }

        let target_nibbles = targets.into_iter().map(Nibbles::unpack).collect::<Vec<_>>();
        self.prefix_set.extend_keys(target_nibbles.clone());

        let trie_cursor = self.trie_cursor_factory.storage_trie_cursor(self.hashed_address)?;
        let walker = TrieWalker::storage_trie(trie_cursor, self.prefix_set.freeze());

        let retainer = ProofRetainer::from_iter(target_nibbles);
        let mut hash_builder = HashBuilder::default()
            .with_proof_retainer(retainer)
            .with_updates(self.collect_branch_node_masks);
        let mut storage_node_iter = TrieNodeIter::storage_trie(walker, hashed_storage_cursor);
        while let Some(node) = storage_node_iter.try_next()? {
            match node {
                TrieElement::Branch(node) => {
                    hash_builder.add_branch(node.key, node.value, node.children_are_in_trie);
                }
                TrieElement::Leaf(hashed_slot, value) => {
                    hash_builder.add_leaf(
                        Nibbles::unpack(hashed_slot),
                        alloy_rlp::encode_fixed_size(&value).as_ref(),
                    );
                }
            }
        }

        let root = hash_builder.root();
        let subtree = hash_builder.take_proof_nodes();
        let (branch_node_hash_masks, branch_node_tree_masks) = if self.collect_branch_node_masks {
            let updated_branch_nodes = hash_builder.updated_branch_nodes.unwrap_or_default();
            (
                updated_branch_nodes.iter().map(|(path, node)| (*path, node.hash_mask)).collect(),
                updated_branch_nodes
                    .into_iter()
                    .map(|(path, node)| (path, node.tree_mask))
                    .collect(),
            )
        } else {
            (HashMap::default(), HashMap::default())
        };

        Ok(StorageMultiProof { root, subtree, branch_node_hash_masks, branch_node_tree_masks })
    }
}
