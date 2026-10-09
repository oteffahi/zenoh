//
// Copyright (c) 2026 ZettaScale Technology
//
// This program and the accompanying materials are made available under the
// terms of the Eclipse Public License 2.0 which is available at
// http://www.eclipse.org/legal/epl-2.0, or the Apache License, Version 2.0
// which is available at https://www.apache.org/licenses/LICENSE-2.0.
//
// SPDX-License-Identifier: EPL-2.0 OR Apache-2.0
//
// Contributors:
//   ZettaScale Zenoh Team, <zenoh@zettascale.tech>
//

use std::{
    collections::{btree_map, BTreeMap, HashMap},
    sync::Arc,
    time::Instant,
};

use tokio_util::task::AbortOnDropHandle;
use zenoh::{
    handlers::Callback,
    sample::{Sample, SourceInfo},
    session::EntityGlobalId,
    time::Timestamp,
};

use super::Miss;
use crate::{fragmentation::FragmentedSample, utils::WrappingSn};

pub(super) struct SequencedSource {
    /// Identifies this source state so old fragment queries cannot affect a
    /// source that was removed and later added again.
    pub(super) generation: Arc<()>,
    pub(super) last_delivered: Option<WrappingSn>,
    /// Reject samples at or below this sequence number, even if delivery has
    /// not reached them yet. These samples were removed to limit buffer size.
    pub(super) last_evicted: Option<WrappingSn>,
    /// Number of unfinished history queries, sample-recovery queries, and fragment
    /// recovery attempts for this source.
    /// An attempt counts once, even if it sends queries for several fragment ranges.
    pub(super) pending_queries: u64,
    /// Newest complete sample buffered when a history or sample-recovery query finished.
    /// Delivery can skip missing sequence numbers up to this value, but still
    /// waits for unfinished queries and fragments it can recover.
    pub(super) gap_skip_through: Option<WrappingSn>,
    pub(super) pending_samples: BTreeMap<WrappingSn, FragmentedSample>,
    pub(super) latest_access: Instant,
    pub(super) periodic_task: Option<AbortOnDropHandle<()>>,
    /// Periodically checks all samples for missing fragments. Stops when none
    /// are missing.
    pub(super) frag_recovery_task: Option<AbortOnDropHandle<()>>,
    pub(super) alive: bool,
}

impl Default for SequencedSource {
    fn default() -> Self {
        Self {
            generation: Arc::new(()),
            last_delivered: None,
            last_evicted: None,
            pending_queries: 0,
            gap_skip_through: None,
            pending_samples: BTreeMap::new(),
            latest_access: Instant::now(),
            periodic_task: None,
            frag_recovery_task: None,
            alive: false,
        }
    }
}

impl SequencedSource {
    /// Add a sample or fragment to the buffer. Return `Ok(true)` if its sequence
    /// number was not already buffered, or `Err(())` if the fragment is invalid.
    pub(super) fn insert_sample(
        &mut self,
        sample: Sample,
        source_info: &SourceInfo,
        max_fragments: u32,
    ) -> Result<bool, ()> {
        let sn = source_info.source_sn().into();
        let Some(fi) = sample.frag_info() else {
            let new = !self.pending_samples.contains_key(&sn);
            self.pending_samples
                .insert(sn, FragmentedSample::single(sample));
            return Ok(new);
        };
        let (frag_num, frag_count) = (fi.frag_num(), fi.frag_count());
        let result = match self.pending_samples.entry(sn) {
            btree_map::Entry::Vacant(entry) => {
                // FIXME: incorrect semantics for .map usage
                FragmentedSample::from_first_fragment(sample, frag_num, frag_count, max_fragments)
                    .map(|sample| {
                        entry.insert(sample);
                        true
                    })
            }
            btree_map::Entry::Occupied(mut entry) => entry
                .get_mut()
                .insert(sample, frag_num, frag_count)
                .map(|()| false),
        };
        result.map_err(|error| {
            tracing::warn!("AdvancedSubscriber: rejected fragment (sn={sn}): {error:?}");
        })
    }

