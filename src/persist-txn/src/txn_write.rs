// Copyright Materialize, Inc. and contributors. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

//! Interfaces for writing txn shards as well as data shards.

use std::collections::BTreeMap;
use std::fmt::Debug;

use differential_dataflow::difference::Semigroup;
use differential_dataflow::lattice::Lattice;
use differential_dataflow::Hashable;
use mz_persist_client::ShardId;
use mz_persist_types::{Codec, Codec64};
use prost::Message;
use timely::order::TotalOrder;
use timely::progress::{Antichain, Timestamp};
use tracing::debug;

use crate::txns::{Tidy, TxnsHandle};
use crate::{StepForward, TxnsCodec, TxnsEntry};

/// An in-progress transaction.
#[derive(Debug)]
pub struct Txn<K, V, D> {
    writes: BTreeMap<ShardId, Vec<(K, V, D)>>,
    tidy: Tidy,
}

impl<K, V, D> Txn<K, V, D>
where
    K: Debug + Codec,
    V: Debug + Codec,
    D: Semigroup + Codec64 + Send + Sync,
{
    pub(crate) fn new() -> Self {
        Txn {
            writes: BTreeMap::default(),
            tidy: Tidy::default(),
        }
    }

    /// Stage a write to the in-progress txn.
    ///
    /// The timestamp will be assigned at commit time.
    ///
    /// TODO(txn): Allow this to spill to s3 (for bounded memory) once persist
    /// can make the ts rewrite op efficient.
    #[allow(clippy::unused_async)]
    pub async fn write(&mut self, data_id: &ShardId, key: K, val: V, diff: D) {
        self.writes
            .entry(*data_id)
            .or_default()
            .push((key, val, diff))
    }

    /// Commit this transaction at `commit_ts`.
    ///
    /// This either atomically commits all staged writes or, if that's no longer
    /// possible at the requested timestamp, returns an error with the least
    /// commit-able timestamp.
    ///
    /// On success a token is returned representing apply work expected to be
    /// promptly performed by the caller. At this point, the txn is durable and
    /// it's safe to bubble up success, but reads at the commit timestamp will
    /// block until this apply work finishes. In the event of a crash, neither
    /// correctness nor liveness require this followup be done.
    ///
    /// Panics if any involved data shards were not registered before commit ts.
    pub async fn commit_at<T, C>(
        &self,
        handle: &mut TxnsHandle<K, V, T, D, C>,
        commit_ts: T,
    ) -> Result<TxnApply<T>, T>
    where
        T: Timestamp + Lattice + TotalOrder + StepForward + Codec64,
        C: TxnsCodec,
    {
        // TODO(txn): Use ownership to disallow a double commit.
        let mut txns_upper = handle
            .txns_write
            .upper()
            .as_option()
            .expect("txns shard should not be closed")
            .clone();

        // Validate that the involved data shards are all registered. txns_upper
        // only advances in the loop below, so we only have to check
        // registration once.
        let () = handle.txns_cache.update_ge(&txns_upper).await;
        for (data_id, _) in self.writes.iter() {
            let registered_before_commit_ts = handle
                .txns_cache
                .data_since(data_id)
                .map_or(false, |x| x < commit_ts);
            assert!(
                registered_before_commit_ts,
                "{} should be registered before commit at {:?}",
                data_id, commit_ts
            );
        }

        loop {
            // txns_upper is the (inclusive) minimum timestamp at which we
            // could possibly write. If our requested commit timestamp is before
            // that, then it's no longer possible to write and the caller needs
            // to decide what to do.
            if commit_ts < txns_upper {
                debug!(
                    "commit_at {:?} mismatch current={:?}",
                    commit_ts, txns_upper
                );
                return Err(txns_upper);
            }
            debug!(
                "commit_at {:?}: [{:?}, {:?}) begin",
                commit_ts,
                txns_upper,
                commit_ts.step_forward(),
            );

            let mut txn_batches = Vec::new();
            let mut txns_updates = Vec::new();
            for (data_id, updates) in self.writes.iter() {
                let data_write = handle.datas.get_write(data_id).await;
                // TODO(txn): Tighter lower bound?
                let mut batch = data_write.builder(Antichain::from_elem(T::minimum()));
                for (k, v, d) in updates.iter() {
                    batch.add(k, v, &commit_ts, d).await.expect("valid usage");
                }
                let batch = batch
                    .finish(Antichain::from_elem(commit_ts.step_forward()))
                    .await
                    .expect("valid usage");
                let batch = batch.into_transmittable_batch();
                let batch_raw = batch.encode_to_vec();
                let batch = data_write.batch_from_transmittable_batch(batch);
                txn_batches.push(batch);
                debug!(
                    "wrote {:.9} batch {} len={}",
                    data_id.to_string(),
                    batch_raw.hashed(),
                    updates.len()
                );
                txns_updates.push(C::encode(TxnsEntry::Append(*data_id, batch_raw)));
            }

            let mut txns_updates = txns_updates
                .iter()
                .map(|(key, val)| ((key, val), &commit_ts, 1))
                .collect::<Vec<_>>();
            let apply_is_empty = txns_updates.is_empty();

            // Tidy guarantees that anything in retractions has been applied,
            // but races mean someone else may have written the retraction. If
            // the following CaA goes through, then the `update_ge(txns_upper)`
            // above means that anything the cache thinks is still unapplied
            // but we know is applied indeed still needs to be retracted.
            let filtered_retractions = handle
                .read_cache()
                .filter_retractions(&txns_upper, self.tidy.retractions.iter())
                .map(|(batch_raw, data_id)| {
                    C::encode(TxnsEntry::Append(*data_id, batch_raw.clone()))
                })
                .collect::<Vec<_>>();
            txns_updates.extend(
                filtered_retractions
                    .iter()
                    .map(|(key, val)| ((key, val), &commit_ts, -1)),
            );

            let res = crate::small_caa(
                || "txns commit",
                &mut handle.txns_write,
                &txns_updates,
                txns_upper.clone(),
                commit_ts.step_forward(),
            )
            .await;
            match res {
                Ok(()) => {
                    debug!(
                        "commit_at {:?}: [{:?}, {:?}) success",
                        commit_ts,
                        txns_upper,
                        commit_ts.step_forward(),
                    );
                    // The batch we wrote at commit_ts did commit. Mark it as
                    // such to avoid a WARN in the logs.
                    for batch in txn_batches {
                        let _ = batch.into_hollow_batch();
                    }
                    return Ok(TxnApply {
                        is_empty: apply_is_empty,
                        commit_ts,
                    });
                }
                Err(new_txns_upper) => {
                    assert!(txns_upper < new_txns_upper);
                    txns_upper = new_txns_upper;
                    // The batch we wrote at commit_ts didn't commit. At the
                    // moment, we'll try writing it out again at some higher
                    // commit_ts on the next loop around, so we're free to go
                    // ahead and delete this one. When we do the TODO to
                    // efficiently re-timestamp batches, this must be removed.
                    for batch in txn_batches {
                        let () = batch.delete().await;
                    }
                    let () = handle.txns_cache.update_ge(&txns_upper).await;
                    continue;
                }
            }
        }
    }

    /// Merges the staged writes in the other txn into this one.
    pub fn merge(&mut self, other: Self) {
        for (data_id, writes) in other.writes {
            self.writes.entry(data_id).or_default().extend(writes);
        }
        self.tidy.merge(other.tidy);
    }

    /// Merges the work represented by given tidy into this txn.
    ///
    /// If this txn commits, the tidy work will be written at the commit ts.
    pub fn tidy(&mut self, tidy: Tidy) {
        self.tidy.merge(tidy);
    }

    /// Extracts any tidy work that has been merged into this txn with
    /// [Self::tidy].
    pub fn take_tidy(&mut self) -> Tidy {
        std::mem::take(&mut self.tidy)
    }
}

