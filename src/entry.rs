//! The `entry` module is a fundamental building block of Proof of History. It contains a
//! unique ID that is the hash of the Entry before it, plus the hash of the
//! transactions within it. Entries cannot be reordered, and its field `num_hashes`
//! represents an approximate amount of time since the last Entry was created.
use crate::packet::{Blob, SharedBlob, BLOB_DATA_SIZE};
use crate::poh::Poh;
use crate::result::Result;
use bincode::{deserialize, serialize_into, serialized_size};
use chrono::prelude::Utc;
use rayon::prelude::*;
use solana_sdk::budget_transaction::BudgetTransaction;
use solana_sdk::hash::{hash, Hash};
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{Keypair, KeypairUtil};
use solana_sdk::transaction::Transaction;
use solana_sdk::vote_program::Vote;
use solana_sdk::vote_transaction::VoteTransaction;
use std::borrow::Borrow;
use std::io::Cursor;
use std::mem::size_of;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, RwLock};

pub type EntrySender = Sender<Vec<Entry>>;
pub type EntryReceiver = Receiver<Vec<Entry>>;

/// Each Entry contains three pieces of data. The `num_hashes` field is the number
/// of hashes performed since the previous entry.  The `id` field is the result
/// of hashing `id` from the previous entry `num_hashes` times.  The `transactions`
/// field points to Transactions that took place shortly before `id` was generated.
///
/// If you divide `num_hashes` by the amount of time it takes to generate a new hash, you
/// get a duration estimate since the last Entry. Since processing power increases
/// over time, one should expect the duration `num_hashes` represents to decrease proportionally.
/// An upper bound on Duration can be estimated by assuming each hash was generated by the
/// world's fastest processor at the time the entry was recorded. Or said another way, it
/// is physically not possible for a shorter duration to have occurred if one assumes the
/// hash was computed by the world's fastest processor at that time. The hash chain is both
/// a Verifiable Delay Function (VDF) and a Proof of Work (not to be confused with Proof of
/// Work consensus!)

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq, Clone)]
pub struct Entry {
    /// tick height of the ledger, not including any tick implied by this Entry
    pub tick_height: u64,

    /// The number of hashes since the previous Entry ID.
    pub num_hashes: u64,

    /// The SHA-256 hash `num_hashes` after the previous Entry ID.
    pub id: Hash,

    /// An unordered list of transactions that were observed before the Entry ID was
    /// generated. They may have been observed before a previous Entry ID but were
    /// pushed back into this list to ensure deterministic interpretation of the ledger.
    pub transactions: Vec<Transaction>,
}

impl Entry {
    /// Creates the next Entry `num_hashes` after `start_hash`.
    pub fn new(
        prev_id: &Hash,
        tick_height: u64,
        num_hashes: u64,
        transactions: Vec<Transaction>,
    ) -> Self {
        let entry = {
            if num_hashes == 0 && transactions.is_empty() {
                Entry {
                    tick_height,
                    num_hashes: 0,
                    id: *prev_id,
                    transactions,
                }
            } else if num_hashes == 0 {
                // If you passed in transactions, but passed in num_hashes == 0, then
                // next_hash will generate the next hash and set num_hashes == 1
                let id = next_hash(prev_id, 1, &transactions);
                Entry {
                    tick_height,
                    num_hashes: 1,
                    id,
                    transactions,
                }
            } else {
                // Otherwise, the next Entry `num_hashes` after `start_hash`.
                // If you wanted a tick for instance, then pass in num_hashes = 1
                // and transactions = empty
                let id = next_hash(prev_id, num_hashes, &transactions);
                Entry {
                    tick_height,
                    num_hashes,
                    id,
                    transactions,
                }
            }
        };

        let size = Entry::serialized_size(&entry.transactions[..]);
        if size > BLOB_DATA_SIZE as u64 {
            panic!(
                "Serialized entry size too large: {} ({} transactions):",
                size,
                entry.transactions.len()
            );
        }
        entry
    }