    /// Remove the oldest samples until the buffer fits the configured limit.
    /// Deliver complete samples and discard samples with missing fragments.
    /// Report discarded samples as missed only when a newer sample is delivered.
    pub(super) fn enforce_pending_limit(
        &mut self,
        callback: &Callback<Sample>,
        miss_handlers: &HashMap<usize, Callback<Miss>>,
        source_id: EntityGlobalId,
        max_pending_samples: usize,
    ) {
        while self.pending_samples.len() > max_pending_samples.max(1) {
            let (sn, sample) = self
                .pending_samples
                .pop_first()
                .expect("non-empty pending_samples");
            if let Some(sample) = sample.into_sample() {
                self.deliver_and_drain(sample, sn, callback, miss_handlers, source_id);
            } else {
                self.last_evicted = Some(sn);
                tracing::info!(
                    "AdvancedSubscriber: evicted incomplete sample at sn={sn} due to max_pending_samples={max_pending_samples}"
                );
                // Removing an incomplete sample may let us deliver the next one.
                // Try now, even if the buffer is already small enough.
                if let Some((sn, sample)) = self.pop_next_ready() {
                    self.deliver_and_drain(sample, sn, callback, miss_handlers, source_id);
                }
            }
        }
    }

    /// Find the next sequence number to deliver, skipping samples we discarded
    /// or gave up recovering. Before the first delivery, start with the oldest
    /// buffered sample we have not given up on.
    pub(super) fn next_expected(&self) -> Option<WrappingSn> {
        let Some(last) = self.last_delivered else {
            return self
                .pending_samples
                .iter()
                .find(|(_, sample)| !sample.is_abandoned())
                .map(|(sn, _)| *sn);
        };
        let mut next = last + 1;
        if let Some(evicted) = self.last_evicted {
            if evicted >= next {
                next = evicted + 1;
            }
        }
        while self
            .pending_samples
            .get(&next)
            .is_some_and(FragmentedSample::is_abandoned)
        {
            next += 1;
        }
        Some(next)
    }

    /// Take the next sample if it is complete. Wait if that sample has not
    /// arrived or still has fragments we can recover.
    pub(super) fn pop_next_ready(&mut self) -> Option<(WrappingSn, Sample)> {
        self.abandon_failed_head();
        let sn = self.next_expected()?;
        self.take_complete(sn).map(|sample| (sn, sample))
    }

    /// Give up recovering the oldest incomplete sample if recovery failed and
    /// a newer complete sample is waiting. Samples we have not received at all
    /// still need a sample-recovery query.
    pub(super) fn abandon_failed_head(&mut self) {
        loop {
            let Some((&sn, sample)) = self.pending_samples.iter().find(|(_, s)| !s.is_abandoned())
            else {
                return;
            };
            if !sample.can_abandon()
                || !self
                    .pending_samples
                    .range((std::ops::Bound::Excluded(sn), std::ops::Bound::Unbounded))
                    .any(|(_, sample)| sample.is_complete())
            {
                return;
            }
            // If an earlier sample is entirely missing, first query for it.
            // A completed query may already allow skipping that sequence number.
            if self.next_expected().is_some_and(|next| sn > next)
                && !self.gap_skip_through.is_some_and(|through| sn <= through)
            {
                return;
            }
            self.pending_samples.insert(sn, FragmentedSample::Abandoned);
        }
    }

    pub(super) fn take_complete(&mut self, sn: WrappingSn) -> Option<Sample> {
        if let btree_map::Entry::Occupied(entry) = self.pending_samples.entry(sn) {
            if entry.get().is_complete() {
                return Some(entry.remove().into_sample().unwrap());
            }
        }
        None
    }

    pub(super) fn deliver_and_drain(
        &mut self,
        sample: Sample,
        source_sn: impl Into<WrappingSn>,
        callback: &Callback<Sample>,
        miss_handlers: &HashMap<usize, Callback<Miss>>,
        source_id: EntityGlobalId,
    ) {
        let mut ready = Some((source_sn.into(), sample));
        while let Some((sn, sample)) = ready {
            // Count missed samples between this delivery and the previous one.
            // This also counts samples we discarded or gave up recovering.
            // Before the first delivery, we do not know which samples were missed.
            if let Some(last) = self.last_delivered {
                if sn > last + 1 {
                    let missed = sn - last - 1;
                    tracing::info!("Sample missed: missed {missed} samples from {source_id:?}.");
                    for callback in miss_handlers.values() {
                        callback.call(Miss {
                            source: source_id,
                            nb: missed,
                        });
                    }
                }
            }
            callback.call(sample);
            self.last_delivered = Some(sn);
            // last_delivered now rejects these old samples, so last_evicted is
            // no longer needed. Keeping it could reject new samples later as
            // the wrapping sequence counter advances.
            if self.last_evicted.is_some_and(|evicted| sn >= evicted) {
                self.last_evicted = None;
            }
            while self
                .pending_samples
                .first_key_value()
                .is_some_and(|(key, _)| *key <= sn)
            {
                self.pending_samples.pop_first();
            }
            ready = self.pop_next_ready();
        }
        if !self
            .pending_samples
            .values()
            .any(FragmentedSample::is_incomplete)
        {
            self.frag_recovery_task = None;
        }
    }

