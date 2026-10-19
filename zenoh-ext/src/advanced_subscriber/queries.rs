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

//! Query transport and reply lifetimes for the advanced subscriber.
//!
//! Reserve queries under the state lock, then dispatch them after unlocking:
//! local queryables may immediately invoke or drop a reply callback. Each
//! handler owns one reservation until its last callback is dropped. Fragment
//! attempts share that reservation across all requested ranges, even if the
//! sample is delivered before the queries finish.

use std::{
    future::Future,
    sync::{Arc, Mutex},
    time::Duration,
};

use uhlc::ID;
use zenoh::{
    internal::zlock,
    key_expr::KeyExpr,
    query::{
        ConsolidationMode, Parameters, QueryTarget, Reply, ReplyKeyExpr, Selector, TimeBound,
        TimeExpr, TimeRange, ZenohParameters,
    },
    sample::Sample,
    session::{EntityGlobalId, WeakSession},
    Wait, KE_ADV_PREFIX, KE_STAR, KE_STARSTAR,
};

use super::{handle_sample, HistoryConfig, State};
use crate::{advanced_query::format_range, fragmentation::FragRange, utils::WrappingSn};

/// Reserve work while holding the state lock, before dispatch can race with
/// another recovery decision. Count fragment attempts, not their range queries.
pub(super) fn reserve_queries(pending_queries: &mut u64, count: usize) {
    *pending_queries += count as u64;
}

/// Release one handler's reservation under the state lock. Return whether all
/// reservations have finished, preserving the existing saturating accounting.
fn finish_query(pending_queries: &mut u64) -> bool {
    *pending_queries = pending_queries.saturating_sub(1);
    *pending_queries == 0
}

/// Query settings copied while holding the state lock, so queries can be sent
/// after releasing it.
pub(super) struct QueryContext {
    session: WeakSession,
    key_expr: KeyExpr<'static>,
    target: QueryTarget,
    timeout: Duration,
}

impl QueryContext {
    pub(super) fn new(state: &State) -> Self {
        Self {
            session: state.session.clone(),
            key_expr: state.key_expr.clone(),
            target: state.query_target,
            timeout: state.query_timeout,
        }
    }

    pub(super) fn source_key(&self, source: EntityGlobalId) -> KeyExpr<'static> {
        &self.key_expr
            / KE_ADV_PREFIX
            / KE_STAR
            / &source.zid().into_keyexpr()
            / &KeyExpr::try_from(source.eid().to_string()).unwrap()
            / KE_STARSTAR
    }

    /// Send the fragment queries selected by a timer tick, building the source
    /// key once for all requests. Call this after releasing the state lock.
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
}

/// Keeps a history query counted as pending until its reply callback is dropped.
/// Do not clone query handlers: each drop releases one reservation made under
/// the state lock before constructing the handler.
pub(super) struct InitialRepliesHandler {
    pub(super) statesref: Arc<Mutex<State>>,
}

/// Owns one source-specific history or whole-sample recovery query reservation.
pub(super) struct SequencedRepliesHandler {
    pub(super) source_id: EntityGlobalId,
    pub(super) statesref: Arc<Mutex<State>>,
}

/// Owns one timestamped source's history query reservation.
pub(super) struct TimestampedRepliesHandler {
    pub(super) id: ID,
    pub(super) statesref: Arc<Mutex<State>>,
}

impl InitialRepliesHandler {
    fn complete(&self) {
        let states = &mut *zlock!(self.statesref);
        if finish_query(&mut states.global_pending_queries) {
            states.on_initial_queries_finished(&self.statesref);
        }
    }

    pub(super) fn issue(self, context: &QueryContext, selector: Selector<'_>) {
        context.issue(selector, move |sample| self.handle_reply(sample));
    }

    fn handle_reply(&self, sample: Sample) {
        State::receive_query_sample(&self.statesref, sample);
    }
}

impl Drop for InitialRepliesHandler {
    fn drop(&mut self) {
        self.complete();
    }
}

impl TimestampedRepliesHandler {
    fn complete(&self) {
        let states = &mut *zlock!(self.statesref);
        if let Some(source) = states.timestamped_states.peek_mut(&self.id) {
            finish_query(&mut source.pending_queries);
            states.on_timestamped_query_finished(&self.id);
        }
    }

    pub(super) fn issue(self, context: &QueryContext, selector: Selector<'_>) {
        context.issue(selector, move |sample| self.handle_reply(sample));
    }

    fn handle_reply(&self, sample: Sample) {
        State::receive_query_sample(&self.statesref, sample);
    }
}

impl Drop for TimestampedRepliesHandler {
    fn drop(&mut self) {
        self.complete();
    }
}