    pub fn to_shared_blob(&self) -> SharedBlob {
        let blob = self.to_blob();
        Arc::new(RwLock::new(blob))
    }

    pub fn to_blob(&self) -> Blob {
        let mut blob = Blob::default();
        let pos = {
            let mut out = Cursor::new(blob.data_mut());
            serialize_into(&mut out, &self).expect("failed to serialize output");
            out.position() as usize
        };
        blob.set_size(pos);
        blob
    }

    /// Estimate serialized_size of Entry without creating an Entry.
    pub fn serialized_size(transactions: &[Transaction]) -> u64 {
        let txs_size: u64 = transactions
            .iter()
            .map(|tx| tx.serialized_size().unwrap())
            .sum();
        // tick_height+num_hashes   +    id  +              txs

        (3 * size_of::<u64>() + size_of::<Hash>()) as u64 + txs_size
    }

    pub fn num_will_fit(transactions: &[Transaction]) -> usize {
        if transactions.is_empty() {
            return 0;
        }
        let mut num = transactions.len();
        let mut upper = transactions.len();
        let mut lower = 1; // if one won't fit, we have a lot of TODOs
        let mut next = transactions.len(); // optimistic
        loop {
            debug!(
                "num {}, upper {} lower {} next {} transactions.len() {}",
                num,
                upper,
                lower,
                next,
                transactions.len()
            );
            if Self::serialized_size(&transactions[..num]) <= BLOB_DATA_SIZE as u64 {
                next = (upper + num) / 2;
                lower = num;
                debug!("num {} fits, maybe too well? trying {}", num, next);
            } else {
                next = (lower + num) / 2;
                upper = num;
                debug!("num {} doesn't fit! trying {}", num, next);
            }
            // same as last time
            if next == num {
                debug!("converged on num {}", num);
                break;
            }
            num = next;
        }
        num
    }

    /// Creates the next Tick Entry `num_hashes` after `start_hash`.
    pub fn new_mut(
        start_hash: &mut Hash,
        num_hashes: &mut u64,
        transactions: Vec<Transaction>,
    ) -> Self {
        let entry = Self::new(start_hash, 0, *num_hashes, transactions);
        *start_hash = entry.id;
        *num_hashes = 0;
        assert!(serialized_size(&entry).unwrap() <= BLOB_DATA_SIZE as u64);
        entry
    }

    /// Creates a Entry from the number of hashes `num_hashes`
    /// since the previous transaction and that resulting `id`.

    #[cfg(test)]
    pub fn new_tick(tick_height: u64, num_hashes: u64, id: &Hash) -> Self {
        Entry {
            tick_height,
            num_hashes,
            id: *id,
            transactions: vec![],
        }
    }

    /// Verifies self.id is the result of hashing a `start_hash` `self.num_hashes` times.
    /// If the transaction is not a Tick, then hash that as well.
    pub fn verify(&self, start_hash: &Hash) -> bool {
        let ref_hash = next_hash(start_hash, self.num_hashes, &self.transactions);
        if self.id != ref_hash {
            warn!(
                "next_hash is invalid expected: {:?} actual: {:?}",
                self.id, ref_hash
            );
            return false;
        }
        true
    }

    pub fn is_tick(&self) -> bool {
        self.transactions.is_empty()
    }
}

/// Creates the hash `num_hashes` after `start_hash`. If the transaction contains
/// a signature, the final hash will be a hash of both the previous ID and
/// the signature.  If num_hashes is zero and there's no transaction data,
///  start_hash is returned.
fn next_hash(start_hash: &Hash, num_hashes: u64, transactions: &[Transaction]) -> Hash {
    if num_hashes == 0 && transactions.is_empty() {
        return *start_hash;
    }

    let mut poh = Poh::new(*start_hash, 0);

    for _ in 1..num_hashes {
        poh.hash();
    }

    if transactions.is_empty() {
        poh.tick().id
    } else {
        poh.record(Transaction::hash(transactions)).id
    }
}