/// A token representing the asynchronous "apply" work expected to be promptly
/// performed by a txn committer.
#[derive(Debug)]
#[cfg_attr(any(test, debug_assertions), derive(PartialEq))]
pub struct TxnApply<T> {
    is_empty: bool,
    pub(crate) commit_ts: T,
}

impl<T> TxnApply<T> {
    /// Applies the txn, unblocking reads at timestamp it was committed at.
    pub async fn apply<K, V, D, C>(self, handle: &mut TxnsHandle<K, V, T, D, C>) -> Tidy
    where
        K: Debug + Codec,
        V: Debug + Codec,
        T: Timestamp + Lattice + TotalOrder + StepForward + Codec64,
        D: Semigroup + Codec64 + Send + Sync,
        C: TxnsCodec,
    {
        debug!("txn apply {:?}", self.commit_ts);
        handle.apply_le(&self.commit_ts).await
    }

    /// Returns whether the apply represents a txn with any non-tidy writes.
    ///
    /// If this returns true, the apply is essentially a no-op and safe to
    /// discard.
    pub fn is_empty(&self) -> bool {
        self.is_empty
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, SystemTime};

    use futures::stream::FuturesUnordered;
    use futures::StreamExt;
    use mz_persist_client::PersistClient;

    use crate::tests::writer;
    use crate::txn_read::TxnsCache;

