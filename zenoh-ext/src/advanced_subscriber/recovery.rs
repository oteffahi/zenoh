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

//! Queries used by the advanced subscriber:
//! - History queries request previously published samples, optionally limited by count or age.
//! - Sample-recovery queries request samples by sequence number (`_sn`).
//! - Fragment-recovery queries request fragment ranges (`_fn`) of one sample (`_sn`).
//!
//! Sample-recovery queries run after a sequence-number gap, a periodic timer tick,
//! or a heartbeat. Both history and sample-recovery queries can receive fragmented replies.

use std::{
    future::Future,
    sync::{Arc, Mutex},
    time::Duration,
};

use tokio_util::task::AbortOnDropHandle;
use uhlc::ID;
use zenoh::{
    internal::{runtime::ZRuntime, zlock},
    key_expr::KeyExpr,
    query::{
        ConsolidationMode, Parameters, QueryTarget, Reply, ReplyKeyExpr, Selector, TimeBound,
        TimeExpr, TimeRange, ZenohParameters,
    },
    sample::Sample,
    session::{EntityGlobalId, WeakSession},
    Wait, KE_ADV_PREFIX, KE_STAR, KE_STARSTAR,
};

use super::{handle_sample, range, HistoryConfig, SequencedSource, State};
use crate::{
    fragmentation::{FragRange, FragmentedSample},
    utils::WrappingSn,
};

/// Query settings copied while holding the state lock, so queries can be sent
/// after releasing it.
pub(super) struct QueryContext {
    session: WeakSession,
    key_expr: KeyExpr<'static>,
    target: QueryTarget,
    timeout: Duration,
}

impl QueryContext {
    /// Send the fragment queries selected by a timer tick, building the source
    /// key once for all requests.
    pub(super) fn issue_fragments(
        &self,
        statesref: &Arc<Mutex<State>>,
        source_id: EntityGlobalId,
        generation: &Arc<()>,
        requests: impl IntoIterator<Item = FragmentRequest>,
    ) {
        let query_expr = self.source_key(source_id);
        for request in requests {
            request
                .into_attempt(statesref, source_id, generation)
                .issue(self, &query_expr);
        }
    }

    /// Prepare a task for a query triggered by a received fragment. Send it on
    /// the application runtime so local queryables cannot re-enter the caller.
    /// Call this after releasing the state lock.
    pub(super) fn fragment_query_task(
        self,
        statesref: &Arc<Mutex<State>>,
        source_id: EntityGlobalId,
        generation: &Arc<()>,
        request: FragmentRequest,
    ) -> impl Future<Output = ()> {
        // Create the handler before the task starts. If the task is cancelled
        // before its first poll, dropping the handler still clears its query count.
        let attempt = request.into_attempt(statesref, source_id, generation);
        async move {
            let query_expr = self.source_key(source_id);
            attempt.issue(&self, &query_expr);
        }
    }

    pub(super) fn new(state: &State) -> Self {
        Self {
            session: state.session.clone(),
            key_expr: state.key_expr.clone(),
            target: state.query_target,
            timeout: state.query_timeout,
        }
    }

    fn source_key(&self, source: EntityGlobalId) -> KeyExpr<'static> {
        &self.key_expr
            / KE_ADV_PREFIX
            / KE_STAR
            / &source.zid().into_keyexpr()
            / &KeyExpr::try_from(source.eid().to_string()).unwrap()
            / KE_STARSTAR
    }

    /// Send a query and pass only samples matching the subscription key to the callback.
    pub(super) fn issue(
        &self,
        selector: Selector<'_>,
        callback: impl Fn(Sample) + Send + Sync + 'static,
    ) {
        tracing::trace!(
            "AdvancedSubscriber{{key_expr: {}}}: Querying {}",
            self.key_expr,
            selector
        );
        // Sending a query can immediately call or drop its reply callback.
        // Both need the state lock, so the caller must release it first.
        let key_expr = self.key_expr.clone();
        let _ = self
            .session
            .get(selector)
            .callback(move |reply: Reply| {
                if let Ok(sample) = reply.into_result() {
                    // Reject unrelated keys before calling any handler that
                    // locks the state. A local reply can be sent from a sample
                    // callback that already holds that lock.
                    if key_expr.intersects(sample.key_expr()) {
                        callback(sample);
                    }
                }
            })
            .consolidation(ConsolidationMode::None)
            .accept_replies(ReplyKeyExpr::Any)
            .target(self.target)
            .timeout(self.timeout)
            .wait();
    }
}