pub fn reconstruct_entries_from_blobs<I>(blobs: I) -> Result<(Vec<Entry>, u64)>
where
    I: IntoIterator,
    I::Item: Borrow<Blob>,
{
    let mut entries: Vec<Entry> = vec![];
    let mut num_ticks = 0;

    for blob in blobs.into_iter() {
        let entry: Entry = {
            let msg_size = blob.borrow().size()?;
            deserialize(&blob.borrow().data()[..msg_size])?
        };

        if entry.is_tick() {
            num_ticks += 1
        }
        entries.push(entry)
    }
    Ok((entries, num_ticks))
}

// an EntrySlice is a slice of Entries
pub trait EntrySlice {
    /// Verifies the hashes and counts of a slice of transactions are all consistent.
    fn verify(&self, start_hash: &Hash) -> bool;
    fn to_shared_blobs(&self) -> Vec<SharedBlob>;
    fn to_blobs(&self) -> Vec<Blob>;
    fn votes(&self) -> Vec<(Pubkey, Vote, Hash)>;
}

impl EntrySlice for [Entry] {
    fn verify(&self, start_hash: &Hash) -> bool {
        let genesis = [Entry {
            tick_height: 0,
            num_hashes: 0,
            id: *start_hash,
            transactions: vec![],
        }];
        let entry_pairs = genesis.par_iter().chain(self).zip(self);
        entry_pairs.all(|(x0, x1)| {
            let r = x1.verify(&x0.id);
            if !r {
                warn!(
                    "entry invalid!: x0: {:?}, x1: {:?} num txs: {}",
                    x0.id,
                    x1.id,
                    x1.transactions.len()
                );
            }
            r
        })
    }

    fn to_blobs(&self) -> Vec<Blob> {
        self.iter().map(|entry| entry.to_blob()).collect()
    }

    fn to_shared_blobs(&self) -> Vec<SharedBlob> {
        self.iter().map(|entry| entry.to_shared_blob()).collect()
    }

    fn votes(&self) -> Vec<(Pubkey, Vote, Hash)> {
        self.iter()
            .flat_map(|entry| {
                entry
                    .transactions
                    .iter()
                    .flat_map(VoteTransaction::get_votes)
            })
            .collect()
    }
}

/// Creates the next entries for given transactions, outputs
/// updates start_hash to id of last Entry, sets num_hashes to 0
pub fn next_entries_mut(
    start_hash: &mut Hash,
    num_hashes: &mut u64,
    transactions: Vec<Transaction>,
) -> Vec<Entry> {
    // TODO: ?? find a number that works better than |?
    //                                               V
    if transactions.is_empty() || transactions.len() == 1 {
        vec![Entry::new_mut(start_hash, num_hashes, transactions)]
    } else {
        let mut chunk_start = 0;
        let mut entries = Vec::new();

        while chunk_start < transactions.len() {
            let mut chunk_end = transactions.len();
            let mut upper = chunk_end;
            let mut lower = chunk_start;
            let mut next = chunk_end; // be optimistic that all will fit

            // binary search for how many transactions will fit in an Entry (i.e. a BLOB)
            loop {
                debug!(
                    "chunk_end {}, upper {} lower {} next {} transactions.len() {}",
                    chunk_end,
                    upper,
                    lower,
                    next,
                    transactions.len()
                );
                if Entry::serialized_size(&transactions[chunk_start..chunk_end])
                    <= BLOB_DATA_SIZE as u64
                {
                    next = (upper + chunk_end) / 2;
                    lower = chunk_end;
                    debug!(
                        "chunk_end {} fits, maybe too well? trying {}",
                        chunk_end, next
                    );
                } else {
                    next = (lower + chunk_end) / 2;
                    upper = chunk_end;
                    debug!("chunk_end {} doesn't fit! trying {}", chunk_end, next);
                }
                // same as last time
                if next == chunk_end {
                    debug!("converged on chunk_end {}", chunk_end);
                    break;
                }
                chunk_end = next;
            }
            entries.push(Entry::new_mut(
                start_hash,
                num_hashes,
                transactions[chunk_start..chunk_end].to_vec(),
            ));
            chunk_start = chunk_end;
        }

        entries
    }
}