    use super::*;

    #[mz_ore::test(tokio::test)]
    #[cfg_attr(miri, ignore)] // too slow
    async fn commit_at() {
        let client = PersistClient::new_for_tests().await;
        let mut txns = TxnsHandle::expect_open(client.clone()).await;
        let mut cache = TxnsCache::expect_open(0, &txns).await;
        let d0 = txns.expect_register(1).await;
        let d1 = txns.expect_register(2).await;

        // Can merge two txns. Can have multiple data shards in a txn.
        let mut txn = txns.begin();
        txn.write(&d0, "0".into(), (), 1).await;
        let mut other = txns.begin();
        other.write(&d0, "1".into(), (), 1).await;
        other.write(&d1, "A".into(), (), 1).await;
        txn.merge(other);
        txn.commit_at(&mut txns, 3).await.unwrap();

        // Can commit an empty txn. Can "skip" timestamps.
        txns.begin().commit_at(&mut txns, 5).await.unwrap();

        // Txn cannot be committed at a closed out time. The Err includes the
        // earliest committable time. Failed txn can commit on retry.
        let mut txn = txns.begin();
        txn.write(&d0, "2".into(), (), 1).await;
        assert_eq!(txn.commit_at(&mut txns, 4).await, Err(6));
        txn.commit_at(&mut txns, 6).await.unwrap();
        txns.apply_le(&6).await;

        let expected_d0 = vec!["0".to_owned(), "1".to_owned(), "2".to_owned()];
        let actual_d0 = cache.expect_snapshot(&client, d0, 6).await;
        assert_eq!(actual_d0, expected_d0);

        let expected_d1 = vec!["A".to_owned()];
        let actual_d1 = cache.expect_snapshot(&client, d1, 6).await;
        assert_eq!(actual_d1, expected_d1);
    }