impl State {
    fn receive_query_sample(statesref: &Mutex<Self>, sample: Sample) {
        let states = &mut *zlock!(statesref);
        tracing::trace!(
            "AdvancedSubscriber{{key_expr: {}}}: Received reply with Sample{{info:{:?}, ts:{:?}}}",
            states.key_expr,
            sample.source_info(),
            sample.timestamp()
        );
        handle_sample(states, sample);
    }

    pub(super) fn on_history_or_sample_recovery_finished(
        &mut self,
        statesref: &Arc<Mutex<State>>,
        source_id: &EntityGlobalId,
    ) {
        let Some(callback) = self.callback.as_ref() else {
            return;
        };
        let Some(source) = self.sequenced_states.peek_mut(source_id) else {
            return;
        };
        source.authorize_gap_skip();
        source.drain_authorized_samples(
            callback,
            &self.miss_handlers,
            *source_id,
            self.retransmission,
        );
        // These fragments may have arrived only through query replies. Start
        // the recovery timer here in case no publication started it earlier.
        if self.retransmission
            && source
                .pending_samples
                .values()
                .any(FragmentedSample::is_incomplete)
        {
            source.arm_fragment_recovery(statesref, *source_id, self.frag_recovery_delay);
        }
    }
}

/// Keeps a history query counted as pending until its reply callback is dropped.
/// Do not clone query handlers: each drop decreases the pending-query count.
/// Fragment queries use an Arc to share one handler across several callbacks.
pub(super) struct InitialRepliesHandler {
    pub(super) statesref: Arc<Mutex<State>>,
}

pub(super) struct SequencedRepliesHandler {
    pub(super) source_id: EntityGlobalId,
    pub(super) statesref: Arc<Mutex<State>>,
}

pub(super) struct TimestampedRepliesHandler {
    pub(super) id: ID,
    pub(super) statesref: Arc<Mutex<State>>,
}

/// Tracks one attempt to recover a sample's missing fragments. All range
/// queries share this handler, so the attempt ends only after they all finish.
/// Fragment recovery alone cannot skip samples that have not arrived at all;
/// that requires a history or sample-recovery query.
pub(super) struct FragmentAttempt {
    pub(super) source_id: EntityGlobalId,
    pub(super) statesref: Arc<Mutex<State>>,
    pub(super) generation: Arc<()>,
    pub(super) sn: WrappingSn,
    pub(super) ranges: Vec<FragRange>,
    pub(super) attempt: Arc<()>,
}

impl Drop for InitialRepliesHandler {
    fn drop(&mut self) {
        self.complete();
    }
}

impl Drop for SequencedRepliesHandler {
    fn drop(&mut self) {
        self.complete();
    }
}

impl Drop for TimestampedRepliesHandler {
    fn drop(&mut self) {
        self.complete();
    }
}

impl Drop for FragmentAttempt {
    fn drop(&mut self) {
        self.complete();
    }
}

impl HistoryConfig {
    pub(super) fn parameters(&self) -> Parameters<'static> {
        let mut params = Parameters::empty();
        if let Some(max) = self.max_samples {
            params.insert("_max", max.to_string());
        }
        if let Some(age) = self.max_age {
            params.set_time_range(TimeRange {
                start: TimeBound::Inclusive(TimeExpr::Now { offset_secs: -age }),
                end: TimeBound::Unbounded,
            });
        }
        params
    }
}