    /// After a history or sample-recovery query finishes, remember the newest
    /// complete sample in the buffer. Once unfinished queries and earlier missing
    /// fragments are resolved, delivery can skip missing sequence numbers up to
    /// that sample.
    ///
    /// For example, a limit of 13 lets delivery skip a missing sample 12 to
    /// deliver sample 13. If sample 15 arrives later, skipping a missing sample
    /// 14 still requires another history or sample-recovery query.
    pub(super) fn authorize_gap_skip(&mut self) {
        if let Some(last_complete) = self
            .pending_samples
            .iter()
            .rev()
            .find_map(|(sn, sample)| sample.is_complete().then_some(*sn))
        {
            self.gap_skip_through = Some(
                self.gap_skip_through
                    .map_or(last_complete, |old| old.max(last_complete)),
            );
        }
    }

    pub(super) fn drain_authorized_samples(
        &mut self,
        callback: &Callback<Sample>,
        miss_handlers: &HashMap<usize, Callback<Miss>>,
        source_id: EntityGlobalId,
        retransmission: bool,
    ) {
        if self.pending_queries != 0 {
            return;
        }
        let Some(through) = self.gap_skip_through else {
            return;
        };
        loop {
            if retransmission {
                self.abandon_failed_head();
            }
            if self.last_delivered.is_some_and(|last| last >= through)
                || self.last_evicted.is_some_and(|last| last >= through)
            {
                self.gap_skip_through = None;
                return;
            }
            // Keep the Abandoned entries until a newer sample is delivered.
            // They prevent late fragments from recreating samples we gave up on.
            let Some((&sn, sample)) = self
                .pending_samples
                .iter()
                .find(|(sn, sample)| **sn <= through && !sample.is_abandoned())
            else {
                self.gap_skip_through = None;
                return;
            };
            if retransmission && sample.is_incomplete() {
                return;
            }
            let sample = self.pending_samples.remove(&sn).unwrap();
            if let Some(sample) = sample.into_sample() {
                if self.last_delivered.map_or(true, |last| sn > last) {
                    self.deliver_and_drain(sample, sn, callback, miss_handlers, source_id);
                }
            }
        }
    }
}

/// Buffers samples by timestamp while history queries are running.
/// Fragment recovery is only supported for sources with sequence numbers.
pub(super) struct TimestampedSource {
    pub(super) last_delivered: Option<Timestamp>,
    pub(super) pending_queries: u64,
    pub(super) pending_samples: BTreeMap<Timestamp, Sample>,
    pub(super) latest_access: Instant,
    pub(super) alive: bool,
}

impl Default for TimestampedSource {
    fn default() -> Self {
        Self {
            last_delivered: None,
            pending_queries: 0,
            pending_samples: BTreeMap::new(),
            latest_access: Instant::now(),
            alive: false,
        }
    }
}

impl TimestampedSource {
    pub(super) fn flush(&mut self, callback: Option<&Callback<Sample>>) {
        let Some(callback) = callback else {
            return;
        };
        if self.pending_queries == 0 && !self.pending_samples.is_empty() {
            for (timestamp, sample) in std::mem::take(&mut self.pending_samples) {
                if self.last_delivered.map_or(true, |last| timestamp > last) {
                    self.last_delivered = Some(timestamp);
                    callback.call(sample);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_layout_does_not_grow() {
        // Compare against the old source struct rather than hard-coded byte
        // counts, since type sizes can differ between platforms.
        #[allow(dead_code)]
        struct PreviousSource<T> {
            generation: Arc<()>,
            last_delivered: Option<T>,
            last_evicted: Option<T>,
            pending_queries: u64,
            pending_flush: Option<WrappingSn>,
            pending_samples: BTreeMap<T, FragmentedSample>,
            latest_access: Instant,
            periodic_task: Option<AbortOnDropHandle<()>>,
            frag_recovery_task: Option<AbortOnDropHandle<()>>,
            alive: bool,
        }
        use std::mem::size_of;
        assert!(size_of::<SequencedSource>() <= size_of::<PreviousSource<WrappingSn>>());
        assert!(size_of::<TimestampedSource>() < size_of::<PreviousSource<Timestamp>>());
        assert!(size_of::<Sample>() <= size_of::<FragmentedSample>());
        println!(
            "source sizes: sequenced {} -> {}, timestamped {} -> {}; buffered sample {} -> {}",
            size_of::<PreviousSource<WrappingSn>>(),
            size_of::<SequencedSource>(),
            size_of::<PreviousSource<Timestamp>>(),
            size_of::<TimestampedSource>(),
            size_of::<FragmentedSample>(),
            size_of::<Sample>()
        );
    }
}