    #[mz_ore::test(tokio::test)]
    #[cfg_attr(miri, ignore)] // unsupported operation: returning ready events from epoll_wait is not yet implemented
    async fn apply_and_tidy() {
        let mut txns = TxnsHandle::expect_open(PersistClient::new_for_tests().await).await;
        let mut cache = TxnsCache::expect_open(0, &txns).await;
        let d0 = txns.expect_register(1).await;

        // Non-empty txn means non-empty apply. Min unapplied ts is the commit
        // ts.
        let mut txn = txns.begin();
        txn.write(&d0, "2".into(), (), 1).await;
        let apply_2 = txn.commit_at(&mut txns, 2).await.unwrap();
        assert_eq!(apply_2.is_empty(), false);
        cache.update_gt(&2).await;
        assert_eq!(cache.min_unapplied_ts(), &2);
        assert_eq!(cache.unapplied_batches().count(), 1);

        // Running the apply unblocks reads but does not advance the min
        // unapplied ts.
        let tidy_2 = apply_2.apply(&mut txns).await;
        assert_eq!(cache.min_unapplied_ts(), &2);

        // Running the tidy advances the min unapplied ts.
        txns.tidy_at(3, tidy_2).await.unwrap();
        cache.update_gt(&3).await;
        assert_eq!(cache.min_unapplied_ts(), &4);
        assert_eq!(cache.unapplied_batches().count(), 0);

        // We can also sneak the tidy into a normal txn. Tidies copy across txn
        // merges.
        let tidy_4 = txns.expect_commit_at(4, d0, &["4"]).await;
        cache.update_gt(&4).await;
        assert_eq!(cache.min_unapplied_ts(), &4);
        let mut txn0 = txns.begin();
        txn0.write(&d0, "5".into(), (), 1).await;
        txn0.tidy(tidy_4);
        let mut txn1 = txns.begin();
        txn1.merge(txn0);
        let apply_5 = txn1.commit_at(&mut txns, 5).await.unwrap();
        cache.update_gt(&5).await;
        assert_eq!(cache.min_unapplied_ts(), &5);
        let tidy_5 = apply_5.apply(&mut txns).await;

        // It's fine to drop a tidy, someone else will do it eventually.
        let tidy_6 = txns.expect_commit_at(6, d0, &["6"]).await;
        txns.tidy_at(7, tidy_6).await.unwrap();
        cache.update_gt(&7).await;
        assert_eq!(cache.min_unapplied_ts(), &8);

        // Also fine if we don't drop it, but instead do it late (no-op but
        // consumes a ts).
        txns.tidy_at(8, tidy_5).await.unwrap();
        cache.update_gt(&8).await;
        assert_eq!(cache.min_unapplied_ts(), &9);

        // Tidies can be merged and also can be stolen back out of a txn.
        let tidy_9 = txns.expect_commit_at(9, d0, &["9"]).await;
        let tidy_10 = txns.expect_commit_at(10, d0, &["10"]).await;
        let mut txn = txns.begin();
        txn.tidy(tidy_9);
        let mut tidy_9 = txn.take_tidy();
        tidy_9.merge(tidy_10);
        txns.tidy_at(11, tidy_9).await.unwrap();
        cache.update_gt(&11).await;
        assert_eq!(cache.min_unapplied_ts(), &12);

        // Can't tidy at an already committed ts.
        let tidy_12 = txns.expect_commit_at(12, d0, &["12"]).await;
        assert_eq!(txns.tidy_at(12, tidy_12).await, Err(13));
    }