impl InitialRepliesHandler {
    fn complete(&self) {
        let states = &mut *zlock!(self.statesref);
        states.global_pending_queries = states.global_pending_queries.saturating_sub(1);
        if states.global_pending_queries == 0 {
            let source_ids: Vec<_> = states.sequenced_states.iter().map(|(id, _)| *id).collect();
            for source_id in source_ids {
                states.on_history_or_sample_recovery_finished(&self.statesref, &source_id);
                let source = states.sequenced_states.peek_mut(&source_id).unwrap();
                source.periodic_task = SequencedRepliesHandler::spawn_periodic(
                    &self.statesref,
                    states.period,
                    source_id,
                );
            }
            for (_, source) in states.timestamped_states.iter_mut() {
                source.flush(states.callback.as_ref());
            }
        }
    }

    pub(super) fn issue(self, context: &QueryContext, selector: Selector<'_>) {
        context.issue(selector, move |sample| self.reply(sample));
    }

    fn reply(&self, sample: Sample) {
        State::receive_query_sample(&self.statesref, sample);
    }
}

impl TimestampedRepliesHandler {
    fn complete(&self) {
        let states = &mut *zlock!(self.statesref);
        if let Some(source) = states.timestamped_states.peek_mut(&self.id) {
            source.pending_queries = source.pending_queries.saturating_sub(1);
            if states.global_pending_queries == 0 {
                source.flush(states.callback.as_ref());
            }
        }
    }

    pub(super) fn issue(self, context: &QueryContext, selector: Selector<'_>) {
        context.issue(selector, move |sample| self.reply(sample));
    }

    fn reply(&self, sample: Sample) {
        State::receive_query_sample(&self.statesref, sample);
    }
}

impl SequencedRepliesHandler {
    fn complete(&self) {
        let states = &mut *zlock!(self.statesref);
        // Finishing a query without receiving samples must not keep an idle
        // source in the cache longer.
        if let Some(source) = states.sequenced_states.peek_mut(&self.source_id) {
            source.pending_queries = source.pending_queries.saturating_sub(1);
            if states.global_pending_queries == 0 {
                states.on_history_or_sample_recovery_finished(&self.statesref, &self.source_id);
            }
        }
    }

    pub(super) fn issue(self, context: &QueryContext, selector: Selector<'_>) {
        context.issue(selector, move |sample| self.reply(sample));
    }

    fn reply(&self, sample: Sample) {
        State::receive_query_sample(&self.statesref, sample);
    }

    /// Check for missing samples and increase pending_queries under the same
    /// lock, so two callers cannot start the same query. Send it after unlocking.
    pub(super) fn recover_gap(statesref: &Arc<Mutex<State>>, source_id: EntityGlobalId) {
        let (context, start) = {
            let mut states = zlock!(statesref);
            if !states.retransmission || states.callback.is_none() {
                return;
            }
            // Use peek_mut: this check must not recreate a removed source or
            // make an idle source look recently used.
            let Some(source) = states.sequenced_states.peek_mut(&source_id) else {
                return;
            };
            if source.last_delivered.is_none() || source.pending_queries != 0 {
                return;
            }
            let start = source.next_expected().unwrap();
            // If we already have some fragments of the next sample, use
            // fragment recovery rather than a sample-recovery query.
            if source
                .pending_samples
                .iter()
                .find(|(_, s)| !s.is_abandoned())
                .map_or(true, |(sn, _)| *sn <= start)
            {
                return;
            }
            source.pending_queries += 1;
            (QueryContext::new(&states), start)
        };
        Self {
            source_id,
            statesref: statesref.clone(),
        }
        .issue(
            &context,
            Selector::from((
                context.source_key(source_id),
                range("_sn", Some(start), None),
            )),
        );
    }

    pub(super) fn periodic(statesref: &Arc<Mutex<State>>, source_id: EntityGlobalId) {
        let (context, start) = {
            let mut states = zlock!(statesref);
            if states.global_pending_queries != 0 {
                return;
            }
            let Some(source) = states.sequenced_states.peek_mut(&source_id) else {
                return;
            };
            // Skip this tick if a query or fragment recovery attempt is running.
            if source.pending_queries != 0 {
                return;
            }
            source.pending_queries += 1;
            let start = source.last_delivered.map(|sn| sn + 1);
            (QueryContext::new(&states), start)
        };
        Self {
            source_id,
            statesref: statesref.clone(),
        }
        .issue(
            &context,
            Selector::from((context.source_key(source_id), range("_sn", start, None))),
        );
    }