/// Creates the next Entries for given transactions
pub fn next_entries(
    start_hash: &Hash,
    num_hashes: u64,
    transactions: Vec<Transaction>,
) -> Vec<Entry> {
    let mut id = *start_hash;
    let mut num_hashes = num_hashes;
    next_entries_mut(&mut id, &mut num_hashes, transactions)
}

pub fn create_ticks(num_ticks: u64, mut hash: Hash) -> Vec<Entry> {
    let mut ticks = Vec::with_capacity(num_ticks as usize);
    for _ in 0..num_ticks {
        let new_tick = Entry::new(&hash, 0, 1, vec![]);
        hash = new_tick.id;
        ticks.push(new_tick);
    }

    ticks
}

pub fn make_tiny_test_entries_from_id(start: &Hash, num: usize) -> Vec<Entry> {
    let keypair = Keypair::new();

    let mut id = *start;
    let mut num_hashes = 0;
    (0..num)
        .map(|_| {
            Entry::new_mut(
                &mut id,
                &mut num_hashes,
                vec![Transaction::budget_new_timestamp(
                    &keypair,
                    keypair.pubkey(),
                    keypair.pubkey(),
                    Utc::now(),
                    *start,
                )],
            )
        })
        .collect()
}

pub fn make_tiny_test_entries(num: usize) -> Vec<Entry> {
    let zero = Hash::default();
    let one = hash(&zero.as_ref());
    make_tiny_test_entries_from_id(&one, num)
}

pub fn make_large_test_entries(num_entries: usize) -> Vec<Entry> {
    let zero = Hash::default();
    let one = hash(&zero.as_ref());
    let keypair = Keypair::new();

    let tx = Transaction::budget_new_timestamp(
        &keypair,
        keypair.pubkey(),
        keypair.pubkey(),
        Utc::now(),
        one,
    );

    let serialized_size = tx.serialized_size().unwrap();
    let num_txs = BLOB_DATA_SIZE / serialized_size as usize;
    let txs = vec![tx; num_txs];
    let entry = next_entries(&one, 1, txs)[0].clone();
    vec![entry; num_entries]
}

#[cfg(test)]
pub fn make_consecutive_blobs(
    id: &Pubkey,
    num_blobs_to_make: u64,
    start_height: u64,
    start_hash: Hash,
    addr: &std::net::SocketAddr,
) -> Vec<SharedBlob> {
    let entries = create_ticks(num_blobs_to_make, start_hash);

    let blobs = entries.to_shared_blobs();
    let mut index = start_height;
    for blob in &blobs {
        let mut blob = blob.write().unwrap();
        blob.set_index(index).unwrap();
        blob.set_id(id).unwrap();
        blob.meta.set_addr(addr);
        index += 1;
    }
    blobs
}