    #[mz_ore::test(tokio::test(flavor = "multi_thread"))]
    #[cfg_attr(miri, ignore)] // too slow
    async fn conflicting_writes() {
        fn jitter() -> u64 {
            // We could also use something like `rand`.
            let time = SystemTime::UNIX_EPOCH.elapsed().unwrap();
            u64::from(time.subsec_micros() % 20)
        }

        let client = PersistClient::new_for_tests().await;
        let mut txns = TxnsHandle::expect_open(client.clone()).await;
        let mut cache = TxnsCache::expect_open(0, &txns).await;
        let d0 = txns.expect_register(1).await;

        const NUM_WRITES: usize = 25;
        let tasks = FuturesUnordered::new();
        for idx in 0..NUM_WRITES {
            let mut txn = txns.begin();
            txn.write(&d0, format!("{:05}", idx), (), 1).await;
            let (txns_id, client) = (txns.txns_id(), client.clone());

            let task = async move {
                let data_write = writer(&client, d0).await;
                let mut txns = TxnsHandle::expect_open_id(client.clone(), txns_id).await;
                let register_ts = txns.register(1, data_write).await.unwrap();
                debug!("{} registered at {}", idx, register_ts);

                // Add some jitter to the commit timestamps (to create gaps) and
                // to the execution (to create interleaving).
                let jitter_ms = jitter();
                let mut commit_ts = register_ts + 1 + jitter_ms;
                let apply = loop {
                    let () = tokio::time::sleep(Duration::from_millis(jitter_ms)).await;
                    match txn.commit_at(&mut txns, commit_ts).await {
                        Ok(apply) => break apply,
                        Err(new_commit_ts) => commit_ts = new_commit_ts,
                    }
                };
                debug!("{} committed at {}", idx, commit_ts);

                // Ditto sleep before apply.
                let () = tokio::time::sleep(Duration::from_millis(jitter_ms)).await;
                let tidy = apply.apply(&mut txns).await;

                // Ditto jitter the tidy timestamps and execution.
                let jitter_ms = jitter();
                let mut txn = txns.begin();
                txn.tidy(tidy);
                let mut tidy_ts = commit_ts + jitter_ms;
                loop {
                    let () = tokio::time::sleep(Duration::from_millis(jitter_ms)).await;
                    match txn.commit_at(&mut txns, tidy_ts).await {
                        Ok(apply) => {
                            debug!("{} tidied at {}", idx, tidy_ts);
                            assert!(apply.is_empty());
                            return commit_ts;
                        }
                        Err(new_tidy_ts) => tidy_ts = new_tidy_ts,
                    }
                }
            };
            tasks.push(task)
        }

        let max_commit_ts = tasks
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .max()
            .unwrap_or_default();

        let expected = (0..NUM_WRITES)
            .map(|x| format!("{:05}", x))
            .collect::<Vec<_>>();
        let actual = cache.expect_snapshot(&client, d0, max_commit_ts).await;
        assert_eq!(actual, expected);
    }

    #[mz_ore::test(tokio::test)]
    #[cfg_attr(miri, ignore)] // too slow
    async fn tidy_race() {
        let client = PersistClient::new_for_tests().await;
        let mut txns0 = TxnsHandle::expect_open(client.clone()).await;
        let d0 = txns0.expect_register(1).await;

        // Commit something and apply it, but don't tidy yet.
        let tidy0 = txns0.expect_commit_at(2, d0, &["foo"]).await;

        // Now open an independent TxnsHandle, commit, apply, and tidy.
        let mut txns1 = TxnsHandle::expect_open_id(client.clone(), txns0.txns_id()).await;
        let d1 = txns1.expect_register(3).await;
        let tidy1 = txns1.expect_commit_at(4, d1, &["foo"]).await;
        let () = txns1.tidy_at(5, tidy1).await.unwrap();

        // Now try the original tidy0. tidy1 has already done the retraction for
        // it, so this needs to be careful not to double-retract.
        let () = txns0.tidy_at(6, tidy0).await.unwrap();

        // Replay a cache from the beginning and make sure we don't see a
        // double retraction.
        let mut cache = TxnsCache::expect_open(0, &txns0).await;
        cache.update_gt(&6).await;
        assert_eq!(cache.validate(), Ok(()));
    }

    // Regression test for a bug caught during code review, where it was
    // possible to commit to an unregistered data shard.
    #[mz_ore::test(tokio::test)]
    #[should_panic(expected = "should be registered")]
    #[cfg_attr(miri, ignore)] // unsupported operation: returning ready events from epoll_wait is not yet implemented
    async fn commit_unregistered_table() {
        let mut txns = TxnsHandle::expect_open(PersistClient::new_for_tests().await).await;
        let d0 = txns.expect_register(2).await;

        let mut txn = txns.begin();
        txn.write(&d0, "foo".into(), (), 1).await;
        // This panics because the commit ts is before the register ts.
        let _ = txn.commit_at(&mut txns, 1).await;
    }
}