    pub(super) fn spawn_periodic(
        statesref: &Arc<Mutex<State>>,
        period: Option<Duration>,
        source_id: EntityGlobalId,
    ) -> Option<AbortOnDropHandle<()>> {
        let period = period?;
        let statesref = statesref.clone();
        Some(AbortOnDropHandle::new(ZRuntime::Application.spawn(
            async move {
                let mut interval = tokio::time::interval(period);
                interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                interval.tick().await;
                loop {
                    interval.tick().await;
                    Self::periodic(&statesref, source_id);
                }
            },
        )))
    }
}

/// Missing fragments to request for one sample. After unlocking, move this into
/// a FragmentAttempt before scheduling a task or awaiting. The attempt's Drop
/// releases its pending-query count if the task is cancelled.
pub(super) struct FragmentRequest {
    sn: WrappingSn,
    ranges: Vec<FragRange>,
    attempt: Arc<()>,
}

impl FragmentRequest {
    fn into_attempt(
        self,
        statesref: &Arc<Mutex<State>>,
        source_id: EntityGlobalId,
        generation: &Arc<()>,
    ) -> FragmentAttempt {
        FragmentAttempt {
            statesref: statesref.clone(),
            source_id,
            generation: generation.clone(),
            sn: self.sn,
            ranges: self.ranges,
            attempt: self.attempt,
        }
    }
}

impl SequencedSource {
    /// Start one timer to check all samples from this source for missing fragments.
    /// Each tick selects queries and updates pending_queries under the lock,
    /// then sends the queries after unlocking, without awaiting in between.
    pub(super) fn arm_fragment_recovery(
        &mut self,
        statesref: &Arc<Mutex<State>>,
        source_id: EntityGlobalId,
        delay: Duration,
    ) {
        if self
            .frag_recovery_task
            .as_ref()
            .is_some_and(|h| !h.is_finished())
        {
            return;
        }
        let statesref = statesref.clone();
        let generation = self.generation.clone();
        self.frag_recovery_task = Some(AbortOnDropHandle::new(ZRuntime::Application.spawn(
            async move {
                let mut interval = tokio::time::interval(delay);
                interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                interval.tick().await;
                loop {
                    interval.tick().await;
                    let (requests, context) = {
                        let mut states = zlock!(statesref);
                        let Some(source) = states.sequenced_states.get_mut(&source_id) else {
                            return;
                        };
                        if !Arc::ptr_eq(&source.generation, &generation) {
                            return;
                        }
                        if !source
                            .pending_samples
                            .values()
                            .any(FragmentedSample::is_incomplete)
                        {
                            return;
                        }
                        let requests = source.prepare_fragment_scan(delay);
                        if requests.is_empty() {
                            continue;
                        }
                        (requests, QueryContext::new(&states))
                    };
                    context.issue_fragments(&statesref, source_id, &generation, requests);
                }
            },
        )));
    }

    pub(super) fn on_fragment(
        &mut self,
        statesref: &Arc<Mutex<State>>,
        source_id: EntityGlobalId,
        sn: WrappingSn,
        new_slot: bool,
        delay: Duration,
    ) -> Option<FragmentRequest> {
        if new_slot {
            self.arm_fragment_recovery(statesref, source_id, delay);
        }
        let sample = self.pending_samples.get_mut(&sn)?;
        if self.pending_queries != 0 && sample.is_retry() {
            return None;
        }
        let (ranges, attempt) = sample.prepare_recovery(delay, false)?;
        self.pending_queries += 1;
        Some(FragmentRequest {
            sn,
            ranges,
            attempt,
        })
    }