impl SequencedRepliesHandler {
    fn complete(&self) {
        let states = &mut *zlock!(self.statesref);
        // Finishing a query without receiving samples must not keep an idle
        // source in the cache longer.
        if let Some(source) = states.sequenced_states.peek_mut(&self.source_id) {
            finish_query(&mut source.pending_queries);
            states.on_history_or_sample_recovery_finished(&self.statesref, &self.source_id);
        }
    }

    pub(super) fn issue(self, context: &QueryContext, selector: Selector<'_>) {
        context.issue(selector, move |sample| self.handle_reply(sample));
    }

    fn handle_reply(&self, sample: Sample) {
        State::receive_query_sample(&self.statesref, sample);
    }
}

impl Drop for SequencedRepliesHandler {
    fn drop(&mut self) {
        self.complete();
    }
}

/// Missing fragments selected and counted under the state lock for one sample.
/// After unlocking, move this into a `FragRecoveryAttempt` before scheduling a
/// task or awaiting, so cancellation releases the reservation.
pub(super) struct FragmentRequest {
    pub(super) sn: WrappingSn,
    pub(super) ranges: Vec<FragRange>,
    /// Identity of this sample's recovery attempt, distinct from its source generation.
    pub(super) attempt: Arc<()>,
}

impl FragmentRequest {
    fn into_attempt(
        self,
        statesref: &Arc<Mutex<State>>,
        source_id: EntityGlobalId,
        generation: &Arc<()>,
    ) -> FragRecoveryAttempt {
        FragRecoveryAttempt {
            statesref: statesref.clone(),
            source_id,
            generation: generation.clone(),
            sn: self.sn,
            ranges: self.ranges,
            attempt: self.attempt,
        }
    }
}

/// Owns one reservation for all range queries recovering a sample's fragments.
/// All callbacks share an `Arc` to this handler, so only the last callback's
/// drop completes the attempt. The reservation outlives delivery or eviction
/// of the sample; only removal of the source generation makes it obsolete.
pub(super) struct FragRecoveryAttempt {
    pub(super) source_id: EntityGlobalId,
    pub(super) statesref: Arc<Mutex<State>>,
    /// Identity of the source state that reserved this attempt. An old handler
    /// must not change a source removed and recreated under the same source ID.
    pub(super) generation: Arc<()>,
    pub(super) sn: WrappingSn,
    pub(super) ranges: Vec<FragRange>,
    /// Identity of this recovery attempt within the source generation and SN.
    /// Late replies or completion must not modify a later attempt for that sample.
    pub(super) attempt: Arc<()>,
}

impl FragRecoveryAttempt {
    fn complete(&self) {
        let should_recheck = {
            let states = &mut *zlock!(self.statesref);
            let Some(source) = states.sequenced_states.peek_mut(&self.source_id) else {
                return;
            };
            if !Arc::ptr_eq(&source.generation, &self.generation) {
                return;
            }
            // The reservation belongs to the attempt, not the buffered sample:
            // live publications may already have completed and delivered it.
            finish_query(&mut source.pending_queries);
            if let Some(sample) = source.pending_samples.get_mut(&self.sn) {
                // finish_recovery checks the sample's current attempt identity.
                sample.finish_recovery(&self.attempt, &self.ranges);
            }
            states.on_fragment_recovery_finished(self.source_id)
        };
        if should_recheck {
            SequencedRepliesHandler::schedule_gap_recheck(&self.statesref, self.source_id);
        }
    }

    fn issue(self, context: &QueryContext, query_expr: &KeyExpr<'static>) {
        let seq_num_range = format_range("_sn", Some(self.sn), Some(self.sn));
        let attempt = Arc::new(self);
        // Keep this Arc until all range queries have been sent. Otherwise, if
        // the first query finishes immediately, it could end the whole attempt
        // before we send the remaining queries.
        for range in &attempt.ranges {
            let params = seq_num_range.clone()
                + ";"
                + &format_range(
                    "_fn",
                    range.start.map(Into::into),
                    range.end.map(Into::into),
                );
            let attempt = attempt.clone();
            context.issue(
                Selector::from((query_expr.clone(), params)),
                move |sample| attempt.handle_reply(sample),
            );
        }
    }

    fn handle_reply(&self, sample: Sample) {
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
                        .is_some_and(|sample| sample.recovery_matches(&self.attempt))
            });
        if current {
            handle_sample(&mut states, sample);
        }
    }
}

impl Drop for FragRecoveryAttempt {
    fn drop(&mut self) {
        self.complete();
    }
}

#[cfg(test)]
#[path = "tests/queries.rs"]
mod tests;