#[cfg(test)]
/// Creates the next Tick or Transaction Entry `num_hashes` after `start_hash`.
pub fn next_entry(prev_id: &Hash, num_hashes: u64, transactions: Vec<Transaction>) -> Entry {
    assert!(num_hashes > 0 || transactions.is_empty());
    Entry {
        tick_height: 0,
        num_hashes,
        id: next_hash(prev_id, num_hashes, &transactions),
        transactions,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entry::Entry;
    use crate::packet::{to_blobs, BLOB_DATA_SIZE, PACKET_DATA_SIZE};
    use solana_sdk::budget_transaction::BudgetTransaction;
    use solana_sdk::hash::hash;
    use solana_sdk::signature::{Keypair, KeypairUtil};
    use solana_sdk::system_transaction::SystemTransaction;
    use solana_sdk::transaction::Transaction;
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};

    #[test]
    fn test_entry_verify() {
        let zero = Hash::default();
        let one = hash(&zero.as_ref());
        assert!(Entry::new_tick(0, 0, &zero).verify(&zero)); // base case, never used
        assert!(!Entry::new_tick(1, 0, &zero).verify(&one)); // base case, bad
        assert!(next_entry(&zero, 1, vec![]).verify(&zero)); // inductive step
        assert!(!next_entry(&zero, 1, vec![]).verify(&one)); // inductive step, bad
    }

    #[test]
    fn test_transaction_reorder_attack() {
        let zero = Hash::default();

        // First, verify entries
        let keypair = Keypair::new();
        let tx0 = Transaction::system_new(&keypair, keypair.pubkey(), 0, zero);
        let tx1 = Transaction::system_new(&keypair, keypair.pubkey(), 1, zero);
        let mut e0 = Entry::new(&zero, 0, 0, vec![tx0.clone(), tx1.clone()]);
        assert!(e0.verify(&zero));

        // Next, swap two transactions and ensure verification fails.
        e0.transactions[0] = tx1; // <-- attack
        e0.transactions[1] = tx0;
        assert!(!e0.verify(&zero));
    }

    #[test]
    fn test_witness_reorder_attack() {
        let zero = Hash::default();

        // First, verify entries
        let keypair = Keypair::new();
        let tx0 = Transaction::budget_new_timestamp(
            &keypair,
            keypair.pubkey(),
            keypair.pubkey(),
            Utc::now(),
            zero,
        );
        let tx1 =
            Transaction::budget_new_signature(&keypair, keypair.pubkey(), keypair.pubkey(), zero);
        let mut e0 = Entry::new(&zero, 0, 0, vec![tx0.clone(), tx1.clone()]);
        assert!(e0.verify(&zero));

        // Next, swap two witness transactions and ensure verification fails.
        e0.transactions[0] = tx1; // <-- attack
        e0.transactions[1] = tx0;
        assert!(!e0.verify(&zero));
    }

    #[test]
    fn test_next_entry() {
        let zero = Hash::default();
        let tick = next_entry(&zero, 1, vec![]);
        assert_eq!(tick.num_hashes, 1);
        assert_ne!(tick.id, zero);

        let tick = next_entry(&zero, 0, vec![]);
        assert_eq!(tick.num_hashes, 0);
        assert_eq!(tick.id, zero);

        let keypair = Keypair::new();
        let tx0 = Transaction::budget_new_timestamp(
            &keypair,
            keypair.pubkey(),
            keypair.pubkey(),
            Utc::now(),
            zero,
        );
        let entry0 = next_entry(&zero, 1, vec![tx0.clone()]);
        assert_eq!(entry0.num_hashes, 1);
        assert_eq!(entry0.id, next_hash(&zero, 1, &vec![tx0]));
    }

    #[test]
    #[should_panic]
    fn test_next_entry_panic() {
        let zero = Hash::default();
        let keypair = Keypair::new();
        let tx = Transaction::system_new(&keypair, keypair.pubkey(), 0, zero);
        next_entry(&zero, 0, vec![tx]);
    }

    #[test]
    fn test_serialized_size() {
        let zero = Hash::default();
        let keypair = Keypair::new();
        let tx = Transaction::system_new(&keypair, keypair.pubkey(), 0, zero);
        let entry = next_entry(&zero, 1, vec![tx.clone()]);
        assert_eq!(
            Entry::serialized_size(&[tx]),
            serialized_size(&entry).unwrap()
        );
    }

    #[test]
    fn test_verify_slice() {
        solana_logger::setup();
        let zero = Hash::default();
        let one = hash(&zero.as_ref());
        assert!(vec![][..].verify(&zero)); // base case
        assert!(vec![Entry::new_tick(0, 0, &zero)][..].verify(&zero)); // singleton case 1
        assert!(!vec![Entry::new_tick(0, 0, &zero)][..].verify(&one)); // singleton case 2, bad
        assert!(vec![next_entry(&zero, 0, vec![]); 2][..].verify(&zero)); // inductive step

        let mut bad_ticks = vec![next_entry(&zero, 0, vec![]); 2];
        bad_ticks[1].id = one;
        assert!(!bad_ticks.verify(&zero)); // inductive step, bad
    }

    fn make_test_entries() -> Vec<Entry> {
        let zero = Hash::default();
        let one = hash(&zero.as_ref());
        let keypair = Keypair::new();
        let vote_account = Keypair::new();
        let tx0 = Transaction::vote_new(&vote_account, 1, one, 1);
        let tx1 = Transaction::budget_new_timestamp(
            &keypair,
            keypair.pubkey(),
            keypair.pubkey(),
            Utc::now(),
            one,
        );
        //
        // TODO: this magic number and the mix of transaction types
        //       is designed to fill up a Blob more or less exactly,
        //       to get near enough the threshold that
        //       deserialization falls over if it uses the wrong size()
        //       parameter to index into blob.data()
        //
        // magic numbers -----------------+
        //                                |
        //                                V
        let mut transactions = vec![tx0; 362];
        transactions.extend(vec![tx1; 100]);
        next_entries(&zero, 0, transactions)
    }

    #[test]
    fn test_entries_to_shared_blobs() {
        solana_logger::setup();
        let entries = make_test_entries();

        let blob_q = entries.to_blobs();

        assert_eq!(reconstruct_entries_from_blobs(blob_q).unwrap().0, entries);
    }

    #[test]
    fn test_bad_blobs_attack() {
        solana_logger::setup();
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)), 8000);
        let blobs_q = to_blobs(vec![(0, addr)]).unwrap(); // <-- attack!
        assert!(reconstruct_entries_from_blobs(blobs_q).is_err());
    }

    #[test]
    fn test_next_entries() {
        solana_logger::setup();
        let id = Hash::default();
        let next_id = hash(&id.as_ref());
        let keypair = Keypair::new();
        let vote_account = Keypair::new();
        let tx_small = Transaction::vote_new(&vote_account, 1, next_id, 2);
        let tx_large = Transaction::budget_new(&keypair, keypair.pubkey(), 1, next_id);

        let tx_small_size = tx_small.serialized_size().unwrap() as usize;
        let tx_large_size = tx_large.serialized_size().unwrap() as usize;
        let entry_size = serialized_size(&Entry {
            tick_height: 0,
            num_hashes: 0,
            id: Hash::default(),
            transactions: vec![],
        })
        .unwrap() as usize;
        assert!(tx_small_size < tx_large_size);
        assert!(tx_large_size < PACKET_DATA_SIZE);

        let threshold = (BLOB_DATA_SIZE - entry_size) / tx_small_size;

        // verify no split
        let transactions = vec![tx_small.clone(); threshold];
        let entries0 = next_entries(&id, 0, transactions.clone());
        assert_eq!(entries0.len(), 1);
        assert!(entries0.verify(&id));

        // verify the split with uniform transactions
        let transactions = vec![tx_small.clone(); threshold * 2];
        let entries0 = next_entries(&id, 0, transactions.clone());
        assert_eq!(entries0.len(), 2);
        assert!(entries0.verify(&id));

        // verify the split with small transactions followed by large
        // transactions
        let mut transactions = vec![tx_small.clone(); BLOB_DATA_SIZE / tx_small_size];
        let large_transactions = vec![tx_large.clone(); BLOB_DATA_SIZE / tx_large_size];

        transactions.extend(large_transactions);

        let entries0 = next_entries(&id, 0, transactions.clone());
        assert!(entries0.len() >= 2);
        assert!(entries0.verify(&id));
    }

}