    fn prepare_fragment_scan(&mut self, delay: Duration) -> Vec<FragmentRequest> {
        let mut requests = Vec::new();
        // Check once, so queries selected by this tick do not prevent other
        // samples from being retried in the same tick.
        let queries_pending = self.pending_queries != 0;
        for (&sn, sample) in &mut self.pending_samples {
            // Let running queries finish and deliver buffered samples before retrying.
            if queries_pending && sample.is_retry() {
                continue;
            }
            if let Some((ranges, attempt)) = sample.prepare_recovery(delay, true) {
                requests.push(FragmentRequest {
                    sn,
                    ranges,
                    attempt,
                });
            }
        }
        self.pending_queries += requests.len() as u64;
        requests
    }
}

impl FragmentAttempt {
    fn complete(&self) {
        let should_recheck = {
            let states = &mut *zlock!(self.statesref);
            let Some(source) = states.sequenced_states.peek_mut(&self.source_id) else {
                return;
            };
            if !Arc::ptr_eq(&source.generation, &self.generation) {
                return;
            }
            source.pending_queries = source.pending_queries.saturating_sub(1);
            if let Some(sample) = source.pending_samples.get_mut(&self.sn) {
                sample.finish_recovery(&self.attempt, &self.ranges);
            }
            if states.global_pending_queries == 0 {
                source.abandon_failed_head();
            }
            let last_query = source.pending_queries == 0;
            if last_query && states.global_pending_queries == 0 {
                if let Some(callback) = states.callback.as_ref() {
                    source.drain_authorized_samples(
                        callback,
                        &states.miss_handlers,
                        self.source_id,
                        states.retransmission,
                    );
                    if let Some((sn, sample)) = source.pop_next_ready() {
                        source.deliver_and_drain(
                            sample,
                            sn,
                            callback,
                            &states.miss_handlers,
                            self.source_id,
                        );
                    }
                }
            }
            last_query
        };
        if should_recheck {
            let statesref = Arc::downgrade(&self.statesref);
            let source_id = self.source_id;
            // Run the next gap check in a separate task. Sending a query here
            // could immediately finish another query and call this method again.
            // recover_gap checks whether a query is still needed before sending it.
            ZRuntime::Application.spawn(async move {
                if let Some(statesref) = statesref.upgrade() {
                    SequencedRepliesHandler::recover_gap(&statesref, source_id);
                }
            });
        }
    }

    fn issue(self, context: &QueryContext, query_expr: &KeyExpr<'static>) {
        let seq_num_range = range("_sn", Some(self.sn), Some(self.sn));
        let attempt = Arc::new(self);
        // Keep this Arc until all range queries have been sent. Otherwise, if
        // the first query finishes immediately, it could end the whole attempt
        // before we send the remaining queries.
        for &(start, end) in &attempt.ranges {
            let params = seq_num_range.clone()
                + ";"
                + &range("_fn", start.map(Into::into), end.map(Into::into));
            let attempt = attempt.clone();
            context.issue(
                Selector::from((query_expr.clone(), params)),
                move |sample| attempt.reply(sample),
            );
        }
    }

    fn reply(&self, sample: Sample) {
        if !sample.source_info().is_some_and(|info| {
            *info.source_id() == self.source_id && WrappingSn::from(info.source_sn()) == self.sn
        }) {
            return;
        }
        let mut states = zlock!(self.statesref);
        let current = states
            .sequenced_states
            .peek(&self.source_id)
            .is_some_and(|source| {
                Arc::ptr_eq(&source.generation, &self.generation)
                    && source
                        .pending_samples
                        .get(&self.sn)
                        .is_some_and(|s| s.recovery_matches(&self.attempt))
            });
        if current {
            handle_sample(&mut states, sample);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use zenoh::{
        bytes::ZBytes,
        internal::ztimeout,
        sample::{FragInfo, Sample, SampleBuilder, SourceInfo},
        Config,
    };

    use super::*;
    use crate::{
        advanced_subscriber::tests::frag_recovery_state, AdvancedSubscriberBuilderExt,
        RecoveryConfig,
    };

    const TIMEOUT: Duration = Duration::from_secs(10);

    fn fragment(key: &KeyExpr<'static>, source: EntityGlobalId, num: u32) -> Sample {
        SampleBuilder::put(key.clone(), "x")
            .source_info(SourceInfo::new(source, 0))
            .frag_info(FragInfo::new(5, num))
            .into()
    }

    async fn check_nonmatching_reply_from_sample_callback(fragment_recovery: bool) {
        let mut config = Config::default();
        config.scouting.multicast.set_enabled(Some(false)).unwrap();
        config.listen.endpoints.set(vec![]).unwrap();
        let session = ztimeout!(zenoh::open(config)).unwrap();
        let source_id = EntityGlobalId::new(session.zid(), 7);
        let live_source_id = EntityGlobalId::new(session.zid(), 8);
        let key = KeyExpr::try_from("test/ext/recovery/nonmatching").unwrap();
        let cache = session
            .declare_queryable("test/ext/recovery/nonmatching/@adv/**")
            .wait()
            .unwrap();
        let retained_query = Arc::new(Mutex::new(None::<zenoh::query::Query>));
        let (delivered_tx, delivered_rx) = flume::unbounded();
        let (checked_tx, checked_rx) = flume::bounded(1);
        let builder = session
            .declare_subscriber(key.clone())
            .advanced()
            .query_timeout(Duration::from_secs(60));
        let builder = if fragment_recovery {
            builder.recovery(
                RecoveryConfig::default().fragments_recovery_delay(Duration::from_secs(60)),
            )
        } else {
            builder.history(HistoryConfig::default().max_samples(1))
        };
        let _sub = builder
            .callback({
                let retained_query = retained_query.clone();
                move |sample| {
                    if sample
                        .source_info()
                        .is_some_and(|info| *info.source_id() == live_source_id)
                    {
                        let query = retained_query.lock().unwrap().as_ref().unwrap().clone();
                        // Use the requested source and sequence number so only the
                        // key filter can reject this fragment reply before locking.
                        let unrelated: Sample = SampleBuilder::put(
                            KeyExpr::try_from("test/ext/recovery/unrelated").unwrap(),
                            "ignored",
                        )
                        .source_info(SourceInfo::new(source_id, 0))
                        .frag_info(FragInfo::new(2, 0))
                        .into();
                        let (finished, result) = std::sync::mpsc::channel();
                        let worker = std::thread::spawn({
                            let query = query.clone();
                            let unrelated = unrelated.clone();
                            move || {
                                let replied = query.reply_sample(unrelated).wait().is_ok();
                                let _ = finished.send(replied);
                            }
                        });
                        // This callback holds the subscriber lock. First check on
                        // another thread that rejecting the reply needs no lock.
                        // On failure, return and release the lock so the test can
                        // report the regression instead of hanging forever.
                        let rejected_without_lock =
                            result.recv_timeout(Duration::from_secs(2)).unwrap_or(false);
                        let replied_inline =
                            rejected_without_lock && query.reply_sample(unrelated).wait().is_ok();
                        checked_tx.send((replied_inline, worker)).unwrap();
                    }
                    delivered_tx.send(sample).unwrap();
                }
            })
            .wait()
            .unwrap();

        if fragment_recovery {
            ztimeout!(session
                .put(&key, "B")
                .source_info(SourceInfo::new(source_id, 0))
                .frag_info(FragInfo::new(2, 1)))
            .unwrap();
        }
        let query = ztimeout!(cache.recv_async()).unwrap();
        if fragment_recovery {
            assert_eq!(query.parameters().get("_fn"), Some("0..0"));
        } else {
            assert_eq!(query.parameters().get("_max"), Some("1"));
        }
        *retained_query.lock().unwrap() = Some(query.clone());
        // History depth 1 allows this live sample through while the history
        // query is held. A different source also lets it through while the
        // fragment query is held, using the same subscriber mutex.
        ztimeout!(session
            .put(&key, "live")
            .source_info(SourceInfo::new(live_source_id, 0)))
        .unwrap();
        let (replied_inline, worker) = ztimeout!(checked_rx.recv_async()).unwrap();
        worker.join().unwrap();
        assert!(
            replied_inline,
            "nonmatching reply waited for the subscriber lock"
        );
        assert_eq!(
            ztimeout!(delivered_rx.recv_async())
                .unwrap()
                .payload()
                .try_to_string()
                .unwrap(),
            "live"
        );
        assert!(delivered_rx.try_recv().is_err());

        // Matching replies must still be delivered after ignoring unrelated ones.
        let reply = SampleBuilder::put(key, if fragment_recovery { "A" } else { "recovered" })
            .source_info(SourceInfo::new(source_id, 0));
        let reply: Sample = if fragment_recovery {
            reply.frag_info(FragInfo::new(2, 0)).into()
        } else {
            reply.into()
        };
        ztimeout!(query.reply_sample(reply)).unwrap();
        let sample = ztimeout!(delivered_rx.recv_async()).unwrap();
        assert_eq!(
            sample.payload().try_to_string().unwrap(),
            if fragment_recovery { "AB" } else { "recovered" }
        );
        assert!(delivered_rx.try_recv().is_err());
        retained_query.lock().unwrap().take();
        drop(query);
        ztimeout!(session.close()).unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn nonmatching_history_reply_from_sample_callback() {
        check_nonmatching_reply_from_sample_callback(false).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn nonmatching_fragment_reply_from_sample_callback() {
        check_nonmatching_reply_from_sample_callback(true).await;
    }

    // Cancel the task before it polls the query future. The query handler must
    // still release the count even though no request was sent.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancelled_fragment_query_releases_pending_count() {
        let mut config = Config::default();
        config.scouting.multicast.set_enabled(Some(false)).unwrap();
        config.listen.endpoints.set(vec![]).unwrap();
        let session = ztimeout!(zenoh::open(config)).unwrap();
        let source_id = EntityGlobalId::new(session.zid(), 7);
        let key = KeyExpr::try_from("test/ext/dispatch/cancelled").unwrap();
        let statesref = frag_recovery_state(&session, &key, Arc::new(Mutex::new(Vec::new())));
        let cache = session
            .declare_queryable("test/ext/dispatch/cancelled/@adv/**")
            .wait()
            .unwrap();
        let (request, generation, context) = {
            let mut states = zlock!(statesref);
            for num in [0, 2, 4] {
                handle_sample(&mut states, fragment(&key, source_id, num));
            }
            let source = states.sequenced_states.peek_mut(&source_id).unwrap();
            let request = source
                .on_fragment(&statesref, source_id, WrappingSn(0), false, Duration::ZERO)
                .unwrap();
            assert_eq!(source.pending_queries, 1);
            (
                request,
                source.generation.clone(),
                QueryContext::new(&states),
            )
        };
        let query_task = context.fragment_query_task(&statesref, source_id, &generation, request);
        let (started, ready) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            started.send(()).unwrap();
            std::future::pending::<()>().await;
            query_task.await;
        });
        ztimeout!(ready).unwrap();
        task.abort();
        assert!(ztimeout!(task).unwrap_err().is_cancelled());
        {
            let states = zlock!(statesref);
            let source = states.sequenced_states.peek(&source_id).unwrap();
            assert_eq!(source.pending_queries, 0);
            assert!(source.pending_samples[&WrappingSn(0)].is_retry());
        }
        assert!(cache.try_recv().unwrap().is_none());
        ztimeout!(session.close()).unwrap();
    }

    /// Even if the first query finishes immediately or sending fails, keep the
    /// attempt counted as pending until all range queries have been sent.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn fragment_dispatch_releases_exactly_one_reservation() {
        for closed_session in [false, true] {
            let mut config = Config::default();
            config.scouting.multicast.set_enabled(Some(false)).unwrap();
            config.listen.endpoints.set(vec![]).unwrap();
            let session = ztimeout!(zenoh::open(config)).unwrap();
            let source_id = EntityGlobalId::new(session.zid(), 7);
            let key = KeyExpr::try_from("test/ext/dispatch/completion").unwrap();
            let statesref = frag_recovery_state(&session, &key, Arc::new(Mutex::new(Vec::new())));
            let queries = Arc::new(AtomicUsize::new(0));
            let _cache = session
                .declare_queryable("test/ext/dispatch/completion/@adv/**")
                .callback({
                    let queries = queries.clone();
                    let statesref = statesref.clone();
                    move |query| {
                        {
                            let states = zlock!(statesref);
                            let source = states.sequenced_states.peek(&source_id).unwrap();
                            assert_eq!(source.pending_queries, 1);
                            assert!(!source.pending_samples[&WrappingSn(0)].is_retry());
                        }
                        queries.fetch_add(1, Ordering::SeqCst);
                        query.reply_err(ZBytes::new()).wait().unwrap();
                    }
                })
                .wait()
                .unwrap();
            let (requests, generation, context) = {
                let mut states = zlock!(statesref);
                for num in [0, 2, 4] {
                    handle_sample(&mut states, fragment(&key, source_id, num));
                }
                let source = states.sequenced_states.peek_mut(&source_id).unwrap();
                let requests = source.prepare_fragment_scan(Duration::ZERO);
                assert_eq!(requests.len(), 1);
                assert_eq!(requests[0].ranges.len(), 2);
                (
                    requests,
                    source.generation.clone(),
                    QueryContext::new(&states),
                )
            };
            if closed_session {
                ztimeout!(session.close()).unwrap();
            }
            context.issue_fragments(&statesref, source_id, &generation, requests);
            ztimeout!(async {
                loop {
                    if zlock!(statesref)
                        .sequenced_states
                        .peek(&source_id)
                        .unwrap()
                        .pending_queries
                        == 0
                    {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            });
            {
                let states = zlock!(statesref);
                let source = states.sequenced_states.peek(&source_id).unwrap();
                assert!(source.pending_samples[&WrappingSn(0)].is_retry());
                assert_eq!(
                    queries.load(Ordering::SeqCst),
                    if closed_session { 0 } else { 2 }
                );
            }
            if !closed_session {
                ztimeout!(session.close()).unwrap();
            }
        }
    }

    /// Publications can fill the missing fragments and deliver the sample while
    /// queries are still running. When those queries finish, they must decrease
    /// pending_queries even though the sample is no longer in the buffer.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn fragment_reservation_outlives_delivered_assembly() {
        let mut config = Config::default();
        config.scouting.multicast.set_enabled(Some(false)).unwrap();
        config.listen.endpoints.set(vec![]).unwrap();
        let session = ztimeout!(zenoh::open(config)).unwrap();
        let source_id = EntityGlobalId::new(session.zid(), 7);
        let key = KeyExpr::try_from("test/ext/dispatch/live").unwrap();
        let received = Arc::new(Mutex::new(Vec::new()));
        let statesref = frag_recovery_state(&session, &key, received.clone());
        let cache = session
            .declare_queryable("test/ext/dispatch/live/@adv/**")
            .wait()
            .unwrap();
        let (requests, generation, context) = {
            let mut states = zlock!(statesref);
            for num in [0, 2, 4] {
                handle_sample(&mut states, fragment(&key, source_id, num));
            }
            let source = states.sequenced_states.peek_mut(&source_id).unwrap();
            let requests = source.prepare_fragment_scan(Duration::ZERO);
            (
                requests,
                source.generation.clone(),
                QueryContext::new(&states),
            )
        };
        context.issue_fragments(&statesref, source_id, &generation, requests);
        let first = ztimeout!(cache.recv_async()).unwrap();
        let second = ztimeout!(cache.recv_async()).unwrap();
        {
            let mut states = zlock!(statesref);
            for num in [1, 3] {
                handle_sample(&mut states, fragment(&key, source_id, num));
            }
            let source = states.sequenced_states.peek(&source_id).unwrap();
            assert!(source.pending_samples.is_empty());
            assert_eq!(source.pending_queries, 1);
            assert_eq!(received.lock().unwrap().len(), 1);
        }
        drop(first);
        drop(second);
        ztimeout!(async {
            loop {
                if zlock!(statesref)
                    .sequenced_states
                    .peek(&source_id)
                    .unwrap()
                    .pending_queries
                    == 0
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        });
        assert_eq!(received.lock().unwrap().len(), 1);
        ztimeout!(session.close()).unwrap();
    }
}
