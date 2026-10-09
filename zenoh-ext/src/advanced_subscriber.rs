//
// Copyright (c) 2022 ZettaScale Technology
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
use std::{cmp::min, fmt, future::IntoFuture, hash::Hash, str::FromStr, sync::Weak, time::Instant};

use lru::LruCache;
use tokio_util::task::AbortOnDropHandle;
use zenoh::{
    config::ZenohId,
    handlers::{Callback, CallbackDrop, CallbackParameter, IntoHandler},
    internal::bail,
    key_expr::KeyExpr,
    liveliness::{LivelinessSubscriberBuilder, LivelinessToken},
    pubsub::{SubscriberBuilder, SubscriberUndeclaration},
    query::Selector,
    sample::{Locality, Sample, SampleKind},
    session::{EntityGlobalId, EntityId},
    Resolvable, Session, Wait, KE_ADV_PREFIX, KE_EMPTY, KE_PUB, KE_STARSTAR, KE_SUB,
};
#[zenoh_macros::unstable]
use {
    std::collections::HashMap,
    std::convert::TryFrom,
    std::future::Ready,
    std::sync::{Arc, Mutex},
    std::time::Duration,
    uhlc::ID,
    zenoh::handlers::{locked, DefaultHandler},
    zenoh::internal::{runtime::ZRuntime, zlock},
    zenoh::pubsub::Subscriber,
    zenoh::query::QueryTarget,
    zenoh::session::WeakSession,
    zenoh::Result as ZResult,
};

use crate::{
    advanced_cache::{ke_liveliness, KE_UHLC},
    fragmentation::{FragmentedSample, MAX_FRAGMENTS_DEFAULT},
    utils::WrappingSn,
    z_deserialize,
};

mod recovery;
mod source;
#[cfg(test)]
use recovery::FragmentAttempt;
use recovery::{
    InitialRepliesHandler, QueryContext, SequencedRepliesHandler, TimestampedRepliesHandler,
};
use source::{SequencedSource, TimestampedSource};

/// Default bound on the number of samples buffered per source for reordering
/// and fragment reassembly. Overridable via
/// [`AdvancedSubscriberBuilder::max_pending_samples`].
pub(crate) const DEFAULT_MAX_PENDING_SAMPLES: usize = 100;

#[derive(Debug, Default, Clone)]
/// Configure query for historical data for [`history`](crate::AdvancedSubscriberBuilder::history) method.
#[zenoh_macros::unstable]
pub struct HistoryConfig {
    liveliness: bool,
    max_samples: Option<usize>,
    max_age: Option<f64>,
}

#[zenoh_macros::unstable]
impl HistoryConfig {
    /// Enable detection of late joiner publishers and query for their historical data.
    ///
    /// Late joiner detection can only be achieved for [`AdvancedPublishers`](crate::AdvancedPublisher) that enable publisher_detection.
    /// History can only be retransmitted by [`AdvancedPublishers`](crate::AdvancedPublisher) that enable [`cache`](crate::AdvancedPublisherBuilder::cache).
    #[inline]
    #[zenoh_macros::unstable]
    pub fn detect_late_publishers(mut self) -> Self {
        self.liveliness = true;
        self
    }

    /// Specify how many samples to query for each resource.
    ///
    /// This also bounds the number of samples buffered per source for
    /// reordering and fragment reassembly, unless
    /// [`max_pending_samples`](AdvancedSubscriberBuilder::max_pending_samples)
    /// is set.
    ///
    /// Builder will fail if `max_samples` is set to zero.
    #[zenoh_macros::unstable]
    pub fn max_samples(mut self, depth: usize) -> Self {
        self.max_samples = Some(depth);
        self
    }

    /// Specify the maximum age of samples to query.
    ///
    /// Builder will fail if `max_age` is set to zero.
    #[zenoh_macros::unstable]
    pub fn max_age(mut self, seconds: f64) -> Self {
        self.max_age = Some(seconds);
        self
    }
}

#[derive(Debug, Clone, Copy)]
/// Configure retransmission.
///
/// Missing fragments are queried only when recovery is enabled. If recovery
/// fails or times out, the subscriber waits before retrying. It gives up on an
/// incomplete sample when needed to deliver a newer complete sample.
#[zenoh_macros::unstable]
pub struct RecoveryConfig<const CONFIGURED: bool = true> {
    frag_recovery_delay: Duration,
    periodic_queries: Option<Duration>,
    heartbeat: bool,
    retention_period: Option<Duration>,
}

impl<const CONFIGURED: bool> Default for RecoveryConfig<CONFIGURED> {
    fn default() -> Self {
        Self {
            frag_recovery_delay: Duration::from_secs(1),
            periodic_queries: None,
            heartbeat: false,
            retention_period: None,
        }
    }
}

#[zenoh_macros::unstable]
impl<const CONFIGURED: bool> RecoveryConfig<CONFIGURED> {
    /// Set how often to check for missing fragments and how long to wait before retrying.
    ///
    /// Missing fragments before the highest received fragment are queried right
    /// away. For example, receiving fragments 0, 1 and 3 triggers a query for
    /// fragment 2. Only one recovery attempt runs at a time for each sample.
    /// If it fails, the subscriber waits for this delay before retrying, or gives
    /// up on the sample if needed to deliver a newer complete sample.
    ///
    /// Fragments after the highest received fragment may still be on their way.
    /// The subscriber queries for them only after no new fragment has arrived
    /// for this delay. Duplicate fragments do not restart the wait.
    ///
    /// Builder will fail if `delay` is zero.
    #[zenoh_macros::unstable]
    #[inline]
    pub fn fragments_recovery_delay(self, delay: Duration) -> RecoveryConfig<CONFIGURED> {
        RecoveryConfig {
            frag_recovery_delay: delay,
            ..self
        }
    }
}

#[zenoh_macros::unstable]
impl RecoveryConfig<false> {
    /// Enable periodic queries for not yet received Samples and specify their period.
    ///
    /// This allows retrieving the last Sample(s) if the last Sample(s) is/are lost.
    /// So it is useful for sporadic publications but useless for periodic publications
    /// with a period smaller or equal to this period.
    /// Retransmission can only be achieved by [`AdvancedPublishers`](crate::AdvancedPublisher)
    /// that enable [`cache`](crate::AdvancedPublisherBuilder::cache) and
    /// [`sample_miss_detection`](crate::AdvancedPublisherBuilder::sample_miss_detection).
    #[zenoh_macros::unstable]
    #[inline]
    pub fn periodic_queries(self, period: Duration) -> RecoveryConfig<true> {
        RecoveryConfig {
            frag_recovery_delay: self.frag_recovery_delay,
            periodic_queries: Some(period),
            heartbeat: false,
            retention_period: self.retention_period,
        }
    }

    /// Subscribe to heartbeats of [`AdvancedPublishers`](crate::AdvancedPublisher).
    ///
    /// This allows receiving the last published Sample's sequence number and check for misses.
    /// Heartbeat subscriber must be paired with [`AdvancedPublishers`](crate::AdvancedPublisher)
    /// that enable [`cache`](crate::AdvancedPublisherBuilder::cache) and
    /// [`sample_miss_detection`](crate::AdvancedPublisherBuilder::sample_miss_detection) with
    /// [`heartbeat`](crate::advanced_publisher::MissDetectionConfig::heartbeat) or
    /// [`sporadic_heartbeat`](crate::advanced_publisher::MissDetectionConfig::sporadic_heartbeat).
    #[zenoh_macros::unstable]
    #[inline]
    pub fn heartbeat(self) -> RecoveryConfig<true> {
        RecoveryConfig {
            frag_recovery_delay: self.frag_recovery_delay,
            periodic_queries: None,
            heartbeat: true,
            retention_period: self.retention_period,
        }
    }
}

#[zenoh_macros::unstable]
impl<const CONFIGURED: bool> RecoveryConfig<CONFIGURED> {
    const RETENTION_PERIOD_DEFAULT: Duration = Duration::from_secs(3600);

    /// Set the retention period of publishers last Sample state (default to 1h).
    #[zenoh_macros::unstable]
    #[inline]
    pub fn retention_period(mut self, period: Duration) -> RecoveryConfig<CONFIGURED> {
        self.retention_period = Some(period);
        self
    }
}

/// The builder of an [`AdvancedSubscriber`], allowing to configure it.
#[zenoh_macros::unstable]
pub struct AdvancedSubscriberBuilder<'a, 'b, 'c, Handler, const BACKGROUND: bool = false> {
    pub(crate) session: &'a Session,
    pub(crate) key_expr: ZResult<KeyExpr<'b>>,
    pub(crate) origin: Locality,
    pub(crate) retransmission: Option<RecoveryConfig>,
    pub(crate) query_target: QueryTarget,
    pub(crate) query_timeout: Duration,
    pub(crate) max_fragments: u32,
    pub(crate) max_pending_samples: Option<usize>,
    pub(crate) history: Option<HistoryConfig>,
    pub(crate) liveliness: bool,
    pub(crate) meta_key_expr: Option<ZResult<KeyExpr<'c>>>,
    pub(crate) handler: Handler,
}

#[zenoh_macros::unstable]
impl<Handler, const BACKGROUND: bool> fmt::Debug
    for AdvancedSubscriberBuilder<'_, '_, '_, Handler, BACKGROUND>
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AdvancedSubscriberBuilder")
            .field("session", &"..")
            .field("key_expr", &self.key_expr)
            .field("origin", &self.origin)
            .field("retransmission", &self.retransmission)
            .field("query_target", &self.query_target)
            .field("query_timeout", &self.query_timeout)
            .field("max_fragments", &self.max_fragments)
            .field("max_pending_samples", &self.max_pending_samples)
            .field("history", &self.history)
            .field("liveliness", &self.liveliness)
            .field("meta_key_expr", &self.meta_key_expr)
            .field("handler", &"..")
            .field("background", &BACKGROUND)
            .finish()
    }
}

#[zenoh_macros::unstable]
impl<'a, 'b, Handler> AdvancedSubscriberBuilder<'a, 'b, '_, Handler> {
    #[zenoh_macros::unstable]
    pub(crate) fn new(builder: SubscriberBuilder<'a, 'b, Handler>) -> Self {
        AdvancedSubscriberBuilder {
            session: builder.session,
            key_expr: builder.key_expr,
            origin: builder.origin,
            handler: builder.handler,
            retransmission: None,
            query_target: QueryTarget::All,
            query_timeout: Duration::from_secs(10),
            max_fragments: MAX_FRAGMENTS_DEFAULT,
            max_pending_samples: None,
            history: None,
            liveliness: false,
            meta_key_expr: None,
        }
    }
}

#[zenoh_macros::unstable]
impl<'a, 'b, 'c> AdvancedSubscriberBuilder<'a, 'b, 'c, DefaultHandler> {
    /// Add callback to AdvancedSubscriber.
    #[inline]
    #[zenoh_macros::unstable]
    pub fn callback<F>(self, callback: F) -> AdvancedSubscriberBuilder<'a, 'b, 'c, Callback<Sample>>
    where
        F: Fn(Sample) + Send + Sync + 'static,
    {
        self.with(Callback::from(callback))
    }

    /// Add callback to `AdvancedSubscriber`.
    ///
    /// Using this guarantees that your callback will never be called concurrently.
    /// If your callback is also accepted by the [`callback`](AdvancedSubscriberBuilder::callback) method, we suggest you use it instead of `callback_mut`
    #[inline]
    #[zenoh_macros::unstable]
    pub fn callback_mut<F>(
        self,
        callback: F,
    ) -> AdvancedSubscriberBuilder<'a, 'b, 'c, Callback<Sample>>
    where
        F: FnMut(Sample) + Send + Sync + 'static,
    {
        self.callback(locked(callback))
    }

    /// Make the built AdvancedSubscriber an [`AdvancedSubscriber`](AdvancedSubscriber).
    #[inline]
    #[zenoh_macros::unstable]
    pub fn with<Handler>(self, handler: Handler) -> AdvancedSubscriberBuilder<'a, 'b, 'c, Handler>
    where
        Handler: IntoHandler<Sample>,
    {
        AdvancedSubscriberBuilder {
            session: self.session,
            key_expr: self.key_expr,
            origin: self.origin,
            retransmission: self.retransmission,
            query_target: self.query_target,
            query_timeout: self.query_timeout,
            max_fragments: self.max_fragments,
            max_pending_samples: self.max_pending_samples,
            history: self.history,
            liveliness: self.liveliness,
            meta_key_expr: self.meta_key_expr,
            handler,
        }
    }
}

#[zenoh_macros::unstable]
impl<'a, 'b, 'c> AdvancedSubscriberBuilder<'a, 'b, 'c, Callback<Sample>> {
    /// Make the subscriber run in background until the session is closed.
    ///
    /// Background builder doesn't return a `AdvancedSubscriber` object anymore.
    pub fn background(self) -> AdvancedSubscriberBuilder<'a, 'b, 'c, Callback<Sample>, true> {
        AdvancedSubscriberBuilder {
            session: self.session,
            key_expr: self.key_expr,
            origin: self.origin,
            retransmission: self.retransmission,
            query_target: self.query_target,
            query_timeout: self.query_timeout,
            max_fragments: self.max_fragments,
            max_pending_samples: self.max_pending_samples,
            history: self.history,
            liveliness: self.liveliness,
            meta_key_expr: self.meta_key_expr,
            handler: self.handler,
        }
    }
}

#[zenoh_macros::unstable]
impl<'a, 'c, Handler, const BACKGROUND: bool>
    AdvancedSubscriberBuilder<'a, '_, 'c, Handler, BACKGROUND>
{
    /// Restrict the matching publications that will be received by this [`Subscriber`] to the ones that have the given [`Locality`](crate::prelude::Locality).
    #[zenoh_macros::unstable]
    #[inline]
    pub fn allowed_origin(mut self, origin: Locality) -> Self {
        self.origin = origin;
        self
    }

    /// Ask for retransmission of detected lost Samples.
    ///
    /// Retransmission can only be achieved by [`AdvancedPublishers`](crate::AdvancedPublisher)
    /// that enable [`cache`](crate::AdvancedPublisherBuilder::cache) and
    /// [`sample_miss_detection`](crate::AdvancedPublisherBuilder::sample_miss_detection).
    ///
    /// Samples buffered while awaiting retransmission are bounded: see
    /// [`max_pending_samples`](AdvancedSubscriberBuilder::max_pending_samples).
    ///
    /// Finishing a history or sample-recovery query does not discard samples
    /// with missing fragments. The subscriber keeps trying to recover them. It gives up if
    /// recovery fails and a newer complete sample needs to be delivered, or if
    /// the buffer limit requires discarding them. If no newer complete sample
    /// is waiting, failed recovery can be retried.
    #[zenoh_macros::unstable]
    #[inline]
    pub fn recovery(mut self, conf: RecoveryConfig) -> Self {
        self.retransmission = Some(conf);
        self
    }

    // /// Change the target to be used for queries.

    // #[inline]
    // pub fn query_target(mut self, query_target: QueryTarget) -> Self {
    //     self.query_target = query_target;
    //     self
    // }

    /// Change the timeout to be used for queries (history, retransmission).
    #[zenoh_macros::unstable]
    #[inline]
    pub fn query_timeout(mut self, query_timeout: Duration) -> Self {
        self.query_timeout = query_timeout;
        self
    }

    /// Set the maximum number of fragments a single sample may carry.
    ///
    /// Fragments advertised with a [`FragInfo::frag_count`](zenoh::sample::FragInfo::frag_count)
    /// larger than this value are rejected.
    ///
    /// Resolving a builder with `max` set to zero will fail.
    #[zenoh_macros::unstable]
    #[inline]
    pub fn max_fragments(mut self, max: u32) -> Self {
        self.max_fragments = max;
        self
    }

    /// Set the maximum number of samples buffered per source.
    ///
    /// Samples are buffered for reordering and fragment reassembly. When the
    /// buffer exceeds this bound, the oldest entry is delivered if complete,
    /// otherwise discarded and reported as missed through
    /// [`sample_miss_listener`](AdvancedSubscriber::sample_miss_listener).
    ///
    /// This setting takes precedence over
    /// [`HistoryConfig::max_samples`](crate::HistoryConfig::max_samples). It
    /// defaults to 100. Resolving a builder with `max` set to zero will fail.
    #[zenoh_macros::unstable]
    #[inline]
    pub fn max_pending_samples(mut self, max: usize) -> Self {
        self.max_pending_samples = Some(max);
        self
    }

    /// Enable query for historical data.
    ///
    /// History can only be retransmitted by [`AdvancedPublishers`](crate::AdvancedPublisher) that enable [`cache`](crate::AdvancedPublisherBuilder::cache).
    #[zenoh_macros::unstable]
    #[inline]
    pub fn history(mut self, config: HistoryConfig) -> Self {
        self.history = Some(config);
        self
    }

    /// Allow this subscriber to be detected through liveliness.
    #[zenoh_macros::unstable]
    pub fn subscriber_detection(mut self) -> Self {
        self.liveliness = true;
        self
    }

    /// A key expression added to the liveliness token key expression.
    ///
    /// It can be used to convey metadata.
    #[zenoh_macros::unstable]
    pub fn subscriber_detection_metadata<TryIntoKeyExpr>(mut self, meta: TryIntoKeyExpr) -> Self
    where
        TryIntoKeyExpr: TryInto<KeyExpr<'c>>,
        <TryIntoKeyExpr as TryInto<KeyExpr<'c>>>::Error: Into<zenoh::Error>,
    {
        self.meta_key_expr = Some(meta.try_into().map_err(Into::into));
        self
    }

    #[zenoh_macros::unstable]
    fn with_static_keys(self) -> AdvancedSubscriberBuilder<'a, 'static, 'static, Handler> {
        AdvancedSubscriberBuilder {
            session: self.session,
            key_expr: self.key_expr.map(|s| s.into_owned()),
            origin: self.origin,
            retransmission: self.retransmission,
            query_target: self.query_target,
            query_timeout: self.query_timeout,
            max_fragments: self.max_fragments,
            max_pending_samples: self.max_pending_samples,
            history: self.history,
            liveliness: self.liveliness,
            meta_key_expr: self.meta_key_expr.map(|s| s.map(|s| s.into_owned())),
            handler: self.handler,
        }
    }
}

#[zenoh_macros::unstable]
impl<Handler> Resolvable for AdvancedSubscriberBuilder<'_, '_, '_, Handler>
where
    Handler: IntoHandler<Sample>,
    Handler::Handler: Send,
{
    type To = ZResult<AdvancedSubscriber<Handler::Handler>>;
}

#[zenoh_macros::unstable]
impl<Handler> Wait for AdvancedSubscriberBuilder<'_, '_, '_, Handler>
where
    Handler: IntoHandler<Sample> + Send,
    Handler::Handler: Send,
{
    #[zenoh_macros::unstable]
    fn wait(self) -> <Self as Resolvable>::To {
        AdvancedSubscriber::new(self.with_static_keys())
    }
}

#[zenoh_macros::unstable]
impl<Handler> IntoFuture for AdvancedSubscriberBuilder<'_, '_, '_, Handler>
where
    Handler: IntoHandler<Sample> + Send,
    Handler::Handler: Send,
{
    type Output = <Self as Resolvable>::To;
    type IntoFuture = Ready<<Self as Resolvable>::To>;

    #[zenoh_macros::unstable]
    fn into_future(self) -> Self::IntoFuture {
        std::future::ready(self.wait())
    }
}

#[zenoh_macros::unstable]
impl Resolvable for AdvancedSubscriberBuilder<'_, '_, '_, Callback<Sample>, true> {
    type To = ZResult<()>;
}

#[zenoh_macros::unstable]
impl Wait for AdvancedSubscriberBuilder<'_, '_, '_, Callback<Sample>, true> {
    #[zenoh_macros::unstable]
    fn wait(self) -> <Self as Resolvable>::To {
        let mut sub = AdvancedSubscriber::new(self.with_static_keys())?;
        sub.set_background_impl(true);
        Ok(())
    }
}

#[zenoh_macros::unstable]
impl IntoFuture for AdvancedSubscriberBuilder<'_, '_, '_, Callback<Sample>, true> {
    type Output = <Self as Resolvable>::To;
    type IntoFuture = Ready<<Self as Resolvable>::To>;

    #[zenoh_macros::unstable]
    fn into_future(self) -> Self::IntoFuture {
        std::future::ready(self.wait())
    }
}

#[zenoh_macros::unstable]
struct State {
    next_id: usize,
    global_pending_queries: u64,
    sequenced_states: LruCache<EntityGlobalId, SequencedSource>,
    timestamped_states: LruCache<ID, TimestampedSource>,
    session: WeakSession,
    key_expr: KeyExpr<'static>,
    retransmission: bool,
    frag_recovery_delay: Duration,
    period: Option<Duration>,
    max_pending_samples: usize,
    query_target: QueryTarget,
    query_timeout: Duration,
    max_fragments: u32,
    // Callback must be dropped when the underlying subscriber is undeclared
    // (for example when session is closed), in order to "close" the advanced
    // subscriber receiver, hence the `Option`.
    callback: Option<Callback<Sample>>,
    miss_handlers: HashMap<usize, Callback<Miss>>,
    token: Option<LivelinessToken>,
    _gc_task: AbortOnDropHandle<()>,
}

#[zenoh_macros::unstable]
impl State {
    #[zenoh_macros::unstable]
    fn register_miss_callback(&mut self, callback: Callback<Miss>) -> usize {
        let id = self.next_id;
        self.next_id += 1;
        self.miss_handlers.insert(id, callback);
        id
    }
    #[zenoh_macros::unstable]
    fn unregister_miss_callback(&mut self, id: &usize) {
        self.miss_handlers.remove(id);
    }
}

/*
use zenoh_ext::{AdvancedSubscriberBuilderExt, HistoryConfig, RecoveryConfig};

let session = zenoh::open(zenoh::Config::default()).await.unwrap();
let subscriber = session
    .declare_subscriber("key/expression")
    .history(HistoryConfig::default().detect_late_publishers())
    .recovery(RecoveryConfig::default())
    .await
    .unwrap();

let miss_listener = subscriber.sample_miss_listener().await.unwrap();
loop {
    tokio::select! {
        sample = subscriber.recv_async() => {
            if let Ok(sample) = sample {
                // ...
            }
        },
        miss = miss_listener.recv_async() => {
            if let Ok(miss) = miss {
                // ...
            }
        },
    }
}
*/

/// The extension to [`Subscriber`](zenoh::pubsub::Subscriber) that provides advanced functionalities
///
/// The `AdvancedSubscriber` is constructed over a regular [`Subscriber`](zenoh::pubsub::Subscriber)
/// through [`advanced`](crate::AdvancedSubscriberBuilderExt::advanced) method or by using
/// any other method of [`AdvancedSubscriberBuilder`](crate::AdvancedSubscriberBuilder).
///
/// The `AdvancedSubscriber` works with [`AdvancedPublisher`](crate::AdvancedPublisher) to provide additional functionalities such as:
/// * missing samples detection using periodic queries or heartbeat subscription configurable with [`recovery`](crate::AdvancedSubscriberBuilder::recovery) method
/// * recovering missing samples, configured with [`history`](crate::AdvancedSubscriberBuilder::history) method
///   (max age and sample count, late joiner detection and requesting)
/// * liveliness-based subscriber detection with [`subscriber_detection`](crate::AdvancedSubscriberBuilder::subscriber_detection) method
///
/// # Examples
/// ```no_run
/// # #[tokio::main]
/// # async fn main() {
/// use zenoh_ext::{AdvancedSubscriberBuilderExt, HistoryConfig, RecoveryConfig};
/// let session = zenoh::open(zenoh::Config::default()).await.unwrap();
/// let subscriber = session
///     .declare_subscriber("key/expression")
///     .history(HistoryConfig::default().detect_late_publishers())
///     .recovery(RecoveryConfig::default().heartbeat())
///     .subscriber_detection()
///     .await
///     .unwrap();
/// let miss_listener = subscriber.sample_miss_listener().await.unwrap();
/// loop {
///     tokio::select! {
///         sample = subscriber.recv_async() => {
///             if let Ok(sample) = sample {
///                 // ...
///             }
///         },
///         miss = miss_listener.recv_async() => {
///             if let Ok(miss) = miss {
///                 // ...
///             }
///         },
///     }
/// }
/// # }
/// ```
#[zenoh_macros::unstable]
pub struct AdvancedSubscriber<Receiver> {
    statesref: Arc<Mutex<State>>,
    subscriber: Subscriber<()>,
    receiver: Receiver,
    liveliness_subscriber: Option<Subscriber<()>>,
    heartbeat_subscriber: Option<Subscriber<()>>,
}

#[zenoh_macros::unstable]
impl<Receiver> fmt::Debug for AdvancedSubscriber<Receiver> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AdvancedSubscriber")
            .field("statesref", &"..")
            .field("subscriber", &self.subscriber)
            .field("receiver", &"..")
            .field("liveliness_subscriber", &self.liveliness_subscriber)
            .field("heartbeat_subscriber", &self.heartbeat_subscriber)
            .finish()
    }
}

#[zenoh_macros::unstable]
impl<Receiver> std::ops::Deref for AdvancedSubscriber<Receiver> {
    type Target = Receiver;
    fn deref(&self) -> &Self::Target {
        &self.receiver
    }
}

#[zenoh_macros::unstable]
impl<Receiver> std::ops::DerefMut for AdvancedSubscriber<Receiver> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.receiver
    }
}

/// Without `SourceInfo`, we cannot tell which sample a fragment belongs to.
/// The source ID and sequence number are needed to group fragments together.
#[inline]
fn is_orphan_fragment(sample: &Sample) -> bool {
    // TODO: update when timestamped sources are supported on fragmentation
    sample.source_info().is_none() && sample.frag_info().is_some()
}

/// Tells the publication callback whether to start recovery timers or queries.
/// Query replies leave that work until the query finishes.
#[derive(Default)]
struct SampleInsertion {
    new_source: bool,
    new_fragment_slot: bool,
}

#[zenoh_macros::unstable]
fn handle_sample(states: &mut State, sample: Sample) -> SampleInsertion {
    let Some(callback) = states.callback.as_ref() else {
        return SampleInsertion::default();
    };
    if is_orphan_fragment(&sample) {
        tracing::debug!(
            "AdvancedSubscriber: dropped fragmented sample without source_info: \
             fragmentation information is meaningless without sequence-number \
             source_info. Fragments must originate from an AdvancedPublisher \
             with sample_miss_detection enabled."
        );
        return SampleInsertion::default();
    }
    if let Some(source_info) = sample.source_info().cloned() {
        let mut new_source = false;
        let source_id = *source_info.source_id();
        let sn: WrappingSn = source_info.source_sn().into();
        let is_fragmented = sample.frag_info().is_some();
        let global_pending_queries = states.global_pending_queries;
        let retransmission = states.retransmission;
        let max_pending_samples = states.max_pending_samples;
        let max_fragments = states.max_fragments;
        let miss_handlers = &states.miss_handlers;
        let state = states.sequenced_states.get_or_insert_mut(source_id, || {
            new_source = true;
            Default::default()
        });

        if state.last_evicted.is_some_and(|last| sn <= last)
            || state
                .pending_samples
                .get(&sn)
                .is_some_and(FragmentedSample::is_abandoned)
        {
            return SampleInsertion {
                new_source,
                new_fragment_slot: false,
            };
        }

        let new_frag = if state.last_delivered.is_none() && global_pending_queries != 0 {
            // Late joiner: buffer until the historical query completes.
            if is_fragmented {
                match state.insert_sample(sample, &source_info, max_fragments) {
                    Ok(new_frag) => {
                        state.enforce_pending_limit(
                            callback,
                            miss_handlers,
                            source_id,
                            max_pending_samples,
                        );
                        new_frag
                    }
                    Err(()) => false,
                }
            } else if max_pending_samples == 1 {
                state.deliver_and_drain(sample, sn, callback, miss_handlers, source_id);
                false
            } else {
                state
                    .pending_samples
                    .insert(sn, FragmentedSample::single(sample));
                state.enforce_pending_limit(
                    callback,
                    miss_handlers,
                    source_id,
                    max_pending_samples,
                );
                false
            }
        } else if state.last_delivered.is_some() && sn != state.last_delivered.unwrap() + 1 {
            if sn > state.last_delivered.unwrap() {
                if retransmission {
                    let new_frag = if is_fragmented {
                        state
                            .insert_sample(sample, &source_info, max_fragments)
                            .unwrap_or_default()
                    } else {
                        state
                            .pending_samples
                            .insert(sn, FragmentedSample::single(sample));
                        false
                    };
                    // A recovered fragment may close the gap; try to flush.
                    if let Some((sn, s)) = state.pop_next_ready() {
                        state.deliver_and_drain(s, sn, callback, miss_handlers, source_id);
                    }
                    state.enforce_pending_limit(
                        callback,
                        miss_handlers,
                        source_id,
                        max_pending_samples,
                    );
                    new_frag
                } else {
                    if is_fragmented {
                        let _ = state.insert_sample(sample, &source_info, max_fragments);
                        state.enforce_pending_limit(
                            callback,
                            miss_handlers,
                            source_id,
                            max_pending_samples,
                        );
                        if let Some(s) = state.take_complete(sn) {
                            state.deliver_and_drain(s, sn, callback, miss_handlers, source_id);
                        }
                    } else {
                        // Misses are reported on delivery.
                        state.deliver_and_drain(sample, sn, callback, miss_handlers, source_id);
                    }
                    false
                }
            } else {
                // Duplicate or old sample.
                false
            }
        } else {
            // In-order sample, or a source with no delivery baseline yet.
            if is_fragmented {
                match state.insert_sample(sample, &source_info, max_fragments) {
                    Ok(new_frag) => {
                        state.enforce_pending_limit(
                            callback,
                            miss_handlers,
                            source_id,
                            max_pending_samples,
                        );
                        let ready = if retransmission {
                            state.pop_next_ready()
                        } else {
                            state.take_complete(sn).map(|s| (sn, s))
                        };
                        if let Some((k, s)) = ready {
                            state.deliver_and_drain(s, k, callback, miss_handlers, source_id);
                        }
                        new_frag
                    }
                    Err(()) => false,
                }
            } else if retransmission && !state.pending_samples.is_empty() {
                // Even a complete sample must wait if an earlier sample has
                // missing fragments we can still recover.
                state
                    .pending_samples
                    .insert(sn, FragmentedSample::single(sample));
                state.enforce_pending_limit(
                    callback,
                    miss_handlers,
                    source_id,
                    max_pending_samples,
                );
                if let Some((sn, sample)) = state.pop_next_ready() {
                    state.deliver_and_drain(sample, sn, callback, miss_handlers, source_id);
                }
                false
            } else {
                state.deliver_and_drain(sample, sn, callback, miss_handlers, source_id);
                false
            }
        };

        if global_pending_queries == 0 {
            state.drain_authorized_samples(callback, miss_handlers, source_id, retransmission);
        }
        state.latest_access = Instant::now();
        SampleInsertion {
            new_source,
            new_fragment_slot: new_frag,
        }
    } else if let Some(timestamp) = sample.timestamp() {
        let state = states
            .timestamped_states
            .get_or_insert_mut(*timestamp.get_id(), Default::default);
        if state.last_delivered.map(|t| t < *timestamp).unwrap_or(true) {
            if (states.global_pending_queries == 0 && state.pending_queries == 0)
                || states.max_pending_samples == 1
            {
                state.last_delivered = Some(*timestamp);
                callback.call(sample);
            } else {
                state.pending_samples.entry(*timestamp).or_insert(sample);
                if state.pending_samples.len() >= states.max_pending_samples {
                    state.flush(Some(callback));
                }
            }
        }
        state.latest_access = Instant::now();
        SampleInsertion::default()
    } else {
        callback.call(sample);
        SampleInsertion::default()
    }
}

#[zenoh_macros::unstable]
fn range(name: &str, start: Option<WrappingSn>, end: Option<WrappingSn>) -> String {
    match (start, end) {
        (Some(start), Some(end)) => format!("{}={}..{}", name, start, end),
        (Some(start), None) => format!("{}={}..", name, start),
        (None, Some(end)) => format!("{}=..{}", name, end),
        (None, None) => format!("{}=..", name),
    }
}

/// Garbage collects the source states' lists.
///
/// Reclamation is based on last access; alive publishers are not reclaimed.
async fn gc_task(statesref: Weak<Mutex<State>>, retention_period: Duration) {
    /// Garbage collect a lists and return the oldest access.
    fn garbage_collect<K: Copy + Eq + Hash, S>(
        states: &mut LruCache<K, S>,
        retention_period: Duration,
        now: Instant,
        retention: impl Fn(&mut S) -> (&mut Instant, bool),
    ) -> Instant {
        while let Some((&key, state)) = states.iter_mut().next_back() {
            let (latest_access, alive) = retention(state);
            if now.duration_since(*latest_access) <= retention_period {
                return *latest_access;
            // if the publisher is still marked as alive, just update its latest access
            // (accessing the state will also move it to the back of the LRU list)
            } else if alive {
                *retention(states.get_mut(&key).unwrap()).0 = now;
            } else {
                states.pop_lru();
            }
        }
        now
    }
    // start by sleeping for the initial retention period
    tokio::time::sleep(retention_period).await;
    loop {
        let oldest_access = {
            let Some(states) = statesref.upgrade() else {
                // either the task was scheduled concurrently to its abortion, so we don't care
                // sleeping, or we are in the theoretically possible but zero probability case
                // of `new_cyclic` not having returned yet, so we still don't care sleeping.
                tokio::time::sleep(retention_period).await;
                continue;
            };
            let mut states = states.lock().unwrap();
            let now = Instant::now();
            min(
                garbage_collect(&mut states.sequenced_states, retention_period, now, |s| {
                    (&mut s.latest_access, s.alive)
                }),
                garbage_collect(&mut states.timestamped_states, retention_period, now, |s| {
                    (&mut s.latest_access, s.alive)
                }),
            )
        };
        tokio::time::sleep_until((oldest_access + retention_period).into()).await;
    }
}

#[zenoh_macros::unstable]
impl<Handler> AdvancedSubscriber<Handler> {
    fn new<H>(conf: AdvancedSubscriberBuilder<'_, '_, '_, H>) -> ZResult<Self>
    where
        H: IntoHandler<Sample, Handler = Handler> + Send,
    {
        // Check config
        if let Some(history) = conf.history.as_ref() {
            if history.max_samples.is_some_and(|d| d == 0) {
                bail!("max_samples must not be zero")
            }
            if history.max_age.is_some_and(|a| a == 0.0) {
                bail!("max_age must not be zero")
            }
        }
        if conf.max_fragments == 0 {
            bail!("max_fragments must not be zero")
        }
        if conf
            .retransmission
            .as_ref()
            .is_some_and(|r| r.frag_recovery_delay.is_zero())
        {
            bail!("frag_recovery_delay must not be zero")
        }
        if conf.max_pending_samples.is_some_and(|m| m == 0) {
            bail!("max_pending_samples must not be zero")
        }
        let (callback, receiver) = conf.handler.into_handler();
        let key_expr = conf.key_expr?;
        let meta = match conf.meta_key_expr {
            Some(meta) => Some(meta?),
            None => None,
        };
        let retransmission = conf.retransmission;
        let max_pending_samples = conf
            .max_pending_samples
            .or(conf.history.as_ref().and_then(|h| h.max_samples))
            .unwrap_or(DEFAULT_MAX_PENDING_SAMPLES);
        let retention_period = retransmission
            .as_ref()
            .and_then(|r| r.retention_period)
            .unwrap_or(RecoveryConfig::<true>::RETENTION_PERIOD_DEFAULT);
        let statesref = Arc::new_cyclic(|weak| {
            Mutex::new(State {
                next_id: 0,
                sequenced_states: LruCache::unbounded(),
                timestamped_states: LruCache::unbounded(),
                global_pending_queries: if conf.history.is_some() { 1 } else { 0 },
                session: conf.session.downgrade(),
                period: retransmission.as_ref().and_then(|r| r.periodic_queries),
                key_expr: key_expr.clone().into_owned(),
                retransmission: retransmission.is_some(),
                frag_recovery_delay: retransmission.unwrap_or_default().frag_recovery_delay,
                max_pending_samples,
                query_target: conf.query_target,
                query_timeout: conf.query_timeout,
                max_fragments: conf.max_fragments,
                callback: Some(callback),
                miss_handlers: HashMap::new(),
                token: None,
                _gc_task: AbortOnDropHandle::new(
                    ZRuntime::Application.spawn(gc_task(weak.clone(), retention_period)),
                ),
            })
        });

        let sub_callback = {
            let statesref = statesref.clone();

            move |s: Sample| {
                let mut lock = zlock!(statesref);
                let states = &mut *lock;
                let source_info = s.source_info().cloned();
                let is_fragmented = s.frag_info().is_some();
                let inserted = handle_sample(states, s);
                let Some(info) = source_info else {
                    return;
                };
                let source_id = *info.source_id();
                let Some(state) = states.sequenced_states.get_mut(&source_id) else {
                    return;
                };
                let request = if is_fragmented {
                    retransmission.and_then(|conf| {
                        state.on_fragment(
                            &statesref,
                            source_id,
                            info.source_sn().into(),
                            inserted.new_fragment_slot,
                            conf.frag_recovery_delay,
                        )
                    })
                } else {
                    None
                };
                if inserted.new_source {
                    state.periodic_task = SequencedRepliesHandler::spawn_periodic(
                        &statesref,
                        states.period,
                        source_id,
                    );
                }
                let work = request.map(|request| (request, state.generation.clone()));
                let work = work
                    .map(|(request, generation)| (request, generation, QueryContext::new(states)));
                drop(lock);
                if let Some((request, generation, context)) = work {
                    ZRuntime::Application.spawn(context.fragment_query_task(
                        &statesref,
                        source_id,
                        &generation,
                        request,
                    ));
                }
                SequencedRepliesHandler::recover_gap(&statesref, source_id);
            }
        };

        // When the underlying subscriber is undeclared (for example when the session is closed)
        // the advanced subscriber callback must be dropped to "close" the receiver.
        let drop_callback = {
            let statesref = statesref.clone();
            move || {
                let mut states = statesref.lock().unwrap();
                states.callback.take();
                states.miss_handlers.clear();
            }
        };

        let subscriber = conf
            .session
            .declare_subscriber(&key_expr)
            .with(CallbackDrop {
                callback: sub_callback,
                drop: drop_callback,
            })
            .allowed_origin(conf.origin)
            .wait()?;

        tracing::debug!("Create AdvancedSubscriber{{key_expr: {}}}", key_expr,);

        if let Some(historyconf) = conf.history.as_ref() {
            let handler = InitialRepliesHandler {
                statesref: statesref.clone(),
            };
            let context = QueryContext::new(&zlock!(statesref));
            handler.issue(
                &context,
                Selector::from((
                    &key_expr / KE_ADV_PREFIX / KE_STARSTAR,
                    historyconf.parameters(),
                )),
            );
        }

        let liveliness_subscriber = if let Some(historyconf) = conf.history.as_ref() {
            if historyconf.liveliness {
                let live_callback = {
                    let context = QueryContext::new(&zlock!(statesref));
                    let statesref = statesref.clone();
                    let historyconf = historyconf.clone();
                    move |s: Sample| {
                        let Ok(parsed) = ke_liveliness::parse(s.key_expr().as_keyexpr()) else {
                            tracing::warn!(
                                "AdvancedSubscriber{{}}: Received malformed liveliness token key expression: {}",
                                s.key_expr()
                            );
                            return;
                        };
                        if let Ok(zid) = ZenohId::from_str(parsed.zid().as_str()) {
                            // TODO : If we already have a state associated to this discovered source
                            // we should query with the appropriate range to avoid unnecessary retransmissions
                            if parsed.eid() == KE_UHLC {
                                let mut lock = zlock!(statesref);
                                let states = &mut *lock;
                                if s.kind() == SampleKind::Delete {
                                    tracing::trace!(
                                        "AdvancedSubscriber{{key_expr: {}}}: Liveliness loss for publishers with zid={}",
                                        states.key_expr,
                                        parsed.zid().as_str()
                                    );
                                    if let Some(state) =
                                        states.timestamped_states.peek_mut(&ID::from(zid))
                                    {
                                        state.alive = false;
                                    }
                                    return;
                                }
                                tracing::trace!(
                                    "AdvancedSubscriber{{key_expr: {}}}: Detect late joiner publishers with zid={}",
                                    states.key_expr,
                                    parsed.zid().as_str()
                                );
                                let state = states
                                    .timestamped_states
                                    .get_or_insert_mut(ID::from(zid), Default::default);
                                state.pending_queries += 1;
                                state.alive = true;
                                state.latest_access = Instant::now();

                                drop(lock);

                                let handler = TimestampedRepliesHandler {
                                    id: ID::from(zid),
                                    statesref: statesref.clone(),
                                };
                                handler.issue(
                                    &context,
                                    Selector::from((s.key_expr(), historyconf.parameters())),
                                );
                            } else if let Ok(eid) = EntityId::from_str(parsed.eid().as_str()) {
                                let source_id = EntityGlobalId::new(zid, eid);
                                let mut lock = zlock!(statesref);
                                let states = &mut *lock;
                                if s.kind() == SampleKind::Delete {
                                    tracing::trace!(
                                        "AdvancedSubscriber{{key_expr: {}}}: Liveliness loss for publishers with zid={}",
                                        states.key_expr,
                                        parsed.zid().as_str()
                                    );
                                    if let Some(state) =
                                        states.sequenced_states.peek_mut(&source_id)
                                    {
                                        state.alive = false;
                                    }
                                    return;
                                }
                                tracing::trace!(
                                    "AdvancedSubscriber{{key_expr: {}}}: Detect late joiner publishers with zid={}",
                                    states.key_expr,
                                    parsed.zid().as_str()
                                );
                                let mut new = false;
                                let state =
                                    states.sequenced_states.get_or_insert_mut(source_id, || {
                                        new = true;
                                        Default::default()
                                    });
                                if new {
                                    state.periodic_task = SequencedRepliesHandler::spawn_periodic(
                                        &statesref,
                                        states.period,
                                        source_id,
                                    );
                                }
                                state.pending_queries += 1;
                                state.alive = true;
                                state.latest_access = Instant::now();

                                drop(lock);

                                let handler = SequencedRepliesHandler {
                                    source_id,
                                    statesref: statesref.clone(),
                                };
                                handler.issue(
                                    &context,
                                    Selector::from((s.key_expr(), historyconf.parameters())),
                                );
                            }
                        } else if s.kind() == SampleKind::Put {
                            let mut lock = zlock!(statesref);
                            let states = &mut *lock;
                            tracing::trace!(
                                "AdvancedSubscriber{{key_expr: {}}}: Detect late joiner publishers with zid={}",
                                states.key_expr,
                                parsed.zid().as_str()
                            );
                            states.global_pending_queries += 1;

                            drop(lock);

                            let handler = InitialRepliesHandler {
                                statesref: statesref.clone(),
                            };
                            handler.issue(
                                &context,
                                Selector::from((s.key_expr(), historyconf.parameters())),
                            );
                        }
                    }
                };

                tracing::debug!(
                    "AdvancedSubscriber{{key_expr: {}}}: Detect late joiner publishers on {}",
                    key_expr,
                    &key_expr / KE_ADV_PREFIX / KE_PUB / KE_STARSTAR
                );
                Some(
                    conf.session
                        .liveliness()
                        .declare_subscriber(&key_expr / KE_ADV_PREFIX / KE_PUB / KE_STARSTAR)
                        // .declare_subscriber(keformat!(ke_liveliness_all::formatter(), zid = 0, eid = 0, remaining = key_expr).unwrap())
                        .history(true)
                        .callback(live_callback)
                        .wait()?,
                )
            } else {
                None
            }
        } else {
            None
        };

        let heartbeat_subscriber = if retransmission.is_some_and(|r| r.heartbeat) {
            let ke_heartbeat_sub = &key_expr / KE_ADV_PREFIX / KE_PUB / KE_STARSTAR;
            let statesref = statesref.clone();
            tracing::debug!(
                "AdvancedSubscriber{{key_expr: {}}}: Enable heartbeat subscriber on {}",
                key_expr,
                ke_heartbeat_sub
            );
            let heartbeat_sub = conf
                .session
                .declare_subscriber(ke_heartbeat_sub)
                .callback(move |sample_hb| {
                    if sample_hb.kind() != SampleKind::Put {
                        return;
                    }

                    let heartbeat_keyexpr = sample_hb.key_expr().as_keyexpr();
                    let Ok(parsed_keyexpr) = ke_liveliness::parse(heartbeat_keyexpr) else {
                        return;
                    };
                    let source_id = {
                        let Ok(zid) = ZenohId::from_str(parsed_keyexpr.zid().as_str()) else {
                            return;
                        };
                        let Ok(eid) = EntityId::from_str(parsed_keyexpr.eid().as_str()) else {
                            return;
                        };
                        EntityGlobalId::new(zid, eid)
                    };

                    let Ok(heartbeat_sn) = z_deserialize::<WrappingSn>(sample_hb.payload()) else {
                        tracing::debug!(
                            "AdvancedSubscriber{{}}: Skipping invalid heartbeat payload on '{}'",
                            heartbeat_keyexpr
                        );
                        return;
                    };

                    let mut lock = zlock!(statesref);
                    let states = &mut *lock;
                    let mut new = false;
                    let state = states.sequenced_states.get_or_insert_mut(source_id, ||{
                        new = true;
                        Default::default()
                    });
                    state.latest_access = Instant::now();
                    if new {
                        // NOTE: API does not allow both heartbeat and periodic_queries
                        state.periodic_task = SequencedRepliesHandler::spawn_periodic(&statesref, states.period, source_id);
                        if states.global_pending_queries > 0 {
                            tracing::trace!("AdvancedSubscriber{{key_expr: {}}}: Skipping heartbeat on '{}' from publisher that is currently being pulled by global query", states.key_expr, heartbeat_keyexpr);
                            return;
                        }
                    }

                    // check that it's not an old sn, and that there are no pending queries
                    if (state.last_delivered.is_none()
                        || state.last_delivered.is_some_and(|sn| heartbeat_sn > sn))
                        && state.pending_queries == 0
                    {
                        let seq_num_range = range(
                            "_sn", 
                            state.last_delivered.map(|s| s + 1),
                            Some(heartbeat_sn),
                        );

                        state.pending_queries += 1;
                        let context = QueryContext::new(states);
                        drop(lock);

                        let handler = SequencedRepliesHandler {
                            source_id,
                            statesref: statesref.clone(),
                        };
                        handler.issue(&context, Selector::from((heartbeat_keyexpr, seq_num_range)));
                    }
                })
                .allowed_origin(conf.origin)
                .wait()?;
            Some(heartbeat_sub)
        } else {
            None
        };

        if conf.liveliness {
            let suffix = KE_ADV_PREFIX
                / KE_SUB
                / &subscriber.id().zid().into_keyexpr()
                / &KeyExpr::try_from(subscriber.id().eid().to_string()).unwrap();
            let suffix = match meta {
                Some(meta) => suffix / &meta,
                // We need this empty chunk because of a routing matching bug
                _ => suffix / KE_EMPTY,
            };
            tracing::debug!(
                "AdvancedSubscriber{{key_expr: {}}}: Declare liveliness token {}",
                key_expr,
                &key_expr / &suffix,
            );
            let token = conf
                .session
                .liveliness()
                .declare_token(&key_expr / &suffix)
                .wait()?;
            zlock!(statesref).token = Some(token)
        }

        let reliable_subscriber = AdvancedSubscriber {
            statesref,
            subscriber,
            receiver,
            liveliness_subscriber,
            heartbeat_subscriber,
        };

        Ok(reliable_subscriber)
    }

    /// Returns the [`EntityGlobalId`] of this AdvancedSubscriber.
    #[zenoh_macros::unstable]
    pub fn id(&self) -> EntityGlobalId {
        self.subscriber.id()
    }

    /// Returns the [`KeyExpr`] this subscriber subscribes to.
    #[zenoh_macros::unstable]
    pub fn key_expr(&self) -> &KeyExpr<'static> {
        self.subscriber.key_expr()
    }

    /// Returns a reference to this subscriber's handler.
    ///
    /// An handler is anything that implements [`zenoh::handlers::IntoHandler`].
    /// The default handler is [`zenoh::handlers::DefaultHandler`].
    #[zenoh_macros::unstable]
    pub fn handler(&self) -> &Handler {
        &self.receiver
    }

    /// Returns a mutable reference to this subscriber's handler.
    ///
    /// An handler is anything that implements [`zenoh::handlers::IntoHandler`].
    /// The default handler is [`zenoh::handlers::DefaultHandler`].
    #[zenoh_macros::unstable]
    pub fn handler_mut(&mut self) -> &mut Handler {
        &mut self.receiver
    }

    /// Declares a listener to detect missed samples.
    ///
    /// Missed samples can only be detected from [`AdvancedPublisher`](crate::AdvancedPublisher) that
    /// enable [`sample_miss_detection`](crate::AdvancedPublisherBuilder::sample_miss_detection).
    #[zenoh_macros::unstable]
    pub fn sample_miss_listener(&self) -> SampleMissListenerBuilder<'_, DefaultHandler> {
        SampleMissListenerBuilder {
            statesref: &self.statesref,
            handler: DefaultHandler::default(),
        }
    }

    /// Declares a listener to detect matching publishers.
    ///
    /// Only [`AdvancedPublisher`](crate::AdvancedPublisher) that enable
    /// [`publisher_detection`](crate::AdvancedPublisherBuilder::publisher_detection) can be detected.
    #[zenoh_macros::unstable]
    pub fn detect_publishers(&self) -> LivelinessSubscriberBuilder<'_, '_, DefaultHandler> {
        self.subscriber
            .session()
            .liveliness()
            .declare_subscriber(self.subscriber.key_expr() / KE_ADV_PREFIX / KE_PUB / KE_STARSTAR)
    }

    /// Undeclares this AdvancedSubscriber
    #[inline]
    #[zenoh_macros::unstable]
    pub fn undeclare(self) -> SubscriberUndeclaration<()> {
        tracing::debug!(
            "AdvancedSubscriber{{key_expr: {}}}: Undeclare",
            self.key_expr()
        );
        self.subscriber.undeclare()
    }

    fn set_background_impl(&mut self, background: bool) {
        self.subscriber.set_background(background);
        if let Some(mut liveliness_sub) = self.liveliness_subscriber.take() {
            liveliness_sub.set_background(background);
        }
        if let Some(mut heartbeat_sub) = self.heartbeat_subscriber.take() {
            heartbeat_sub.set_background(background);
        }
    }

    #[zenoh_macros::internal]
    pub fn set_background(&mut self, background: bool) {
        self.set_background_impl(background)
    }
}

/// A struct that represent missed samples.
///
/// Reports are emitted when the delivery of a full sample crates a gap
/// in sequence number, not at fragment reception or eviction.
#[zenoh_macros::unstable]
#[derive(Debug, Clone)]
pub struct Miss {
    source: EntityGlobalId,
    nb: u32,
}

impl Miss {
    /// The source of missed samples.
    pub fn source(&self) -> EntityGlobalId {
        self.source
    }

    /// The number of missed samples.
    pub fn nb(&self) -> u32 {
        self.nb
    }
}

impl CallbackParameter for Miss {
    type Message<'a> = Self;

    fn from_message(msg: Self::Message<'_>) -> Self {
        msg
    }
}

/// A listener to detect missed samples.
///
/// Missed samples can only be detected from [`AdvancedPublisher`](crate::AdvancedPublisher) that
/// enable [`sample_miss_detection`](crate::AdvancedPublisherBuilder::sample_miss_detection).
#[zenoh_macros::unstable]
pub struct SampleMissListener<Handler> {
    id: usize,
    statesref: Arc<Mutex<State>>,
    handler: Handler,
    undeclare_on_drop: bool,
}

#[zenoh_macros::unstable]
impl<Handler> fmt::Debug for SampleMissListener<Handler> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SampleMissListener")
            .field("id", &self.id)
            .field("statesref", &"..")
            .field("handler", &"..")
            .field("undeclare_on_drop", &self.undeclare_on_drop)
            .finish()
    }
}

#[zenoh_macros::unstable]
impl<Handler> SampleMissListener<Handler> {
    #[inline]
    pub fn undeclare(self) -> SampleMissHandlerUndeclaration<Handler>
    where
        Handler: Send,
    {
        SampleMissHandlerUndeclaration { listener: self }
    }

    fn undeclare_impl(&mut self) -> ZResult<()> {
        // set the flag first to avoid double panic if this function panic
        self.undeclare_on_drop = false;
        zlock!(self.statesref).unregister_miss_callback(&self.id);
        Ok(())
    }

    #[zenoh_macros::internal]
    pub fn set_background(&mut self, background: bool) {
        self.undeclare_on_drop = !background;
    }
}

#[cfg(feature = "unstable")]
impl<Handler> Drop for SampleMissListener<Handler> {
    fn drop(&mut self) {
        if self.undeclare_on_drop {
            if let Err(error) = self.undeclare_impl() {
                tracing::error!(error);
            }
        }
    }
}

// #[zenoh_macros::unstable]
// impl<Handler: Send> UndeclarableSealed<()> for SampleMissHandler<Handler> {
//     type Undeclaration = SampleMissHandlerUndeclaration<Handler>;

//     fn undeclare_inner(self, _: ()) -> Self::Undeclaration {
//         SampleMissHandlerUndeclaration(self)
//     }
// }

#[zenoh_macros::unstable]
impl<Handler> std::ops::Deref for SampleMissListener<Handler> {
    type Target = Handler;

    fn deref(&self) -> &Self::Target {
        &self.handler
    }
}
#[zenoh_macros::unstable]
impl<Handler> std::ops::DerefMut for SampleMissListener<Handler> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.handler
    }
}

/// A [`Resolvable`] returned by [`SampleMissListener::undeclare`]
#[zenoh_macros::unstable]
pub struct SampleMissHandlerUndeclaration<Handler> {
    listener: SampleMissListener<Handler>,
}

#[zenoh_macros::unstable]
impl<Handler> fmt::Debug for SampleMissHandlerUndeclaration<Handler> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("SampleMissHandlerUndeclaration")
            .field(&self.listener)
            .finish()
    }
}

impl<Handler> SampleMissHandlerUndeclaration<Handler> {
    /// Block in undeclare operation until all currently running instances of sample miss listener callback (if any) return.
    pub fn wait_callbacks(self) -> Self {
        // Note: no particular synchronization is required as of now since miss listener callbacks are always executed
        // under state lock
        self
    }
}

#[zenoh_macros::unstable]
impl<Handler> Resolvable for SampleMissHandlerUndeclaration<Handler> {
    type To = ZResult<()>;
}

#[zenoh_macros::unstable]
impl<Handler> Wait for SampleMissHandlerUndeclaration<Handler> {
    fn wait(mut self) -> <Self as Resolvable>::To {
        self.listener.undeclare_impl()
    }
}

#[zenoh_macros::unstable]
impl<Handler> IntoFuture for SampleMissHandlerUndeclaration<Handler> {
    type Output = <Self as Resolvable>::To;
    type IntoFuture = Ready<<Self as Resolvable>::To>;

    fn into_future(self) -> Self::IntoFuture {
        std::future::ready(self.wait())
    }
}

/// A builder for initializing a [`SampleMissListener`].
#[zenoh_macros::unstable]
pub struct SampleMissListenerBuilder<'a, Handler, const BACKGROUND: bool = false> {
    statesref: &'a Arc<Mutex<State>>,
    handler: Handler,
}

#[zenoh_macros::unstable]
impl<Handler, const BACKGROUND: bool> fmt::Debug
    for SampleMissListenerBuilder<'_, Handler, BACKGROUND>
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SampleMissListenerBuilder")
            .field("statesref", &"..")
            .field("handler", &"..")
            .field("background", &BACKGROUND)
            .finish()
    }
}

#[zenoh_macros::unstable]
impl<'a> SampleMissListenerBuilder<'a, DefaultHandler> {
    /// Receive the sample miss notification with a callback.
    #[inline]
    #[zenoh_macros::unstable]
    pub fn callback<F>(self, callback: F) -> SampleMissListenerBuilder<'a, Callback<Miss>>
    where
        F: Fn(Miss) + Send + Sync + 'static,
    {
        self.with(Callback::from(callback))
    }

    /// Receive the sample miss notification with a mutable callback.
    #[inline]
    #[zenoh_macros::unstable]
    pub fn callback_mut<F>(self, callback: F) -> SampleMissListenerBuilder<'a, Callback<Miss>>
    where
        F: FnMut(Miss) + Send + Sync + 'static,
    {
        self.callback(zenoh::handlers::locked(callback))
    }

    /// Receive the sample miss notification with a [`Handler`](IntoHandler).
    #[inline]
    #[zenoh_macros::unstable]
    pub fn with<Handler>(self, handler: Handler) -> SampleMissListenerBuilder<'a, Handler>
    where
        Handler: IntoHandler<Miss>,
    {
        SampleMissListenerBuilder {
            statesref: self.statesref,
            handler,
        }
    }
}

#[zenoh_macros::unstable]
impl<'a> SampleMissListenerBuilder<'a, Callback<Miss>> {
    /// Make the sample miss notification run in the background until the advanced subscriber is undeclared.
    ///
    /// Background builder doesn't return a `SampleMissHandler` object anymore.
    #[zenoh_macros::unstable]
    pub fn background(self) -> SampleMissListenerBuilder<'a, Callback<Miss>, true> {
        SampleMissListenerBuilder {
            statesref: self.statesref,
            handler: self.handler,
        }
    }
}

#[zenoh_macros::unstable]
impl<Handler> Resolvable for SampleMissListenerBuilder<'_, Handler>
where
    Handler: IntoHandler<Miss> + Send,
    Handler::Handler: Send,
{
    type To = ZResult<SampleMissListener<Handler::Handler>>;
}

#[zenoh_macros::unstable]
impl<Handler> Wait for SampleMissListenerBuilder<'_, Handler>
where
    Handler: IntoHandler<Miss> + Send,
    Handler::Handler: Send,
{
    #[zenoh_macros::unstable]
    fn wait(self) -> <Self as Resolvable>::To {
        let (callback, handler) = self.handler.into_handler();
        let id = zlock!(self.statesref).register_miss_callback(callback);
        Ok(SampleMissListener {
            id,
            statesref: self.statesref.clone(),
            handler,
            undeclare_on_drop: true,
        })
    }
}

#[zenoh_macros::unstable]
impl<Handler> IntoFuture for SampleMissListenerBuilder<'_, Handler>
where
    Handler: IntoHandler<Miss> + Send,
    Handler::Handler: Send,
{
    type Output = <Self as Resolvable>::To;
    type IntoFuture = Ready<<Self as Resolvable>::To>;

    #[zenoh_macros::unstable]
    fn into_future(self) -> Self::IntoFuture {
        std::future::ready(self.wait())
    }
}

#[zenoh_macros::unstable]
impl Resolvable for SampleMissListenerBuilder<'_, Callback<Miss>, true> {
    type To = ZResult<()>;
}

#[zenoh_macros::unstable]
impl Wait for SampleMissListenerBuilder<'_, Callback<Miss>, true> {
    #[zenoh_macros::unstable]
    fn wait(self) -> <Self as Resolvable>::To {
        let (callback, _) = self.handler.into_handler();
        zlock!(self.statesref).register_miss_callback(callback);
        Ok(())
    }
}

#[zenoh_macros::unstable]
impl IntoFuture for SampleMissListenerBuilder<'_, Callback<Miss>, true> {
    type Output = <Self as Resolvable>::To;
    type IntoFuture = Ready<<Self as Resolvable>::To>;

    #[zenoh_macros::unstable]
    fn into_future(self) -> Self::IntoFuture {
        std::future::ready(self.wait())
    }
}

#[cfg(all(test, feature = "unstable"))]
mod tests {
    use std::{
        sync::{Arc, Mutex},
        time::Duration,
    };

    use zenoh::{
        bytes::ZBytes,
        config::{Config, WhatAmI},
        internal::{traits::SampleBuilderTrait, ztimeout},
        query::{ConsolidationMode, Query, Reply, ReplyKeyExpr},
        sample::{FragInfo, SampleBuilder, SourceInfo},
        time::Timestamp,
    };
    use zenoh_config::ModeDependentValue;

    use super::*;
    use crate::{AdvancedPublisherBuilderExt, AdvancedSubscriberBuilderExt};

    /// 12-byte payload, 3 fragments of 4 bytes with `fragmentation(4)`.
    const PAYLOAD: &str = "0123456789AB";
    const TIMEOUT: Duration = Duration::from_secs(60);

    /// Only the middle fragment of a 3-fragment sample is fed to
    /// `handle_sample`; the missing fragments must be recovered from the
    /// publisher's cache via `_sn`/`_fn`-range queries and reassembled.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_recovery_of_missing_fragments() {
        zenoh_util::init_log_from_env_or("error");

        let mut config = Config::default();
        config.scouting.multicast.set_enabled(Some(false)).unwrap();
        config.set_mode(Some(WhatAmI::Peer)).unwrap();
        let session = ztimeout!(zenoh::open(config)).unwrap();

        // Records query selectors; replies an error so `session.get` (target
        // `All`) completes immediately instead of at `query_timeout`.
        let spy_queries = Arc::new(Mutex::new(Vec::<String>::new()));
        let _spy = {
            let spy_queries = spy_queries.clone();
            ztimeout!(session
                .declare_queryable("test/ext/frag/recovery/@adv/**")
                .callback(move |q: Query| {
                    spy_queries.lock().unwrap().push(q.selector().to_string());
                    let _ = q.reply_err(ZBytes::new()).wait();
                }))
            .unwrap()
        };

        // Publisher cache answers the recovery queries.
        let publ = ztimeout!(session
            .declare_publisher("test/ext/frag/recovery")
            .advanced()
            .fragmentation(4)
            .cache(crate::CacheConfig::default().max_samples(10))
            .sample_miss_detection(crate::MissDetectionConfig::default()))
        .unwrap();
        let source_id = publ.id();
        ztimeout!(publ.put(PAYLOAD)).unwrap();

        // Reassembly state of an advanced subscriber with recovery enabled.
        let key_expr = KeyExpr::try_from("test/ext/frag/recovery").unwrap();
        let received = Arc::new(Mutex::new(Vec::<Sample>::new()));
        let statesref = {
            let received = received.clone();
            Arc::new_cyclic(|weak| {
                Mutex::new(State {
                    next_id: 0,
                    global_pending_queries: 0,
                    sequenced_states: LruCache::unbounded(),
                    timestamped_states: LruCache::unbounded(),
                    session: session.downgrade(),
                    key_expr: key_expr.clone().into_owned(),
                    retransmission: true,
                    frag_recovery_delay: RecoveryConfig::<true>::default().frag_recovery_delay,
                    period: None,
                    max_pending_samples: 10,
                    query_target: QueryTarget::All,
                    query_timeout: Duration::from_secs(10),
                    max_fragments: MAX_FRAGMENTS_DEFAULT,
                    callback: Some(Callback::from(move |s: Sample| {
                        received.lock().unwrap().push(s);
                    })),
                    miss_handlers: HashMap::new(),
                    token: None,
                    _gc_task: AbortOnDropHandle::new(
                        ZRuntime::Application
                            .spawn(gc_task(weak.clone(), Duration::from_secs(3600))),
                    ),
                })
            })
        };

        let frag1: Sample = SampleBuilder::put(key_expr.clone(), "4567")
            .frag_info(FragInfo::new(3, 1))
            .source_info(SourceInfo::new(source_id, 0))
            .into();

        let inserted = {
            let mut states = zlock!(statesref);
            handle_sample(&mut states, frag1)
        };
        assert!(inserted.new_source);
        assert!(inserted.new_fragment_slot);

        // Arm the fragment recovery scan like the live callback does.
        {
            let mut states = zlock!(statesref);
            let state = states.sequenced_states.get_mut(&source_id).unwrap();
            state.arm_fragment_recovery(&statesref, source_id, Duration::from_millis(500));
        }

        // Wait for reassembled delivery: fragments 0 and 2 must have come
        // from the cache, and no raw fragment must leak through.
        let mut delivered = None;
        for _ in 0..100 {
            if let Some(s) = received.lock().unwrap().first() {
                delivered = Some(s.payload().try_to_string().unwrap().to_string());
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        // The reassembled payload proves fragments 0 and 2 arrived from the
        // cache: the locally fed fragment only carried "4567".
        assert_eq!(
            delivered.as_deref(),
            Some(PAYLOAD),
            "fragmented sample was not recovered"
        );
        assert_eq!(received.lock().unwrap().len(), 1);

        {
            let mut states = zlock!(statesref);
            let state = states.sequenced_states.get(&source_id).unwrap();
            assert!(state.pending_samples.is_empty());
            assert_eq!(state.last_delivered, Some(WrappingSn(0)));
        }

        // One query per missing fragment range: `_fn=0..0` and `_fn=2..`.
        {
            let queries = spy_queries.lock().unwrap();
            assert!(
                queries.iter().filter(|q| q.contains("_sn=0..0")).count() >= 2,
                "one query per missing fragment range expected: {queries:?}"
            );
            assert!(
                queries.iter().any(|q| q.contains("_fn=0..0;")),
                "missing fragment range 0 must be queried: {queries:?}"
            );
            assert!(
                queries.iter().any(|q| q.contains("_fn=2..;")),
                "missing trailing fragment range must be queried: {queries:?}"
            );
        }

        let _ = ztimeout!(session.close());
    }

    /// Completing fragment recovery must resume sample recovery if a
    /// preceding sequence number is still missing, without another live sample
    /// or a periodic/heartbeat query to trigger it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_fragment_recovery_resumes_sample_recovery() {
        zenoh_util::init_log_from_env_or("error");
        let mut config = Config::default();
        config.scouting.multicast.set_enabled(Some(false)).unwrap();
        config.listen.endpoints.set(vec![]).unwrap();
        let session = ztimeout!(zenoh::open(config)).unwrap();
        let key_expr = "test/ext/frag/sequence_gap";

        // Fill the real publisher cache before subscribing. Injecting selected
        // samples below simulates loss without relying on transport timing.
        let publ = ztimeout!(session
            .declare_publisher(key_expr)
            .advanced()
            .fragmentation(4)
            .cache(crate::CacheConfig::default().max_samples(3))
            .sample_miss_detection(crate::MissDetectionConfig::default()))
        .unwrap();
        let source_id = publ.id();
        for payload in ["base", "lost", PAYLOAD] {
            ztimeout!(publ.put(payload)).unwrap();
        }

        let sub = ztimeout!(session.declare_subscriber(key_expr).advanced().recovery(
            RecoveryConfig::default().fragments_recovery_delay(Duration::from_millis(50)),
        ))
        .unwrap();
        let misses = ztimeout!(sub.sample_miss_listener()).unwrap();

        ztimeout!(session
            .put(key_expr, "base")
            .source_info(SourceInfo::new(source_id, 0)))
        .unwrap();
        let baseline = ztimeout!(sub.recv_async()).unwrap();
        assert_eq!(baseline.source_info().unwrap().source_sn(), 0);

        // SN 1 is entirely lost. Receiving fragment 1 of SN 2 opens a hole:
        // its fragment-recovery query temporarily delays the sample-recovery query.
        ztimeout!(session
            .put(key_expr, "4567")
            .source_info(SourceInfo::new(source_id, 2))
            .frag_info(FragInfo::new(3, 1)))
        .unwrap();

        for (sn, payload) in [(1, "lost"), (2, PAYLOAD)] {
            let received = tokio::time::timeout(Duration::from_secs(5), sub.recv_async()).await;
            if received.is_err() {
                let diagnostic = {
                    let states = zlock!(sub.statesref);
                    let state = states.sequenced_states.peek(&source_id).unwrap();
                    format!(
                        "SN {sn} was not delivered after fragment recovery: \
                         last_delivered={:?}, pending_queries={}, pending_samples={:?}",
                        state.last_delivered,
                        state.pending_queries,
                        state
                            .pending_samples
                            .iter()
                            .map(|(sn, sample)| (*sn, sample.is_complete()))
                            .collect::<Vec<_>>()
                    )
                };
                ztimeout!(session.close()).unwrap();
                panic!("{diagnostic}");
            }
            let sample = received.unwrap().unwrap();
            assert_eq!(sample.source_info().unwrap().source_sn(), sn);
            assert_eq!(sample.payload().try_to_string().unwrap().as_ref(), payload);
        }
        assert!(sub.try_recv().unwrap().is_none());
        assert!(misses.try_recv().unwrap().is_none());
        ztimeout!(session.close()).unwrap();
    }

    /// One recurring scan must recover any number of concurrently incomplete
    /// samples: arming for a newer sample must not cancel the recovery of an
    /// older one.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_fragment_recovery_recovers_multiple_incomplete_samples() {
        zenoh_util::init_log_from_env_or("error");

        let mut config = Config::default();
        config.scouting.multicast.set_enabled(Some(false)).unwrap();
        config.set_mode(Some(WhatAmI::Peer)).unwrap();
        let session = ztimeout!(zenoh::open(config)).unwrap();

        // Records query selectors; replies an error so `session.get` (target
        // `All`) completes immediately instead of at `query_timeout`.
        let spy_queries = Arc::new(Mutex::new(Vec::<String>::new()));
        let _spy = {
            let spy_queries = spy_queries.clone();
            ztimeout!(session
                .declare_queryable("test/ext/frag/recovery2/@adv/**")
                .callback(move |q: Query| {
                    spy_queries.lock().unwrap().push(q.selector().to_string());
                    let _ = q.reply_err(ZBytes::new()).wait();
                }))
            .unwrap()
        };

        // Publisher cache answers the recovery queries; the two puts land at
        // SN 0 and SN 1, 3 fragments each.
        let publ = ztimeout!(session
            .declare_publisher("test/ext/frag/recovery2")
            .advanced()
            .fragmentation(4)
            .cache(crate::CacheConfig::default().max_samples(10))
            .sample_miss_detection(crate::MissDetectionConfig::default()))
        .unwrap();
        let source_id = publ.id();
        ztimeout!(publ.put("0123456789AB")).unwrap();
        ztimeout!(publ.put("abcdefghijkl")).unwrap();

        // Reassembly state of an advanced subscriber with recovery enabled.
        let key_expr = KeyExpr::try_from("test/ext/frag/recovery2").unwrap();
        let received = Arc::new(Mutex::new(Vec::<Sample>::new()));
        let statesref = {
            let received = received.clone();
            Arc::new_cyclic(|weak| {
                Mutex::new(State {
                    next_id: 0,
                    global_pending_queries: 0,
                    sequenced_states: LruCache::unbounded(),
                    timestamped_states: LruCache::unbounded(),
                    session: session.downgrade(),
                    key_expr: key_expr.clone().into_owned(),
                    retransmission: true,
                    frag_recovery_delay: RecoveryConfig::<true>::default().frag_recovery_delay,
                    period: None,
                    max_pending_samples: 10,
                    query_target: QueryTarget::All,
                    query_timeout: Duration::from_secs(10),
                    max_fragments: MAX_FRAGMENTS_DEFAULT,
                    callback: Some(Callback::from(move |s: Sample| {
                        received.lock().unwrap().push(s);
                    })),
                    miss_handlers: HashMap::new(),
                    token: None,
                    _gc_task: AbortOnDropHandle::new(
                        ZRuntime::Application
                            .spawn(gc_task(weak.clone(), Duration::from_secs(3600))),
                    ),
                })
            })
        };

        // Feed only the middle fragment of each sample, arming the scan like
        // the live callback does on every new fragmented slot.
        for (frag_payload, sn) in [("4567", 0u32), ("efgh", 1u32)] {
            let frag: Sample = SampleBuilder::put(key_expr.clone(), frag_payload)
                .frag_info(FragInfo::new(3, 1))
                .source_info(SourceInfo::new(source_id, sn))
                .into();
            let inserted = {
                let mut states = zlock!(statesref);
                handle_sample(&mut states, frag)
            };
            assert!(inserted.new_fragment_slot);
            let mut states = zlock!(statesref);
            let state = states.sequenced_states.get_mut(&source_id).unwrap();
            state.arm_fragment_recovery(&statesref, source_id, Duration::from_millis(500));
        }

        // Wait for both reassembled samples to be delivered, in order.
        let mut delivered: Vec<String> = Vec::new();
        for _ in 0..100 {
            let done = {
                let received = received.lock().unwrap();
                if received.len() >= 2 {
                    delivered = received
                        .iter()
                        .map(|s| s.payload().try_to_string().unwrap().to_string())
                        .collect();
                    true
                } else {
                    false
                }
            };
            if done {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert_eq!(
            delivered,
            vec!["0123456789AB", "abcdefghijkl"],
            "both fragmented samples must be recovered"
        );
        assert_eq!(received.lock().unwrap().len(), 2);

        {
            let mut states = zlock!(statesref);
            let state = states.sequenced_states.get(&source_id).unwrap();
            assert!(state.pending_samples.is_empty());
            assert_eq!(state.last_delivered, Some(WrappingSn(1)));
        }

        // One query per missing fragment range of each SN: `_fn=0..0` and
        // `_fn=2..` for both SN 0 and SN 1.
        {
            let queries = spy_queries.lock().unwrap();
            assert!(
                queries.iter().filter(|q| q.contains("_sn=0..0")).count() >= 2,
                "SN 0 missing ranges must be queried: {queries:?}"
            );
            assert!(
                queries.iter().filter(|q| q.contains("_sn=1..1")).count() >= 2,
                "SN 1 missing ranges must be queried: {queries:?}"
            );
        }

        let _ = ztimeout!(session.close());
    }

    /// Build a reassembly state with recovery enabled, collecting deliveries.
    pub(super) fn frag_recovery_state(
        session: &Session,
        key_expr: &KeyExpr<'static>,
        received: Arc<Mutex<Vec<Sample>>>,
    ) -> Arc<Mutex<State>> {
        Arc::new(Mutex::new(State {
            next_id: 0,
            global_pending_queries: 0,
            sequenced_states: LruCache::unbounded(),
            timestamped_states: LruCache::unbounded(),
            session: session.downgrade(),
            key_expr: key_expr.clone().into_owned(),
            retransmission: true,
            frag_recovery_delay: RecoveryConfig::<true>::default().frag_recovery_delay,
            period: None,
            max_pending_samples: 10,
            query_target: QueryTarget::All,
            query_timeout: Duration::from_secs(10),
            max_fragments: MAX_FRAGMENTS_DEFAULT,
            callback: Some(Callback::from(move |s: Sample| {
                received.lock().unwrap().push(s);
            })),
            miss_handlers: HashMap::new(),
            token: None,
            _gc_task: AbortOnDropHandle::new(ZRuntime::Application.spawn(std::future::pending())),
        }))
    }

    /// Insert a fragment and run fragment recovery like the live callback
    /// does: buffer the fragment and count any new query under the lock,
    /// then schedule the query on the application runtime after unlocking.
    #[allow(clippy::too_many_arguments)]
    fn feed_fragment(
        statesref: &Arc<Mutex<State>>,
        key_expr: &KeyExpr<'static>,
        source_id: EntityGlobalId,
        payload: &str,
        frag_count: u32,
        frag_num: u32,
        sn: u32,
        delay: Duration,
    ) {
        let frag: Sample = SampleBuilder::put(key_expr.clone(), payload)
            .frag_info(FragInfo::new(frag_count, frag_num))
            .source_info(SourceInfo::new(source_id, sn))
            .into();
        let mut states = zlock!(statesref);
        let inserted = handle_sample(&mut states, frag);
        let request = states
            .sequenced_states
            .get_mut(&source_id)
            .unwrap()
            .on_fragment(
                statesref,
                source_id,
                sn.into(),
                inserted.new_fragment_slot,
                delay,
            );
        let generation = states
            .sequenced_states
            .peek(&source_id)
            .unwrap()
            .generation
            .clone();
        let context = QueryContext::new(&states);
        drop(states);
        if let Some(request) = request {
            ZRuntime::Application.spawn(context.fragment_query_task(
                statesref,
                source_id,
                &generation,
                request,
            ));
        }
    }

    /// Duplicate activity must not prevent the recovery scan from querying a
    /// stalled tail and delivering the reassembled sample.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_fragment_recovery_tail_survives_duplicate_activity() {
        let delay = Duration::from_millis(50);
        let mut config = Config::default();
        config.scouting.multicast.set_enabled(Some(false)).unwrap();
        config.listen.endpoints.set(vec![]).unwrap();
        let session = ztimeout!(zenoh::open(config)).unwrap();
        let source_id = EntityGlobalId::new(session.zid(), 7);
        let key_expr = KeyExpr::try_from("test/ext/frag/duplicate_tail").unwrap();
        let cache =
            ztimeout!(session.declare_queryable("test/ext/frag/duplicate_tail/@adv/**")).unwrap();
        let received = Arc::new(Mutex::new(Vec::<Sample>::new()));
        let statesref = frag_recovery_state(&session, &key_expr, received.clone());
        {
            let mut states = zlock!(statesref);
            handle_sample(
                &mut states,
                SampleBuilder::put(key_expr.clone(), "base")
                    .source_info(SourceInfo::new(source_id, 0))
                    .into(),
            );
            let fragment: Sample = SampleBuilder::put(key_expr.clone(), "ab")
                .source_info(SourceInfo::new(source_id, 1))
                .frag_info(FragInfo::new(2, 0))
                .into();
            let inserted = handle_sample(&mut states, fragment.clone());
            let state = states.sequenced_states.peek_mut(&source_id).unwrap();
            assert!(state
                .on_fragment(
                    &statesref,
                    source_id,
                    WrappingSn(1),
                    inserted.new_fragment_slot,
                    delay,
                )
                .is_none());
            let FragmentedSample::Partial { last_progress, .. } =
                state.pending_samples.get_mut(&WrappingSn(1)).unwrap()
            else {
                panic!("expected partial assembly");
            };
            *last_progress = Instant::now() - Duration::from_secs(1);

            // Age and duplicate under one lock so the scan cannot run between
            // them. Duplicates must leave the tail immediately recoverable.
            for _ in 0..5 {
                handle_sample(&mut states, fragment.clone());
            }
            assert_eq!(
                states
                    .sequenced_states
                    .peek(&source_id)
                    .unwrap()
                    .pending_samples
                    .get(&WrappingSn(1))
                    .unwrap()
                    .clone()
                    .prepare_recovery(delay, true)
                    .map(|(ranges, _)| ranges),
                Some(vec![(Some(1), None)])
            );
        }

        let query = ztimeout!(cache.recv_async()).unwrap();
        assert_eq!(query.parameters().get("_sn"), Some("1..1"));
        assert_eq!(query.parameters().get("_fn"), Some("1.."));
        ztimeout!(query.reply_sample(
            SampleBuilder::put(key_expr, "cd")
                .source_info(SourceInfo::new(source_id, 1))
                .frag_info(FragInfo::new(2, 1))
                .into(),
        ))
        .unwrap();
        drop(query);
        ztimeout!(async {
            loop {
                if received.lock().unwrap().len() == 2 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        });
        {
            let received = received.lock().unwrap();
            assert_eq!(received.len(), 2);
            assert_eq!(received[0].source_info().unwrap().source_sn(), 0);
            assert_eq!(received[1].source_info().unwrap().source_sn(), 1);
            assert_eq!(received[1].payload().try_to_string().unwrap(), "abcd");
        }
        assert!(cache.try_recv().unwrap().is_none());
        ztimeout!(session.close()).unwrap();
    }

    /// A hole opened by an out-of-order fragment (e.g. receiving 0, 1, 3 of
    /// 5) must be queried immediately, well before the first recovery scan
    /// tick, and must not be re-queried immediately by every subsequent
    /// fragment arrival.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_fragment_recovery_immediate_hole_query() {
        zenoh_util::init_log_from_env_or("error");
        const DELAY: Duration = Duration::from_millis(300);

        let mut config = Config::default();
        config.scouting.multicast.set_enabled(Some(false)).unwrap();
        config.set_mode(Some(WhatAmI::Peer)).unwrap();
        let session = ztimeout!(zenoh::open(config)).unwrap();

        // Hold the attempt open while live arrivals and scan ticks occur.
        let held_queries = Arc::new(Mutex::new(Vec::<Query>::new()));
        let spy_queries = Arc::new(Mutex::new(Vec::<String>::new()));
        let _spy = {
            let spy_queries = spy_queries.clone();
            let held_queries = held_queries.clone();
            ztimeout!(session
                .declare_queryable("test/ext/frag/hole/@adv/**")
                .callback(move |q: Query| {
                    spy_queries.lock().unwrap().push(q.selector().to_string());
                    held_queries.lock().unwrap().push(q);
                }))
            .unwrap()
        };

        let publ = ztimeout!(session
            .declare_publisher("test/ext/frag/hole")
            .advanced()
            .fragmentation(4)
            .cache(crate::CacheConfig::default().max_samples(10))
            .sample_miss_detection(crate::MissDetectionConfig::default()))
        .unwrap();
        let source_id = publ.id();

        let key_expr: KeyExpr<'static> = KeyExpr::try_from("test/ext/frag/hole").unwrap();
        let received = Arc::new(Mutex::new(Vec::<Sample>::new()));
        let statesref = frag_recovery_state(&session, &key_expr, received.clone());

        // Sequential prefix 0, 1 of 5: no hole, only an active tail. No query
        // must be fired at all.
        feed_fragment(&statesref, &key_expr, source_id, "0123", 5, 0, 0, DELAY);
        feed_fragment(&statesref, &key_expr, source_id, "4567", 5, 1, 0, DELAY);
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            spy_queries.lock().unwrap().is_empty(),
            "sequential fragments must not trigger recovery queries: {:?}",
            spy_queries.lock().unwrap()
        );

        // Fragment 3 arrives out of order: hole (2, 2) opens and must be
        // queried immediately (the first scan tick is only at ~300ms).
        feed_fragment(&statesref, &key_expr, source_id, "CDEF", 5, 3, 0, DELAY);
        tokio::time::sleep(Duration::from_millis(50)).await;
        {
            let queries = spy_queries.lock().unwrap();
            let hole_queries = queries.iter().filter(|q| q.contains("_fn=2..2")).count();
            assert_eq!(
                hole_queries, 1,
                "exactly one immediate query for the new hole expected: {queries:?}"
            );
            assert!(
                queries.iter().all(|q| q.contains("_sn=0..0")),
                "only SN 0 must be queried: {queries:?}"
            );
        }

        // Fragment 4 arrives: the hole (2, 2) is unchanged and already
        // recorded, so no additional immediate query (the scan only ticks at
        // ~300ms).
        feed_fragment(&statesref, &key_expr, source_id, "GHIJ", 5, 4, 0, DELAY);
        tokio::time::sleep(Duration::from_millis(50)).await;
        {
            let queries = spy_queries.lock().unwrap();
            let hole_queries = queries.iter().filter(|q| q.contains("_fn=2..2")).count();
            assert_eq!(
                hole_queries, 1,
                "a recorded hole must not be re-queried immediately: {queries:?}"
            );
        }

        // Scan ticks must not overlap the outstanding attempt.
        tokio::time::sleep(DELAY + Duration::from_millis(100)).await;
        {
            let queries = spy_queries.lock().unwrap();
            assert_eq!(
                queries.iter().filter(|q| q.contains("_fn=2..2")).count(),
                1,
                "the scan must not overlap the outstanding attempt: {queries:?}"
            );
        }

        // Feed the missing fragment: the sample completes, is delivered and
        // the scan is aborted — no task is left ticking.
        feed_fragment(&statesref, &key_expr, source_id, "89AB", 5, 2, 0, DELAY);
        let mut delivered = None;
        for _ in 0..50 {
            let first = {
                let received = received.lock().unwrap();
                received
                    .first()
                    .map(|s| s.payload().try_to_string().unwrap().to_string())
            };
            if let Some(s) = first {
                delivered = Some(s);
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(delivered.as_deref(), Some("0123456789ABCDEFGHIJ"));
        held_queries.lock().unwrap().clear();
        {
            let mut states = zlock!(statesref);
            let state = states.sequenced_states.get(&source_id).unwrap();
            assert!(state.pending_samples.is_empty());
            assert!(state.frag_recovery_task.is_none());
        }

        let _ = ztimeout!(session.close());
    }

    /// The tail of a sample (fragments following the highest received one)
    /// must not be queried while the sequential stream delivering it is
    /// active; it must be queried once the stream stalls.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_fragment_recovery_tail_queried_when_stalled() {
        zenoh_util::init_log_from_env_or("error");
        const DELAY: Duration = Duration::from_millis(300);

        let mut config = Config::default();
        config.scouting.multicast.set_enabled(Some(false)).unwrap();
        config.set_mode(Some(WhatAmI::Peer)).unwrap();
        let session = ztimeout!(zenoh::open(config)).unwrap();

        // Keep the query open until the remaining live fragments arrive.
        let held_queries = Arc::new(Mutex::new(Vec::<Query>::new()));
        let spy_queries = Arc::new(Mutex::new(Vec::<String>::new()));
        let _spy = {
            let spy_queries = spy_queries.clone();
            let held_queries = held_queries.clone();
            ztimeout!(session
                .declare_queryable("test/ext/frag/tail/@adv/**")
                .callback(move |q: Query| {
                    spy_queries.lock().unwrap().push(q.selector().to_string());
                    held_queries.lock().unwrap().push(q);
                }))
            .unwrap()
        };

        let publ = ztimeout!(session
            .declare_publisher("test/ext/frag/tail")
            .advanced()
            .fragmentation(4)
            .cache(crate::CacheConfig::default().max_samples(10))
            .sample_miss_detection(crate::MissDetectionConfig::default()))
        .unwrap();
        let source_id = publ.id();

        let key_expr: KeyExpr<'static> = KeyExpr::try_from("test/ext/frag/tail").unwrap();
        let received = Arc::new(Mutex::new(Vec::<Sample>::new()));
        let statesref = frag_recovery_state(&session, &key_expr, received.clone());

        // Sequential prefix 0, 1 of 4: only the (active) tail (2, ..) is
        // missing. No query must be fired while the stream looks active.
        feed_fragment(&statesref, &key_expr, source_id, "0123", 4, 0, 0, DELAY);
        tokio::time::sleep(Duration::from_millis(100)).await;
        feed_fragment(&statesref, &key_expr, source_id, "4567", 4, 1, 0, DELAY);
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            spy_queries.lock().unwrap().is_empty(),
            "an active sequential stream must not trigger recovery queries: {:?}",
            spy_queries.lock().unwrap()
        );

        // The first scan tick (~300ms after arming) sees the stream as
        // recently active (fragment 1 arrived ~200ms before it) and must
        // still not query the tail.
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(
            spy_queries.lock().unwrap().is_empty(),
            "the tail must not be queried while the stream looks active: {:?}",
            spy_queries.lock().unwrap()
        );

        // The stream stalls: the tail must eventually be queried.
        let mut tail_queried = false;
        for _ in 0..40 {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let queries = spy_queries.lock().unwrap();
            // Tail `_fn=2..` (open-ended): contains `_fn=2..` but is not a
            // closed hole `_fn=2..2`.
            if queries
                .iter()
                .any(|q| q.contains("_fn=2..") && !q.contains("_fn=2..2"))
            {
                tail_queried = true;
                break;
            }
        }
        assert!(
            tail_queried,
            "stalled tail must be queried: {:?}",
            spy_queries.lock().unwrap()
        );

        // Feed the remaining fragments: the sample completes, is delivered
        // and the scan is aborted — no task is left ticking.
        feed_fragment(&statesref, &key_expr, source_id, "89AB", 4, 2, 0, DELAY);
        feed_fragment(&statesref, &key_expr, source_id, "CDEF", 4, 3, 0, DELAY);
        let mut delivered = None;
        for _ in 0..50 {
            let first = {
                let received = received.lock().unwrap();
                received
                    .first()
                    .map(|s| s.payload().try_to_string().unwrap().to_string())
            };
            if let Some(s) = first {
                delivered = Some(s);
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(delivered.as_deref(), Some("0123456789ABCDEF"));
        held_queries.lock().unwrap().clear();
        {
            let mut states = zlock!(statesref);
            let state = states.sequenced_states.get(&source_id).unwrap();
            assert!(state.pending_samples.is_empty());
            assert!(state.frag_recovery_task.is_none());
        }

        let _ = ztimeout!(session.close());
    }

    /// Empty, erroneous, partial and timed-out fragment replies must all
    /// release a complete successor rather than retrying the failed sample.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_failed_fragment_recovery_unblocks_successor() {
        let mut config = Config::default();
        config.scouting.multicast.set_enabled(Some(false)).unwrap();
        config.listen.endpoints.set(vec![]).unwrap();
        let session = ztimeout!(zenoh::open(config)).unwrap();
        let source_id = EntityGlobalId::new(session.zid(), 7);
        let key = KeyExpr::try_from("test/ext/frag/failure").unwrap();
        for failure in ["empty", "error", "partial", "timeout", "unavailable"] {
            let mut cache = Some(
                ztimeout!(session.declare_queryable("test/ext/frag/failure/@adv/**")).unwrap(),
            );
            let sub = ztimeout!(session
                .declare_subscriber(&key)
                .advanced()
                .recovery(
                    RecoveryConfig::default().fragments_recovery_delay(Duration::from_millis(20))
                )
                .query_timeout(Duration::from_millis(150)))
            .unwrap();
            let misses = ztimeout!(sub.sample_miss_listener()).unwrap();
            ztimeout!(session
                .put(&key, "base")
                .source_info(SourceInfo::new(source_id, 0)))
            .unwrap();
            assert_eq!(
                ztimeout!(sub.recv_async())
                    .unwrap()
                    .source_info()
                    .unwrap()
                    .source_sn(),
                0
            );
            if failure == "unavailable" {
                // Simulate loss of the publisher's cache before recovery starts.
                drop(cache.take());
            }
            ztimeout!(session
                .put(&key, "ab")
                .source_info(SourceInfo::new(source_id, 1))
                .frag_info(FragInfo::new(3, 0)))
            .unwrap();
            ztimeout!(session
                .put(&key, "next")
                .source_info(SourceInfo::new(source_id, 2)))
            .unwrap();
            let mut held_query = None;
            if let Some(cache) = cache.as_ref() {
                let query = ztimeout!(cache.recv_async()).unwrap();
                assert_eq!(query.parameters().get("_fn"), Some("1.."));
                assert!(sub.try_recv().unwrap().is_none());
                match failure {
                    "error" => {
                        ztimeout!(query.reply_err("unavailable")).unwrap();
                    }
                    "partial" => {
                        ztimeout!(query.reply_sample(
                            SampleBuilder::put(key.clone(), "cd")
                                .source_info(SourceInfo::new(source_id, 1))
                                .frag_info(FragInfo::new(3, 1))
                                .into()
                        ))
                        .unwrap();
                    }
                    _ => {}
                }
                if failure == "timeout" {
                    held_query = Some(query);
                } else {
                    drop(query);
                }
            }
            let next = ztimeout!(sub.recv_async()).unwrap();
            assert_eq!(next.source_info().unwrap().source_sn(), 2, "{failure}");
            assert_eq!(ztimeout!(misses.recv_async()).unwrap().nb(), 1);
            // A delayed reply or live fragment cannot resurrect SN 1.
            if let Some(query) = held_query {
                let _ = query
                    .reply_sample(
                        SampleBuilder::put(key.clone(), "ef")
                            .source_info(SourceInfo::new(source_id, 1))
                            .frag_info(FragInfo::new(3, 2))
                            .into(),
                    )
                    .wait();
            }
            for num in 0..3 {
                ztimeout!(session
                    .put(&key, "late")
                    .source_info(SourceInfo::new(source_id, 1))
                    .frag_info(FragInfo::new(3, num)))
                .unwrap();
            }
            assert!(sub.try_recv().unwrap().is_none());
            assert!(misses.try_recv().unwrap().is_none());
            if let Some(cache) = cache.as_ref() {
                assert!(
                    cache.try_recv().unwrap().is_none(),
                    "overlapping/retried query: {failure}"
                );
            }
        }
        ztimeout!(session.close()).unwrap();
    }

    /// Periodic ticks must not overlap a held query: its completion must flush
    /// a complete successor across an unrecoverable sequence-number gap.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_periodic_queries_coalesce_and_flush_unrecoverable_gap() {
        let mut config = Config::default();
        config.scouting.multicast.set_enabled(Some(false)).unwrap();
        config.listen.endpoints.set(vec![]).unwrap();
        let session = ztimeout!(zenoh::open(config)).unwrap();
        let source_id = EntityGlobalId::new(session.zid(), 7);
        let cache = ztimeout!(session.declare_queryable("test/ext/pending/@adv/**")).unwrap();
        let received = Arc::new(Mutex::new(Vec::<Sample>::new()));
        let misses = Arc::new(Mutex::new(Vec::<u32>::new()));
        let statesref = pending_bound_state(&session, 10, received.clone(), misses.clone());
        {
            let mut states = zlock!(statesref);
            states.retransmission = true;
            for (sn, payload) in [(0, "base"), (2, "next")] {
                handle_sample(
                    &mut states,
                    SampleBuilder::put(KeyExpr::try_from("test/ext/pending").unwrap(), payload)
                        .source_info(SourceInfo::new(source_id, sn))
                        .into(),
                );
            }
        }

        // Invoke the tick helper directly to avoid timing-dependent overlap.
        SequencedRepliesHandler::periodic(&statesref, source_id);
        let query = ztimeout!(cache.recv_async()).unwrap();
        assert_eq!(query.parameters().get("_sn"), Some("1.."));
        for _ in 0..5 {
            SequencedRepliesHandler::periodic(&statesref, source_id);
            let states = zlock!(statesref);
            let state = states.sequenced_states.peek(&source_id).unwrap();
            assert_eq!(state.pending_queries, 1);
            assert_eq!(state.last_delivered, Some(WrappingSn(0)));
        }
        assert!(cache.try_recv().unwrap().is_none());
        assert_eq!(received.lock().unwrap().len(), 1);

        // SN 1 is no longer cached. Finishing the only query must release SN 2.
        drop(query);
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
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        });
        let delivered: Vec<_> = received
            .lock()
            .unwrap()
            .iter()
            .map(|s| s.source_info().unwrap().source_sn())
            .collect();
        assert_eq!(delivered, [0, 2]);
        assert_eq!(*misses.lock().unwrap(), [1]);
        assert!(zlock!(statesref)
            .sequenced_states
            .peek(&source_id)
            .unwrap()
            .pending_samples
            .is_empty());

        // Coalescing must not disable subsequent periodic recovery.
        SequencedRepliesHandler::periodic(&statesref, source_id);
        let query = ztimeout!(cache.recv_async()).unwrap();
        assert_eq!(query.parameters().get("_sn"), Some("3.."));
        assert_eq!(
            zlock!(statesref)
                .sequenced_states
                .peek(&source_id)
                .unwrap()
                .pending_queries,
            1
        );
        drop(query);
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
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        });
        assert_eq!(received.lock().unwrap().len(), 2);
        assert_eq!(*misses.lock().unwrap(), [1]);
        ztimeout!(session.close()).unwrap();
    }

    /// Do not start a periodic query while a history query or another recovery
    /// query for this source is still running.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_periodic_queries_respect_pending_queries() {
        let mut config = Config::default();
        config.scouting.multicast.set_enabled(Some(false)).unwrap();
        config.listen.endpoints.set(vec![]).unwrap();
        let session = ztimeout!(zenoh::open(config)).unwrap();
        let source_id = EntityGlobalId::new(session.zid(), 7);
        let cache = ztimeout!(session.declare_queryable("test/ext/pending/@adv/**")).unwrap();
        let statesref = pending_bound_state(
            &session,
            10,
            Arc::new(Mutex::new(Vec::new())),
            Arc::new(Mutex::new(Vec::new())),
        );
        for (global_pending, source_pending) in [(1, 0), (0, 1)] {
            {
                let mut states = zlock!(statesref);
                states.global_pending_queries = global_pending;
                states
                    .sequenced_states
                    .get_or_insert_mut(source_id, Default::default)
                    .pending_queries = source_pending;
            }
            SequencedRepliesHandler::periodic(&statesref, source_id);
            {
                let states = zlock!(statesref);
                assert_eq!(states.global_pending_queries, global_pending);
                assert_eq!(
                    states
                        .sequenced_states
                        .peek(&source_id)
                        .unwrap()
                        .pending_queries,
                    source_pending
                );
            }
            assert!(cache.try_recv().unwrap().is_none());
        }
        {
            let mut states = zlock!(statesref);
            states
                .sequenced_states
                .peek_mut(&source_id)
                .unwrap()
                .pending_queries = 0;
        }
        SequencedRepliesHandler::periodic(&statesref, source_id);
        let query = ztimeout!(cache.recv_async()).unwrap();
        assert_eq!(query.parameters().get("_sn"), Some(".."));
        drop(query);
        ztimeout!(session.close()).unwrap();
    }

    /// A final sample stays recoverable after an empty/error/timeout response,
    /// through either published fragments or replies to a later sample-recovery query.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_final_fragmented_sample_recovers_after_failure() {
        let mut config = Config::default();
        config.scouting.multicast.set_enabled(Some(false)).unwrap();
        config.listen.endpoints.set(vec![]).unwrap();
        let session = ztimeout!(zenoh::open(config)).unwrap();
        let source_id = EntityGlobalId::new(session.zid(), 7);
        let key = KeyExpr::try_from("test/ext/frag/final_retry").unwrap();
        for failure in ["empty", "error", "timeout"] {
            for recovery in [
                "live",
                "sample-query-fragments",
                "sample-query-unfragmented",
            ] {
                let cache =
                    ztimeout!(session.declare_queryable("test/ext/frag/final_retry/@adv/**"))
                        .unwrap();
                let sub = ztimeout!(session
                    .declare_subscriber(&key)
                    .advanced()
                    .recovery(
                        RecoveryConfig::default().fragments_recovery_delay(Duration::from_secs(60))
                    )
                    .query_timeout(Duration::from_millis(100)))
                .unwrap();
                let misses = ztimeout!(sub.sample_miss_listener()).unwrap();
                ztimeout!(session
                    .put(&key, "base")
                    .source_info(SourceInfo::new(source_id, 0)))
                .unwrap();
                ztimeout!(sub.recv_async()).unwrap();
                ztimeout!(session
                    .put(&key, "ef")
                    .source_info(SourceInfo::new(source_id, 1))
                    .frag_info(FragInfo::new(3, 2)))
                .unwrap();
                let query = ztimeout!(cache.recv_async()).unwrap();
                assert_eq!(query.parameters().get("_fn"), Some("0..1"));
                if failure == "error" {
                    ztimeout!(query.reply_err("temporarily unavailable")).unwrap();
                }
                let held = if failure == "timeout" {
                    Some(query)
                } else {
                    drop(query);
                    None
                };
                ztimeout!(async {
                    loop {
                        let failed = {
                            let states = zlock!(sub.statesref);
                            let state = states.sequenced_states.peek(&source_id).unwrap();
                            state.pending_queries == 0
                                && state
                                    .pending_samples
                                    .get(&WrappingSn(1))
                                    .is_some_and(FragmentedSample::can_abandon)
                        };
                        if failed {
                            break;
                        }
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                });
                drop(held);
                {
                    let states = zlock!(sub.statesref);
                    let slot = states
                        .sequenced_states
                        .peek(&source_id)
                        .unwrap()
                        .pending_samples
                        .get(&WrappingSn(1))
                        .unwrap();
                    assert!(slot.is_incomplete());
                    assert!(!slot.is_abandoned());
                    assert_eq!(slot.iter_frags().count(), 1);
                }
                assert!(sub.try_recv().unwrap().is_none());
                assert!(misses.try_recv().unwrap().is_none());
                assert!(cache.try_recv().unwrap().is_none());

                if recovery == "live" {
                    for (num, payload) in [(0, "ab"), (1, "cd")] {
                        ztimeout!(session
                            .put(&key, payload)
                            .source_info(SourceInfo::new(source_id, 1))
                            .frag_info(FragInfo::new(3, num)))
                        .unwrap();
                    }
                } else {
                    SequencedRepliesHandler::periodic(&sub.statesref, source_id);
                    let query = ztimeout!(cache.recv_async()).unwrap();
                    assert_eq!(query.parameters().get("_sn"), Some("1.."));
                    assert!(query.parameters().get("_fn").is_none());
                    if recovery == "sample-query-unfragmented" {
                        ztimeout!(query.reply_sample(
                            SampleBuilder::put(key.clone(), "abcdef")
                                .source_info(SourceInfo::new(source_id, 1))
                                .into()
                        ))
                        .unwrap();
                    } else {
                        for (num, payload) in [(0, "ab"), (1, "cd")] {
                            ztimeout!(query.reply_sample(
                                SampleBuilder::put(key.clone(), payload)
                                    .source_info(SourceInfo::new(source_id, 1))
                                    .frag_info(FragInfo::new(3, num))
                                    .into()
                            ))
                            .unwrap();
                        }
                    }
                    drop(query);
                }
                let sample = ztimeout!(sub.recv_async()).unwrap();
                assert_eq!(sample.source_info().unwrap().source_sn(), 1);
                assert_eq!(sample.payload().try_to_string().unwrap(), "abcdef");
                assert!(sub.try_recv().unwrap().is_none());
                assert!(misses.try_recv().unwrap().is_none());
            }
        }
        ztimeout!(session.close()).unwrap();
    }

    /// Only a complete successor, not another partial, makes a retained
    /// recovery failure terminal. Cover wraparound and late retransmissions.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_later_complete_successor_abandons_failed_sample() {
        let mut config = Config::default();
        config.scouting.multicast.set_enabled(Some(false)).unwrap();
        config.listen.endpoints.set(vec![]).unwrap();
        let session = ztimeout!(zenoh::open(config)).unwrap();
        let source_id = EntityGlobalId::new(session.zid(), 7);
        let key = "test/ext/frag/later_successor";
        for baseline in [0u32, u32::MAX - 1] {
            let partial = baseline.wrapping_add(1);
            let successor = baseline.wrapping_add(2);
            let cache =
                ztimeout!(session.declare_queryable("test/ext/frag/later_successor/@adv/**"))
                    .unwrap();
            let sub = ztimeout!(session.declare_subscriber(key).advanced().recovery(
                RecoveryConfig::default().fragments_recovery_delay(Duration::from_secs(60))
            ))
            .unwrap();
            let misses = ztimeout!(sub.sample_miss_listener()).unwrap();
            ztimeout!(session
                .put(key, "base")
                .source_info(SourceInfo::new(source_id, baseline)))
            .unwrap();
            ztimeout!(sub.recv_async()).unwrap();
            ztimeout!(session
                .put(key, "b")
                .source_info(SourceInfo::new(source_id, partial))
                .frag_info(FragInfo::new(2, 1)))
            .unwrap();
            drop(ztimeout!(cache.recv_async()).unwrap());
            ztimeout!(async {
                loop {
                    if zlock!(sub.statesref)
                        .sequenced_states
                        .peek(&source_id)
                        .unwrap()
                        .pending_samples
                        .get(&WrappingSn(partial))
                        .unwrap()
                        .can_abandon()
                    {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            });
            ztimeout!(session
                .put(key, "c")
                .source_info(SourceInfo::new(source_id, successor))
                .frag_info(FragInfo::new(2, 0)))
            .unwrap();
            {
                let states = zlock!(sub.statesref);
                let state = states.sequenced_states.peek(&source_id).unwrap();
                assert!(state
                    .pending_samples
                    .get(&WrappingSn(partial))
                    .unwrap()
                    .can_abandon());
                assert_eq!(state.pending_samples.len(), 2);
                assert_eq!(state.last_delivered, Some(WrappingSn(baseline)));
            }
            assert!(misses.try_recv().unwrap().is_none());
            ztimeout!(session
                .put(key, "d")
                .source_info(SourceInfo::new(source_id, successor))
                .frag_info(FragInfo::new(2, 1)))
            .unwrap();
            let sample = ztimeout!(sub.recv_async()).unwrap();
            assert_eq!(sample.source_info().unwrap().source_sn(), successor);
            assert_eq!(sample.payload().try_to_string().unwrap(), "cd");
            assert_eq!(ztimeout!(misses.recv_async()).unwrap().nb(), 1);
            ztimeout!(session
                .put(key, "a")
                .source_info(SourceInfo::new(source_id, partial))
                .frag_info(FragInfo::new(2, 0)))
            .unwrap();
            assert!(sub.try_recv().unwrap().is_none());
            assert!(misses.try_recv().unwrap().is_none());
            assert!(cache.try_recv().unwrap().is_none());
        }
        ztimeout!(session.close()).unwrap();
    }

    /// Automatic retries remain exclusive, and a complete successor arriving
    /// during a retry must wait for that attempt's opportunity to succeed.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_automatic_fragment_retry_can_win_against_successor() {
        let mut config = Config::default();
        config.scouting.multicast.set_enabled(Some(false)).unwrap();
        config.listen.endpoints.set(vec![]).unwrap();
        let session = ztimeout!(zenoh::open(config)).unwrap();
        let source_id = EntityGlobalId::new(session.zid(), 7);
        let key = KeyExpr::try_from("test/ext/frag/automatic_retry").unwrap();
        let cache =
            ztimeout!(session.declare_queryable("test/ext/frag/automatic_retry/@adv/**")).unwrap();
        let sub = ztimeout!(session.declare_subscriber(&key).advanced().recovery(
            RecoveryConfig::default().fragments_recovery_delay(Duration::from_millis(50))
        ))
        .unwrap();
        let misses = ztimeout!(sub.sample_miss_listener()).unwrap();
        ztimeout!(session
            .put(&key, "base")
            .source_info(SourceInfo::new(source_id, 0)))
        .unwrap();
        ztimeout!(sub.recv_async()).unwrap();
        ztimeout!(session
            .put(&key, "ef")
            .source_info(SourceInfo::new(source_id, 1))
            .frag_info(FragInfo::new(3, 2)))
        .unwrap();
        drop(ztimeout!(cache.recv_async()).unwrap());
        let retry = ztimeout!(cache.recv_async()).unwrap();
        assert_eq!(retry.parameters().get("_sn"), Some("1..1"));
        assert_eq!(retry.parameters().get("_fn"), Some("0..1"));
        ztimeout!(session
            .put(&key, "next")
            .source_info(SourceInfo::new(source_id, 2)))
        .unwrap();
        for _ in 0..5 {
            ztimeout!(session
                .put(&key, "ef")
                .source_info(SourceInfo::new(source_id, 1))
                .frag_info(FragInfo::new(3, 2)))
            .unwrap();
        }
        assert!(sub.try_recv().unwrap().is_none());
        assert!(cache.try_recv().unwrap().is_none());
        assert_eq!(
            zlock!(sub.statesref)
                .sequenced_states
                .peek(&source_id)
                .unwrap()
                .pending_queries,
            1
        );
        for (num, payload) in [(0, "ab"), (1, "cd")] {
            ztimeout!(retry.reply_sample(
                SampleBuilder::put(key.clone(), payload)
                    .source_info(SourceInfo::new(source_id, 1))
                    .frag_info(FragInfo::new(3, num))
                    .into()
            ))
            .unwrap();
        }
        drop(retry);
        for (sn, payload) in [(1, "abcdef"), (2, "next")] {
            let sample = ztimeout!(sub.recv_async()).unwrap();
            assert_eq!(sample.source_info().unwrap().source_sn(), sn);
            assert_eq!(sample.payload().try_to_string().unwrap(), payload);
        }
        assert!(sub.try_recv().unwrap().is_none());
        assert!(misses.try_recv().unwrap().is_none());
        ztimeout!(session.close()).unwrap();
    }

    /// Do not retry fragment recovery while a sample-recovery query is running.
    /// Both timer ticks and duplicate arrivals must wait for that query to finish.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_fragment_retry_waits_for_sample_recovery_query() {
        let mut config = Config::default();
        config.scouting.multicast.set_enabled(Some(false)).unwrap();
        config.listen.endpoints.set(vec![]).unwrap();
        let session = ztimeout!(zenoh::open(config)).unwrap();
        let source_id = EntityGlobalId::new(session.zid(), 7);
        let key = KeyExpr::try_from("test/ext/frag/retry_overlap").unwrap();
        let cache =
            ztimeout!(session.declare_queryable("test/ext/frag/retry_overlap/@adv/**")).unwrap();
        let delay = Duration::from_millis(200);
        let sub = ztimeout!(session
            .declare_subscriber(&key)
            .advanced()
            .recovery(RecoveryConfig::default().fragments_recovery_delay(delay)))
        .unwrap();
        ztimeout!(session
            .put(&key, "base")
            .source_info(SourceInfo::new(source_id, 0)))
        .unwrap();
        ztimeout!(sub.recv_async()).unwrap();
        ztimeout!(session
            .put(&key, "ef")
            .source_info(SourceInfo::new(source_id, 1))
            .frag_info(FragInfo::new(3, 2)))
        .unwrap();
        drop(ztimeout!(cache.recv_async()).unwrap());
        ztimeout!(async {
            loop {
                if zlock!(sub.statesref)
                    .sequenced_states
                    .peek(&source_id)
                    .unwrap()
                    .pending_samples
                    .get(&WrappingSn(1))
                    .unwrap()
                    .can_abandon()
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        });
        SequencedRepliesHandler::periodic(&sub.statesref, source_id);
        let sample_query = ztimeout!(cache.recv_async()).unwrap();
        assert!(sample_query.parameters().get("_fn").is_none());
        // More than a full cooldown and scan period elapse while this query is held.
        assert!(tokio::time::timeout(delay * 3, cache.recv_async())
            .await
            .is_err());
        for _ in 0..5 {
            ztimeout!(session
                .put(&key, "ef")
                .source_info(SourceInfo::new(source_id, 1))
                .frag_info(FragInfo::new(3, 2)))
            .unwrap();
        }
        assert!(cache.try_recv().unwrap().is_none());
        assert_eq!(
            zlock!(sub.statesref)
                .sequenced_states
                .peek(&source_id)
                .unwrap()
                .pending_queries,
            1
        );
        drop(sample_query);
        let retry = ztimeout!(cache.recv_async()).unwrap();
        assert_eq!(retry.parameters().get("_fn"), Some("0..1"));
        for (num, payload) in [(0, "ab"), (1, "cd")] {
            ztimeout!(retry.reply_sample(
                SampleBuilder::put(key.clone(), payload)
                    .source_info(SourceInfo::new(source_id, 1))
                    .frag_info(FragInfo::new(3, num))
                    .into()
            ))
            .unwrap();
        }
        drop(retry);
        assert_eq!(
            ztimeout!(sub.recv_async())
                .unwrap()
                .payload()
                .try_to_string()
                .unwrap(),
            "abcdef"
        );
        ztimeout!(session.close()).unwrap();
    }

    /// Abandonment needed by a complete successor stays terminal even while
    /// another partial prevents advancing the delivery watermark.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_abandoned_sample_cannot_be_resurrected() {
        let mut config = Config::default();
        config.scouting.multicast.set_enabled(Some(false)).unwrap();
        config.listen.endpoints.set(vec![]).unwrap();
        let session = ztimeout!(zenoh::open(config)).unwrap();
        let source_id = EntityGlobalId::new(session.zid(), 7);
        let key = "test/ext/frag/abandoned";
        let cache =
            ztimeout!(session.declare_queryable("test/ext/frag/abandoned/@adv/**")).unwrap();
        let sub = ztimeout!(session
            .declare_subscriber(key)
            .advanced()
            .recovery(RecoveryConfig::default().fragments_recovery_delay(Duration::from_secs(10))))
        .unwrap();
        let misses = ztimeout!(sub.sample_miss_listener()).unwrap();
        ztimeout!(session
            .put(key, "base")
            .source_info(SourceInfo::new(source_id, 0)))
        .unwrap();
        ztimeout!(sub.recv_async()).unwrap();
        ztimeout!(session
            .put(key, "b")
            .source_info(SourceInfo::new(source_id, 1))
            .frag_info(FragInfo::new(2, 1)))
        .unwrap();
        ztimeout!(session
            .put(key, "c")
            .source_info(SourceInfo::new(source_id, 2))
            .frag_info(FragInfo::new(2, 0)))
        .unwrap();
        ztimeout!(session
            .put(key, "next")
            .source_info(SourceInfo::new(source_id, 3)))
        .unwrap();
        drop(ztimeout!(cache.recv_async()).unwrap());
        ztimeout!(async {
            loop {
                let abandoned = zlock!(sub.statesref)
                    .sequenced_states
                    .peek(&source_id)
                    .unwrap()
                    .pending_samples
                    .get(&WrappingSn(1))
                    .is_some_and(FragmentedSample::is_abandoned);
                if abandoned {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        });
        for num in 0..2 {
            ztimeout!(session
                .put(key, "late")
                .source_info(SourceInfo::new(source_id, 1))
                .frag_info(FragInfo::new(2, num)))
            .unwrap();
        }
        ztimeout!(session
            .put(key, "whole late sample")
            .source_info(SourceInfo::new(source_id, 1)))
        .unwrap();
        tokio::time::sleep(Duration::from_millis(80)).await;
        assert!(cache.try_recv().unwrap().is_none());
        assert!(sub.try_recv().unwrap().is_none());
        assert!(misses.try_recv().unwrap().is_none());
        drop(cache);
        ztimeout!(session
            .put(key, "d")
            .source_info(SourceInfo::new(source_id, 2))
            .frag_info(FragInfo::new(2, 1)))
        .unwrap();
        assert_eq!(
            ztimeout!(sub.recv_async())
                .unwrap()
                .source_info()
                .unwrap()
                .source_sn(),
            2
        );
        assert_eq!(ztimeout!(misses.recv_async()).unwrap().nb(), 1);
        assert_eq!(
            ztimeout!(sub.recv_async())
                .unwrap()
                .source_info()
                .unwrap()
                .source_sn(),
            3
        );
        ztimeout!(session.close()).unwrap();
    }

    /// Successful hole recovery must neither abandon an unrequested tail nor
    /// let the buffered successor bypass a still-recoverable sample.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_successful_hole_recovery_preserves_active_tail() {
        let mut config = Config::default();
        config.scouting.multicast.set_enabled(Some(false)).unwrap();
        config.listen.endpoints.set(vec![]).unwrap();
        let session = ztimeout!(zenoh::open(config)).unwrap();
        let source_id = EntityGlobalId::new(session.zid(), 7);
        let key = KeyExpr::try_from("test/ext/frag/hole_tail").unwrap();
        let cache =
            ztimeout!(session.declare_queryable("test/ext/frag/hole_tail/@adv/**")).unwrap();
        let sub = ztimeout!(session
            .declare_subscriber(&key)
            .advanced()
            .recovery(RecoveryConfig::default().fragments_recovery_delay(Duration::from_secs(10))))
        .unwrap();
        let misses = ztimeout!(sub.sample_miss_listener()).unwrap();
        ztimeout!(session
            .put(&key, "base")
            .source_info(SourceInfo::new(source_id, 0)))
        .unwrap();
        ztimeout!(sub.recv_async()).unwrap();
        for (num, payload) in [(0, "a"), (2, "c")] {
            ztimeout!(session
                .put(&key, payload)
                .source_info(SourceInfo::new(source_id, 1))
                .frag_info(FragInfo::new(4, num)))
            .unwrap();
        }
        ztimeout!(session
            .put(&key, "next")
            .source_info(SourceInfo::new(source_id, 2)))
        .unwrap();
        let query = ztimeout!(cache.recv_async()).unwrap();
        assert_eq!(query.parameters().get("_fn"), Some("1..1"));
        ztimeout!(query.reply_sample(
            SampleBuilder::put(key.clone(), "b")
                .source_info(SourceInfo::new(source_id, 1))
                .frag_info(FragInfo::new(4, 1))
                .into()
        ))
        .unwrap();
        drop(query);
        ztimeout!(async {
            loop {
                if zlock!(sub.statesref)
                    .sequenced_states
                    .peek(&source_id)
                    .unwrap()
                    .pending_queries
                    == 0
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        });
        assert!(sub.try_recv().unwrap().is_none());
        assert!(misses.try_recv().unwrap().is_none());
        ztimeout!(session
            .put(&key, "d")
            .source_info(SourceInfo::new(source_id, 1))
            .frag_info(FragInfo::new(4, 3)))
        .unwrap();
        for (sn, payload) in [(1, "abcd"), (2, "next")] {
            let sample = ztimeout!(sub.recv_async()).unwrap();
            assert_eq!(sample.source_info().unwrap().source_sn(), sn);
            assert_eq!(sample.payload().try_to_string().unwrap(), payload);
        }
        assert!(misses.try_recv().unwrap().is_none());
        assert!(cache.try_recv().unwrap().is_none());
        ztimeout!(session.close()).unwrap();
    }

    /// All disjoint range queries belong to one attempt. An empty first range
    /// cannot finalize it while another range query is still outstanding.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_fragment_recovery_waits_for_all_requested_ranges() {
        let mut config = Config::default();
        config.scouting.multicast.set_enabled(Some(false)).unwrap();
        config.listen.endpoints.set(vec![]).unwrap();
        let session = ztimeout!(zenoh::open(config)).unwrap();
        let source_id = EntityGlobalId::new(session.zid(), 7);
        let key = KeyExpr::try_from("test/ext/frag/ranges").unwrap();
        let cache = ztimeout!(session.declare_queryable("test/ext/frag/ranges/@adv/**")).unwrap();
        let received = Arc::new(Mutex::new(Vec::new()));
        let statesref = frag_recovery_state(&session, &key, received.clone());
        {
            let mut states = zlock!(statesref);
            handle_sample(
                &mut states,
                SampleBuilder::put(key.clone(), "base")
                    .source_info(SourceInfo::new(source_id, 0))
                    .into(),
            );
            // Seed both holes before arming the scan so it queries them in one attempt.
            for (num, payload) in [(0, "a"), (2, "c"), (4, "e")] {
                handle_sample(
                    &mut states,
                    SampleBuilder::put(key.clone(), payload)
                        .source_info(SourceInfo::new(source_id, 1))
                        .frag_info(FragInfo::new(5, num))
                        .into(),
                );
            }
            handle_sample(
                &mut states,
                SampleBuilder::put(key.clone(), "next")
                    .source_info(SourceInfo::new(source_id, 2))
                    .into(),
            );
            states
                .sequenced_states
                .peek_mut(&source_id)
                .unwrap()
                .arm_fragment_recovery(&statesref, source_id, Duration::from_millis(20));
        }
        let first = ztimeout!(cache.recv_async()).unwrap();
        let second = ztimeout!(cache.recv_async()).unwrap();
        assert_eq!(first.parameters().get("_fn"), Some("1..1"));
        assert_eq!(second.parameters().get("_fn"), Some("3..3"));
        drop(first);
        tokio::time::sleep(Duration::from_millis(80)).await;
        let (pending_queries, incomplete) = {
            let states = zlock!(statesref);
            let state = states.sequenced_states.peek(&source_id).unwrap();
            (
                state.pending_queries,
                state
                    .pending_samples
                    .get(&WrappingSn(1))
                    .unwrap()
                    .is_incomplete(),
            )
        };
        assert_eq!(pending_queries, 1);
        assert!(incomplete);
        assert_eq!(received.lock().unwrap().len(), 1);
        assert!(cache.try_recv().unwrap().is_none());
        ztimeout!(second.reply_sample(
            SampleBuilder::put(key, "d")
                .source_info(SourceInfo::new(source_id, 1))
                .frag_info(FragInfo::new(5, 3))
                .into()
        ))
        .unwrap();
        drop(second);
        ztimeout!(async {
            loop {
                if received.lock().unwrap().len() == 2 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        });
        assert_eq!(
            received.lock().unwrap()[1]
                .source_info()
                .unwrap()
                .source_sn(),
            2
        );
        ztimeout!(session.close()).unwrap();
    }

    /// Build a reassembly state with no retransmission and the given
    /// `max_pending_samples`, collecting deliveries and misses.
    fn pending_bound_state(
        session: &Session,
        max_pending_samples: usize,
        received: Arc<Mutex<Vec<Sample>>>,
        misses: Arc<Mutex<Vec<u32>>>,
    ) -> Arc<Mutex<State>> {
        Arc::new(Mutex::new(State {
            next_id: 0,
            global_pending_queries: 0,
            sequenced_states: LruCache::unbounded(),
            timestamped_states: LruCache::unbounded(),
            session: session.downgrade(),
            key_expr: KeyExpr::try_from("test/ext/pending").unwrap(),
            retransmission: false,
            frag_recovery_delay: RecoveryConfig::<true>::default().frag_recovery_delay,
            period: None,
            max_pending_samples,
            query_target: QueryTarget::All,
            query_timeout: Duration::from_secs(10),
            max_fragments: MAX_FRAGMENTS_DEFAULT,
            callback: Some(Callback::from(move |s: Sample| {
                received.lock().unwrap().push(s);
            })),
            miss_handlers: HashMap::from([(
                0,
                Callback::from(move |m: Miss| misses.lock().unwrap().push(m.nb())),
            )]),
            token: None,
            _gc_task: AbortOnDropHandle::new(ZRuntime::Application.spawn(std::future::pending())),
        }))
    }

    /// An abandoned first sample must not prevent a later fragmented sample
    /// from establishing the first delivery baseline.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_abandoned_first_sample_allows_fragmented_successor() {
        let mut config = Config::default();
        config.scouting.multicast.set_enabled(Some(false)).unwrap();
        config.listen.endpoints.set(vec![]).unwrap();
        let session = ztimeout!(zenoh::open(config)).unwrap();
        let source_id = EntityGlobalId::new(session.zid(), 7);
        let key = "test/ext/frag/first_abandoned";
        let cache =
            ztimeout!(session.declare_queryable("test/ext/frag/first_abandoned/@adv/**")).unwrap();
        let sub =
            ztimeout!(session.declare_subscriber(key).advanced().recovery(
                RecoveryConfig::default().fragments_recovery_delay(Duration::from_secs(10)),
            ))
            .unwrap();
        let misses = ztimeout!(sub.sample_miss_listener()).unwrap();
        ztimeout!(session
            .put(key, "b")
            .source_info(SourceInfo::new(source_id, 0))
            .frag_info(FragInfo::new(2, 1)))
        .unwrap();
        drop(ztimeout!(cache.recv_async()).unwrap());
        ztimeout!(async {
            loop {
                let abandoned = zlock!(sub.statesref)
                    .sequenced_states
                    .peek(&source_id)
                    .unwrap()
                    .pending_samples
                    .get(&WrappingSn(0))
                    .is_some_and(FragmentedSample::can_abandon);
                if abandoned {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        });
        ztimeout!(session
            .put(key, "c")
            .source_info(SourceInfo::new(source_id, 1))
            .frag_info(FragInfo::new(2, 0)))
        .unwrap();
        // Before the first delivery, a complete unfragmented successor must
        // still wait for this recoverable partial sample.
        ztimeout!(session
            .put(key, "next")
            .source_info(SourceInfo::new(source_id, 2)))
        .unwrap();
        assert!(sub.try_recv().unwrap().is_none());
        ztimeout!(session
            .put(key, "d")
            .source_info(SourceInfo::new(source_id, 1))
            .frag_info(FragInfo::new(2, 1)))
        .unwrap();
        let sample = tokio::time::timeout(Duration::from_secs(2), sub.recv_async())
            .await
            .expect("abandoned first sample blocked a complete successor")
            .unwrap();
        assert_eq!(sample.source_info().unwrap().source_sn(), 1);
        assert_eq!(sample.payload().try_to_string().unwrap(), "cd");
        assert_eq!(
            ztimeout!(sub.recv_async())
                .unwrap()
                .source_info()
                .unwrap()
                .source_sn(),
            2
        );
        assert!(misses.try_recv().unwrap().is_none());
        assert!(cache.try_recv().unwrap().is_none());
        ztimeout!(session.close()).unwrap();
    }

    /// Failed recovery of sample 1 must not discard sample 2, whose remaining
    /// fragments are still arriving, just because sample 3 is complete. It must
    /// not skip missing sequence numbers that still need a sample-recovery query.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_fragment_failure_preserves_other_partial_sample() {
        let mut config = Config::default();
        config.scouting.multicast.set_enabled(Some(false)).unwrap();
        config.listen.endpoints.set(vec![]).unwrap();
        let session = ztimeout!(zenoh::open(config)).unwrap();
        let source_id = EntityGlobalId::new(session.zid(), 7);
        let key = "test/ext/frag/failure_with_active_tail";
        let cache =
            ztimeout!(session.declare_queryable("test/ext/frag/failure_with_active_tail/@adv/**"))
                .unwrap();
        let sub =
            ztimeout!(session.declare_subscriber(key).advanced().recovery(
                RecoveryConfig::default().fragments_recovery_delay(Duration::from_secs(10)),
            ))
            .unwrap();
        let misses = ztimeout!(sub.sample_miss_listener()).unwrap();
        ztimeout!(session
            .put(key, "base")
            .source_info(SourceInfo::new(source_id, 0)))
        .unwrap();
        ztimeout!(sub.recv_async()).unwrap();
        ztimeout!(session
            .put(key, "b")
            .source_info(SourceInfo::new(source_id, 1))
            .frag_info(FragInfo::new(2, 1)))
        .unwrap();
        let failed_query = ztimeout!(cache.recv_async()).unwrap();
        ztimeout!(session
            .put(key, "c")
            .source_info(SourceInfo::new(source_id, 2))
            .frag_info(FragInfo::new(2, 0)))
        .unwrap();
        ztimeout!(session
            .put(key, "next")
            .source_info(SourceInfo::new(source_id, 3)))
        .unwrap();
        drop(failed_query);
        ztimeout!(async {
            loop {
                if zlock!(sub.statesref)
                    .sequenced_states
                    .peek(&source_id)
                    .unwrap()
                    .pending_queries
                    == 0
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        });
        let (last_delivered, pending) = {
            let states = zlock!(sub.statesref);
            let state = states.sequenced_states.peek(&source_id).unwrap();
            (
                state.last_delivered,
                state
                    .pending_samples
                    .iter()
                    .map(|(sn, sample)| (*sn, sample.is_abandoned(), sample.is_complete()))
                    .collect::<Vec<_>>(),
            )
        };
        assert_eq!(last_delivered, Some(WrappingSn(0)));
        assert_eq!(
            pending,
            [
                (WrappingSn(1), true, false),
                (WrappingSn(2), false, false),
                (WrappingSn(3), false, true)
            ]
        );
        // Exercise the scheduled gap check directly as well: abandoned SN 1
        // must not make the still-pending SN 2 look like a sequence-number gap.
        SequencedRepliesHandler::recover_gap(&sub.statesref, source_id);
        assert!(cache.try_recv().unwrap().is_none());
        assert!(sub.try_recv().unwrap().is_none());
        assert!(misses.try_recv().unwrap().is_none());
        ztimeout!(session
            .put(key, "d")
            .source_info(SourceInfo::new(source_id, 2))
            .frag_info(FragInfo::new(2, 1)))
        .unwrap();
        for (sn, payload) in [(2, "cd"), (3, "next")] {
            let sample = ztimeout!(sub.recv_async()).unwrap();
            assert_eq!(sample.source_info().unwrap().source_sn(), sn);
            assert_eq!(sample.payload().try_to_string().unwrap(), payload);
        }
        assert_eq!(ztimeout!(misses.recv_async()).unwrap().nb(), 1);
        assert!(misses.try_recv().unwrap().is_none());
        ztimeout!(session.close()).unwrap();
    }

    /// Skipping a known abandonment must not hide an actual missing sequence
    /// number before a complete successor.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_fragment_failure_recovers_unknown_gap_after_abandoned_prefix() {
        let mut config = Config::default();
        config.scouting.multicast.set_enabled(Some(false)).unwrap();
        config.listen.endpoints.set(vec![]).unwrap();
        let session = ztimeout!(zenoh::open(config)).unwrap();
        let source_id = EntityGlobalId::new(session.zid(), 7);
        let key = KeyExpr::try_from("test/ext/frag/abandoned_then_gap").unwrap();
        let cache =
            ztimeout!(session.declare_queryable("test/ext/frag/abandoned_then_gap/@adv/**"))
                .unwrap();
        let sub =
            ztimeout!(session.declare_subscriber(&key).advanced().recovery(
                RecoveryConfig::default().fragments_recovery_delay(Duration::from_secs(10)),
            ))
            .unwrap();
        let misses = ztimeout!(sub.sample_miss_listener()).unwrap();
        ztimeout!(session
            .put(&key, "base")
            .source_info(SourceInfo::new(source_id, 0)))
        .unwrap();
        ztimeout!(sub.recv_async()).unwrap();
        ztimeout!(session
            .put(&key, "b")
            .source_info(SourceInfo::new(source_id, 1))
            .frag_info(FragInfo::new(2, 1)))
        .unwrap();
        let failed_query = ztimeout!(cache.recv_async()).unwrap();
        ztimeout!(session
            .put(&key, "three")
            .source_info(SourceInfo::new(source_id, 3)))
        .unwrap();
        drop(failed_query);
        let gap_query = ztimeout!(cache.recv_async()).unwrap();
        assert_eq!(gap_query.parameters().get("_sn"), Some("2.."));
        assert_eq!(gap_query.parameters().get("_fn"), None);
        assert!(sub.try_recv().unwrap().is_none());
        ztimeout!(gap_query.reply_sample(
            SampleBuilder::put(key, "two")
                .source_info(SourceInfo::new(source_id, 2))
                .into()
        ))
        .unwrap();
        drop(gap_query);
        for (sn, payload) in [(2, "two"), (3, "three")] {
            let sample = ztimeout!(sub.recv_async()).unwrap();
            assert_eq!(sample.source_info().unwrap().source_sn(), sn);
            assert_eq!(sample.payload().try_to_string().unwrap(), payload);
        }
        assert_eq!(ztimeout!(misses.recv_async()).unwrap().nb(), 1);
        assert!(misses.try_recv().unwrap().is_none());
        assert!(cache.try_recv().unwrap().is_none());
        ztimeout!(session.close()).unwrap();
    }

    /// A retired eviction watermark must not reject a valid next sample once
    /// the sequence counter reaches its old half-range boundary.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_delivery_retires_eviction_watermark() {
        let mut config = Config::default();
        config.scouting.multicast.set_enabled(Some(false)).unwrap();
        config.listen.endpoints.set(vec![]).unwrap();
        let session = ztimeout!(zenoh::open(config)).unwrap();
        let source_id = EntityGlobalId::new(session.zid(), 7);
        let key = KeyExpr::try_from("test/ext/pending").unwrap();
        for baseline in [0u32, u32::MAX - 1] {
            let received = Arc::new(Mutex::new(Vec::new()));
            let misses = Arc::new(Mutex::new(Vec::new()));
            let statesref = pending_bound_state(&session, 1, received.clone(), misses);
            let evicted = baseline.wrapping_add(1);
            let successor = baseline.wrapping_add(2);
            let half_range = evicted.wrapping_add(u32::MAX / 2);
            let (watermark, last_delivered) = {
                let mut states = zlock!(statesref);
                handle_sample(
                    &mut states,
                    SampleBuilder::put(key.clone(), "base")
                        .source_info(SourceInfo::new(source_id, baseline))
                        .into(),
                );
                for sn in [evicted, successor] {
                    handle_sample(
                        &mut states,
                        SampleBuilder::put(key.clone(), "a")
                            .source_info(SourceInfo::new(source_id, sn))
                            .frag_info(FragInfo::new(2, 0))
                            .into(),
                    );
                }
                handle_sample(
                    &mut states,
                    SampleBuilder::put(key.clone(), "b")
                        .source_info(SourceInfo::new(source_id, successor))
                        .frag_info(FragInfo::new(2, 1))
                        .into(),
                );
                let watermark = states
                    .sequenced_states
                    .peek(&source_id)
                    .unwrap()
                    .last_evicted;
                for sn in [half_range, half_range.wrapping_add(1)] {
                    handle_sample(
                        &mut states,
                        SampleBuilder::put(key.clone(), "new")
                            .source_info(SourceInfo::new(source_id, sn))
                            .into(),
                    );
                }
                (
                    watermark,
                    states
                        .sequenced_states
                        .peek(&source_id)
                        .unwrap()
                        .last_delivered,
                )
            };
            assert_eq!(last_delivered, Some(WrappingSn(half_range.wrapping_add(1))));
            assert_eq!(watermark, None, "delivery must retire last_evicted");
            let delivered: Vec<_> = received
                .lock()
                .unwrap()
                .iter()
                .map(|sample| sample.source_info().unwrap().source_sn())
                .collect();
            assert_eq!(
                delivered,
                [baseline, successor, half_range, half_range.wrapping_add(1)]
            );
        }
        ztimeout!(session.close()).unwrap();
    }

    /// Bounded buffering must not let evicted abandonment tombstones be
    /// recreated by late data before the delivery watermark advances.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_evicted_abandoned_sample_stays_obsolete() {
        let mut config = Config::default();
        config.scouting.multicast.set_enabled(Some(false)).unwrap();
        config.listen.endpoints.set(vec![]).unwrap();
        let session = ztimeout!(zenoh::open(config)).unwrap();
        let source_id = EntityGlobalId::new(session.zid(), 7);
        let key = KeyExpr::try_from("test/ext/pending").unwrap();
        let received = Arc::new(Mutex::new(Vec::new()));
        let misses = Arc::new(Mutex::new(Vec::new()));
        let statesref = pending_bound_state(&session, 2, received.clone(), misses.clone());
        let (last_evicted, pending) = {
            let mut states = zlock!(statesref);
            handle_sample(
                &mut states,
                SampleBuilder::put(key.clone(), "base")
                    .source_info(SourceInfo::new(source_id, 0))
                    .into(),
            );
            states
                .sequenced_states
                .peek_mut(&source_id)
                .unwrap()
                .pending_samples
                .insert(WrappingSn(1), FragmentedSample::Abandoned);
            for sn in [3, 4, 1] {
                handle_sample(
                    &mut states,
                    SampleBuilder::put(key.clone(), "a")
                        .source_info(SourceInfo::new(source_id, sn))
                        .frag_info(FragInfo::new(2, 0))
                        .into(),
                );
            }
            handle_sample(
                &mut states,
                SampleBuilder::put(key, "late whole sample")
                    .source_info(SourceInfo::new(source_id, 1))
                    .into(),
            );
            let state = states.sequenced_states.peek(&source_id).unwrap();
            (
                state.last_evicted,
                state.pending_samples.keys().copied().collect::<Vec<_>>(),
            )
        };
        assert_eq!(last_evicted, Some(WrappingSn(1)));
        assert_eq!(pending, [WrappingSn(3), WrappingSn(4)]);
        assert_eq!(received.lock().unwrap().len(), 1);
        assert!(misses.lock().unwrap().is_empty());
        ztimeout!(session.close()).unwrap();
    }

    /// Completing an old attempt after source GC must not decrement a new
    /// source state's counters or abandon its assembly at the same SN.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_fragment_recovery_ignores_recreated_source() {
        let mut config = Config::default();
        config.scouting.multicast.set_enabled(Some(false)).unwrap();
        config.listen.endpoints.set(vec![]).unwrap();
        let session = ztimeout!(zenoh::open(config)).unwrap();
        let source_id = EntityGlobalId::new(session.zid(), 7);
        let key = KeyExpr::try_from("test/ext/pending").unwrap();
        let received = Arc::new(Mutex::new(Vec::new()));
        let statesref = frag_recovery_state(&session, &key, received);
        let handler = {
            let mut states = zlock!(statesref);
            handle_sample(
                &mut states,
                SampleBuilder::put(key.clone(), "a")
                    .source_info(SourceInfo::new(source_id, 1))
                    .frag_info(FragInfo::new(2, 0))
                    .into(),
            );
            let state = states.sequenced_states.peek_mut(&source_id).unwrap();
            let attempt = state
                .pending_samples
                .get_mut(&WrappingSn(1))
                .unwrap()
                .prepare_recovery(Duration::ZERO, true)
                .unwrap()
                .1;
            state.pending_queries = 1;
            FragmentAttempt {
                source_id,
                statesref: statesref.clone(),
                generation: state.generation.clone(),
                sn: WrappingSn(1),
                ranges: vec![(Some(1), None)],
                attempt,
            }
        };
        {
            let mut states = zlock!(statesref);
            states.sequenced_states.pop(&source_id);
            handle_sample(
                &mut states,
                SampleBuilder::put(key, "new")
                    .source_info(SourceInfo::new(source_id, 1))
                    .frag_info(FragInfo::new(2, 0))
                    .into(),
            );
            states
                .sequenced_states
                .peek_mut(&source_id)
                .unwrap()
                .pending_queries = 5;
        }
        drop(handler);
        let (pending_queries, incomplete) = {
            let states = zlock!(statesref);
            let state = states.sequenced_states.peek(&source_id).unwrap();
            (
                state.pending_queries,
                state
                    .pending_samples
                    .get(&WrappingSn(1))
                    .unwrap()
                    .is_incomplete(),
            )
        };
        assert_eq!(pending_queries, 5);
        assert!(incomplete);
        ztimeout!(session.close()).unwrap();
    }

    /// A history or sample-recovery query must keep samples with missing fragments.
    /// Delivery resumes once those fragments arrive or recovery gives up.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_query_flush_preserves_recoverable_partials() {
        let mut config = Config::default();
        config.scouting.multicast.set_enabled(Some(false)).unwrap();
        config.listen.endpoints.set(vec![]).unwrap();
        let session = ztimeout!(zenoh::open(config)).unwrap();
        let source_id = EntityGlobalId::new(session.zid(), 7);
        let key_expr = KeyExpr::try_from("test/ext/pending").unwrap();
        let cache = ztimeout!(session.declare_queryable("test/ext/pending/@adv/**")).unwrap();
        for history in [false, true] {
            for baseline in [0u32, u32::MAX - 2] {
                for gap in [false, true] {
                    for outcome in ["live", "reply", "empty", "timeout"] {
                        let received = Arc::new(Mutex::new(Vec::<Sample>::new()));
                        let misses = Arc::new(Mutex::new(Vec::<u32>::new()));
                        let statesref =
                            pending_bound_state(&session, 10, received.clone(), misses.clone());
                        let partial = baseline.wrapping_add(1);
                        let successor = baseline.wrapping_add(if gap { 3 } else { 2 });
                        let frag = |num| -> Sample {
                            SampleBuilder::put(key_expr.clone(), if num == 0 { "ab" } else { "cd" })
                                .source_info(SourceInfo::new(source_id, partial))
                                .frag_info(FragInfo::new(2, num))
                                .into()
                        };
                        {
                            let mut states = zlock!(statesref);
                            states.retransmission = true;
                            states.frag_recovery_delay = Duration::from_millis(10);
                            states.query_timeout = Duration::from_millis(100);
                            handle_sample(
                                &mut states,
                                SampleBuilder::put(key_expr.clone(), "base")
                                    .source_info(SourceInfo::new(source_id, baseline))
                                    .into(),
                            );
                            states.global_pending_queries = u64::from(history);
                            handle_sample(&mut states, frag(0));
                            handle_sample(
                                &mut states,
                                SampleBuilder::put(key_expr.clone(), "next")
                                    .source_info(SourceInfo::new(source_id, successor))
                                    .into(),
                            );
                            states
                                .sequenced_states
                                .peek_mut(&source_id)
                                .unwrap()
                                .pending_queries = u64::from(!history);
                        }
                        if history {
                            drop(InitialRepliesHandler {
                                statesref: statesref.clone(),
                            });
                        } else {
                            drop(SequencedRepliesHandler {
                                source_id,
                                statesref: statesref.clone(),
                            });
                        }
                        {
                            let mut states = zlock!(statesref);
                            let state = states.sequenced_states.peek(&source_id).unwrap();
                            assert_eq!(state.last_delivered, Some(WrappingSn(baseline)));
                            assert_eq!(state.pending_samples.len(), 2);
                            assert_eq!(state.gap_skip_through, Some(WrappingSn(successor)));
                            if outcome == "live" {
                                // Resolve before the scan needs to issue any query.
                                handle_sample(&mut states, frag(1));
                            }
                        }
                        if outcome != "live" {
                            let query = ztimeout!(cache.recv_async()).unwrap();
                            let range = format!("{partial}..{partial}");
                            assert_eq!(query.parameters().get("_sn"), Some(range.as_str()));
                            assert_eq!(query.parameters().get("_fn"), Some("1.."));
                            if outcome == "reply" {
                                ztimeout!(query.reply_sample(frag(1))).unwrap();
                            }
                            if outcome == "timeout" {
                                ztimeout!(async {
                                    loop {
                                        if received.lock().unwrap().len() == 2 {
                                            break;
                                        }
                                        tokio::time::sleep(Duration::from_millis(5)).await;
                                    }
                                });
                            }
                            drop(query);
                        }
                        ztimeout!(async {
                            loop {
                                if zlock!(statesref)
                                    .sequenced_states
                                    .peek(&source_id)
                                    .unwrap()
                                    .last_delivered
                                    == Some(WrappingSn(successor))
                                {
                                    break;
                                }
                                tokio::time::sleep(Duration::from_millis(5)).await;
                            }
                        });
                        let recovered = outcome == "live" || outcome == "reply";
                        let samples = received.lock().unwrap();
                        let delivered: Vec<_> = samples
                            .iter()
                            .map(|s| s.source_info().unwrap().source_sn())
                            .collect();
                        assert_eq!(
                            delivered,
                            if recovered {
                                vec![baseline, partial, successor]
                            } else {
                                vec![baseline, successor]
                            }
                        );
                        if recovered {
                            assert_eq!(samples[1].payload().try_to_string().unwrap(), "abcd");
                        }
                        let missed = u32::from(gap) + u32::from(!recovered);
                        assert_eq!(
                            *misses.lock().unwrap(),
                            if missed == 0 { vec![] } else { vec![missed] }
                        );
                        let states = zlock!(statesref);
                        let state = states.sequenced_states.peek(&source_id).unwrap();
                        assert!(state.pending_samples.is_empty());
                        assert!(state.gap_skip_through.is_none());
                    }
                }
            }
        }
        ztimeout!(session.close()).unwrap();
    }

    /// Missing fragments in history or sample-recovery replies must trigger
    /// fragment recovery, even when a newer complete sample is buffered.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_query_only_partial_before_complete_successor_is_recovered() {
        let mut config = Config::default();
        config.scouting.multicast.set_enabled(Some(false)).unwrap();
        config.listen.endpoints.set(vec![]).unwrap();
        let session = ztimeout!(zenoh::open(config)).unwrap();
        let source_id = EntityGlobalId::new(session.zid(), 7);
        let key_expr = KeyExpr::try_from("test/ext/frag/query_barrier").unwrap();
        for history in [false, true] {
            let cache = ztimeout!(session.declare_queryable("test/ext/frag/query_barrier/@adv/**"))
                .unwrap();
            let builder = session.declare_subscriber(&key_expr).advanced().recovery(
                RecoveryConfig::default().fragments_recovery_delay(Duration::from_millis(50)),
            );
            let sub = ztimeout!(if history {
                builder.history(HistoryConfig::default())
            } else {
                builder
            })
            .unwrap();
            let misses = ztimeout!(sub.sample_miss_listener()).unwrap();
            if !history {
                ztimeout!(session
                    .put(&key_expr, "base")
                    .source_info(SourceInfo::new(source_id, 0)))
                .unwrap();
                assert_eq!(
                    ztimeout!(sub.recv_async())
                        .unwrap()
                        .source_info()
                        .unwrap()
                        .source_sn(),
                    0
                );
                ztimeout!(session
                    .put(&key_expr, "next")
                    .source_info(SourceInfo::new(source_id, 2)))
                .unwrap();
            }
            let query = ztimeout!(cache.recv_async()).unwrap();
            ztimeout!(query.reply_sample(
                SampleBuilder::put(key_expr.clone(), "ab")
                    .source_info(SourceInfo::new(source_id, 1))
                    .frag_info(FragInfo::new(2, 0))
                    .into(),
            ))
            .unwrap();
            if history {
                ztimeout!(query.reply_sample(
                    SampleBuilder::put(key_expr.clone(), "next")
                        .source_info(SourceInfo::new(source_id, 2))
                        .into(),
                ))
                .unwrap();
            }
            drop(query);
            let recovery = ztimeout!(cache.recv_async()).unwrap();
            assert_eq!(recovery.parameters().get("_sn"), Some("1..1"));
            assert_eq!(recovery.parameters().get("_fn"), Some("1.."));
            assert!(sub.try_recv().unwrap().is_none());
            assert!(misses.try_recv().unwrap().is_none());
            ztimeout!(recovery.reply_sample(
                SampleBuilder::put(key_expr.clone(), "cd")
                    .source_info(SourceInfo::new(source_id, 1))
                    .frag_info(FragInfo::new(2, 1))
                    .into(),
            ))
            .unwrap();
            drop(recovery);
            for (sn, payload) in [(1, "abcd"), (2, "next")] {
                let sample = ztimeout!(sub.recv_async()).unwrap();
                assert_eq!(sample.source_info().unwrap().source_sn(), sn);
                assert_eq!(sample.payload().try_to_string().unwrap(), payload);
            }
            assert!(sub.try_recv().unwrap().is_none());
            assert!(misses.try_recv().unwrap().is_none());
        }
        ztimeout!(session.close()).unwrap();
    }

    /// A query finishes with sample 3 buffered, so delivery may skip missing
    /// sample 2 after sample 1's fragments arrive. Receiving sample 5 later
    /// must not also cause missing sample 4 to be skipped.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_deferred_flush_does_not_skip_future_gap() {
        let mut config = Config::default();
        config.scouting.multicast.set_enabled(Some(false)).unwrap();
        config.listen.endpoints.set(vec![]).unwrap();
        let session = ztimeout!(zenoh::open(config)).unwrap();
        let source_id = EntityGlobalId::new(session.zid(), 7);
        let received = Arc::new(Mutex::new(Vec::<Sample>::new()));
        let misses = Arc::new(Mutex::new(Vec::<u32>::new()));
        let statesref = pending_bound_state(&session, 10, received.clone(), misses.clone());
        let frag = |num| -> Sample {
            SampleBuilder::put(KeyExpr::try_from("test/ext/pending").unwrap(), "ab")
                .source_info(SourceInfo::new(source_id, 1))
                .frag_info(FragInfo::new(2, num))
                .into()
        };
        let single = |sn| -> Sample {
            SampleBuilder::put(KeyExpr::try_from("test/ext/pending").unwrap(), "single")
                .source_info(SourceInfo::new(source_id, sn))
                .into()
        };
        {
            let mut states = zlock!(statesref);
            states.retransmission = true;
            states.frag_recovery_delay = Duration::from_secs(60);
            handle_sample(&mut states, single(0));
            handle_sample(&mut states, frag(0));
            handle_sample(&mut states, single(3));
            states.on_history_or_sample_recovery_finished(&statesref, &source_id);
            handle_sample(&mut states, single(5));
            handle_sample(&mut states, frag(1));
            let state = states.sequenced_states.peek(&source_id).unwrap();
            assert_eq!(state.last_delivered, Some(WrappingSn(3)));
            assert!(state.pending_samples.contains_key(&WrappingSn(5)));
            assert!(state.gap_skip_through.is_none());
        }
        assert_eq!(*misses.lock().unwrap(), [1]);
        assert_eq!(received.lock().unwrap().len(), 3);
        let cache = ztimeout!(session.declare_queryable("test/ext/pending/@adv/**")).unwrap();
        SequencedRepliesHandler::recover_gap(&statesref, source_id);
        let query = ztimeout!(cache.recv_async()).unwrap();
        assert_eq!(query.parameters().get("_sn"), Some("4.."));
        drop(query);
        ztimeout!(async {
            loop {
                if received.lock().unwrap().len() == 4 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        });
        assert_eq!(*misses.lock().unwrap(), [1, 1]);
        ztimeout!(session.close()).unwrap();
    }

    /// Resolving the first barrier must preserve a second recoverable partial;
    /// eviction must also resume a deferred flush across an absent SN.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_deferred_flush_multiple_barriers_and_eviction() {
        let mut config = Config::default();
        config.scouting.multicast.set_enabled(Some(false)).unwrap();
        config.listen.endpoints.set(vec![]).unwrap();
        let session = ztimeout!(zenoh::open(config)).unwrap();
        let source_id = EntityGlobalId::new(session.zid(), 7);
        let single = |sn| -> Sample {
            SampleBuilder::put(KeyExpr::try_from("test/ext/pending").unwrap(), "single")
                .source_info(SourceInfo::new(source_id, sn))
                .into()
        };
        let frag = |sn, num| -> Sample {
            SampleBuilder::put(KeyExpr::try_from("test/ext/pending").unwrap(), "ab")
                .source_info(SourceInfo::new(source_id, sn))
                .frag_info(FragInfo::new(2, num))
                .into()
        };
        for evict in [false, true] {
            let received = Arc::new(Mutex::new(Vec::<Sample>::new()));
            let misses = Arc::new(Mutex::new(Vec::<u32>::new()));
            let statesref = pending_bound_state(
                &session,
                if evict { 2 } else { 10 },
                received.clone(),
                misses.clone(),
            );
            {
                let mut states = zlock!(statesref);
                states.retransmission = true;
                states.frag_recovery_delay = Duration::from_secs(60);
                handle_sample(&mut states, single(0));
                handle_sample(&mut states, frag(1, 0));
                if evict {
                    handle_sample(&mut states, single(3));
                } else {
                    handle_sample(&mut states, frag(3, 0));
                    handle_sample(&mut states, single(5));
                }
                states.on_history_or_sample_recovery_finished(&statesref, &source_id);
                if evict {
                    handle_sample(&mut states, frag(4, 0));
                    let state = states.sequenced_states.peek(&source_id).unwrap();
                    assert_eq!(state.last_delivered, Some(WrappingSn(3)));
                    assert_eq!(state.pending_samples.len(), 1);
                    assert!(state.pending_samples.contains_key(&WrappingSn(4)));
                    assert!(state.gap_skip_through.is_none());
                } else {
                    handle_sample(&mut states, frag(1, 1));
                    let state = states.sequenced_states.peek(&source_id).unwrap();
                    assert_eq!(state.last_delivered, Some(WrappingSn(1)));
                    assert!(state
                        .pending_samples
                        .get(&WrappingSn(3))
                        .unwrap()
                        .is_incomplete());
                    assert_eq!(state.gap_skip_through, Some(WrappingSn(5)));
                    handle_sample(&mut states, frag(3, 1));
                    let state = states.sequenced_states.peek(&source_id).unwrap();
                    assert_eq!(state.last_delivered, Some(WrappingSn(5)));
                    assert!(state.pending_samples.is_empty());
                    assert!(state.gap_skip_through.is_none());
                }
            }
            assert_eq!(
                *misses.lock().unwrap(),
                if evict { vec![2] } else { vec![1, 1] }
            );
            assert_eq!(received.lock().unwrap().len(), if evict { 2 } else { 4 });
        }
        ztimeout!(session.close()).unwrap();
    }

    /// Finishing fragment recovery must not change which missing samples can
    /// be skipped. Another sample-recovery query can extend that limit. Check both
    /// orders in which the queries can finish.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_deferred_flush_overlapping_queries_and_boundary_extension() {
        let mut config = Config::default();
        config.scouting.multicast.set_enabled(Some(false)).unwrap();
        config.listen.endpoints.set(vec![]).unwrap();
        let session = ztimeout!(zenoh::open(config)).unwrap();
        let source_id = EntityGlobalId::new(session.zid(), 7);
        let cache = ztimeout!(session.declare_queryable("test/ext/pending/@adv/**")).unwrap();
        let single = |sn| -> Sample {
            SampleBuilder::put(KeyExpr::try_from("test/ext/pending").unwrap(), "single")
                .source_info(SourceInfo::new(source_id, sn))
                .into()
        };
        let frag = |num| -> Sample {
            SampleBuilder::put(KeyExpr::try_from("test/ext/pending").unwrap(), "ab")
                .source_info(SourceInfo::new(source_id, 1))
                .frag_info(FragInfo::new(2, num))
                .into()
        };
        for (sample_query_first, extend) in [(false, false), (true, false), (true, true)] {
            let received = Arc::new(Mutex::new(Vec::<Sample>::new()));
            let misses = Arc::new(Mutex::new(Vec::<u32>::new()));
            let statesref = pending_bound_state(&session, 10, received.clone(), misses.clone());
            let (generation, attempt) = {
                let mut states = zlock!(statesref);
                states.retransmission = true;
                states.frag_recovery_delay = Duration::from_secs(60);
                handle_sample(&mut states, single(0));
                handle_sample(&mut states, frag(0));
                handle_sample(&mut states, single(3));
                let state = states.sequenced_states.peek_mut(&source_id).unwrap();
                let attempt = state
                    .pending_samples
                    .get_mut(&WrappingSn(1))
                    .unwrap()
                    .prepare_recovery(Duration::ZERO, true)
                    .unwrap()
                    .1;
                state.pending_queries = 2;
                (state.generation.clone(), attempt)
            };
            let sample_query = SequencedRepliesHandler {
                source_id,
                statesref: statesref.clone(),
            };
            let fragment = FragmentAttempt {
                source_id,
                statesref: statesref.clone(),
                generation,
                sn: WrappingSn(1),
                ranges: vec![(Some(1), None)],
                attempt,
            };
            if sample_query_first {
                drop(sample_query);
                // This successor was not covered by the completed query.
                handle_sample(&mut zlock!(statesref), single(5));
                if extend {
                    zlock!(statesref)
                        .sequenced_states
                        .peek_mut(&source_id)
                        .unwrap()
                        .pending_queries += 1;
                    drop(SequencedRepliesHandler {
                        source_id,
                        statesref: statesref.clone(),
                    });
                    assert_eq!(
                        zlock!(statesref)
                            .sequenced_states
                            .peek(&source_id)
                            .unwrap()
                            .gap_skip_through,
                        Some(WrappingSn(5))
                    );
                }
                drop(fragment);
                let states = zlock!(statesref);
                let state = states.sequenced_states.peek(&source_id).unwrap();
                assert_eq!(
                    state.last_delivered,
                    Some(WrappingSn(if extend { 5 } else { 3 }))
                );
                assert_eq!(state.pending_samples.contains_key(&WrappingSn(5)), !extend);
            } else {
                drop(fragment);
                drop(sample_query);
                handle_sample(&mut zlock!(statesref), single(5));
            }
            // A new sample-recovery query allows skipping missing SN 4. Hold
            // its reply handler so the scheduled gap recheck cannot flush early.
            if !extend {
                SequencedRepliesHandler::recover_gap(&statesref, source_id);
                let query = ztimeout!(cache.recv_async()).unwrap();
                assert_eq!(query.parameters().get("_sn"), Some("4.."));
                drop(query);
            }
            ztimeout!(async {
                loop {
                    if zlock!(statesref)
                        .sequenced_states
                        .peek(&source_id)
                        .unwrap()
                        .last_delivered
                        == Some(WrappingSn(5))
                    {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            });
            {
                let states = zlock!(statesref);
                let state = states.sequenced_states.peek(&source_id).unwrap();
                assert_eq!(state.last_delivered, Some(WrappingSn(5)));
                assert!(state.gap_skip_through.is_none());
            }
            assert_eq!(*misses.lock().unwrap(), [2, 1]);
            assert_eq!(received.lock().unwrap().len(), 3);
        }
        ztimeout!(session.close()).unwrap();
    }

    /// Finishing a history or sample-recovery query may skip missing sequence
    /// numbers. Keep newer incomplete samples so later publications can fill
    /// their missing fragments.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_query_completion_preserves_trailing_partial_samples() {
        let mut config = Config::default();
        config.scouting.multicast.set_enabled(Some(false)).unwrap();
        config.listen.endpoints.set(vec![]).unwrap();
        let session = ztimeout!(zenoh::open(config)).unwrap();
        let source_id = EntityGlobalId::new(session.zid(), 7);
        let key_expr = KeyExpr::try_from("test/ext/pending").unwrap();

        for history in [false, true] {
            for advance_past_gap in [false, true] {
                let received = Arc::new(Mutex::new(Vec::<Sample>::new()));
                let misses = Arc::new(Mutex::new(Vec::<u32>::new()));
                let statesref = pending_bound_state(&session, 10, received.clone(), misses.clone());
                let frag = |sn: u32, num: u32, payload: &str| -> Sample {
                    SampleBuilder::put(key_expr.clone(), payload)
                        .source_info(SourceInfo::new(source_id, sn))
                        .frag_info(FragInfo::new(2, num))
                        .into()
                };
                let tail_sn = if advance_past_gap { 3 } else { 1 };
                {
                    let mut states = zlock!(statesref);
                    handle_sample(
                        &mut states,
                        SampleBuilder::put(key_expr.clone(), "base")
                            .source_info(SourceInfo::new(source_id, 0))
                            .into(),
                    );
                    states.global_pending_queries = u64::from(history);
                    let state = states.sequenced_states.peek_mut(&source_id).unwrap();
                    state.pending_queries = u64::from(!history);
                    // Model the buffer at query completion, independently of
                    // whether each entry originated from a reply or live data.
                    let mut pending = vec![frag(1, 0, "ab")];
                    if advance_past_gap {
                        pending.push(
                            SampleBuilder::put(key_expr.clone(), "next")
                                .source_info(SourceInfo::new(source_id, 2))
                                .into(),
                        );
                        pending.push(frag(3, 0, "ab"));
                    }
                    for sample in pending {
                        let info = sample.source_info().unwrap().clone();
                        state
                            .insert_sample(sample, &info, MAX_FRAGMENTS_DEFAULT)
                            .unwrap();
                    }
                }
                if history {
                    drop(InitialRepliesHandler {
                        statesref: statesref.clone(),
                    });
                } else {
                    drop(SequencedRepliesHandler {
                        source_id,
                        statesref: statesref.clone(),
                    });
                }

                let (last_delivered, pending) = {
                    let states = zlock!(statesref);
                    let state = states.sequenced_states.peek(&source_id).unwrap();
                    (
                        state.last_delivered,
                        state.pending_samples.keys().copied().collect::<Vec<_>>(),
                    )
                };
                assert_eq!(last_delivered, Some(WrappingSn(tail_sn - 1)));
                assert_eq!(
                    pending,
                    [WrappingSn(tail_sn)],
                    "history={history}, advance_past_gap={advance_past_gap}"
                );
                // No retransmission: only the remaining fragment arrives.
                handle_sample(&mut zlock!(statesref), frag(tail_sn, 1, "cd"));
                if advance_past_gap {
                    // The skipped sample remains obsolete even if it completes.
                    handle_sample(&mut zlock!(statesref), frag(1, 1, "cd"));
                }
                let delivered: Vec<_> = received
                    .lock()
                    .unwrap()
                    .iter()
                    .map(|sample| sample.source_info().unwrap().source_sn())
                    .collect();
                assert_eq!(
                    delivered,
                    if advance_past_gap {
                        vec![0, 2, 3]
                    } else {
                        vec![0, 1]
                    }
                );
                assert_eq!(
                    *misses.lock().unwrap(),
                    if advance_past_gap { vec![1] } else { vec![] }
                );
                assert_eq!(
                    received
                        .lock()
                        .unwrap()
                        .last()
                        .unwrap()
                        .payload()
                        .try_to_string()
                        .unwrap(),
                    "abcd"
                );
            }
        }
        ztimeout!(session.close()).unwrap();
    }

    /// If the last sample in history or sample-recovery replies has missing
    /// fragments, query for them after that query finishes.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_query_completion_recovers_trailing_partial_sample() {
        let mut config = Config::default();
        config.scouting.multicast.set_enabled(Some(false)).unwrap();
        config.listen.endpoints.set(vec![]).unwrap();
        let session = ztimeout!(zenoh::open(config)).unwrap();
        let source_id = EntityGlobalId::new(session.zid(), 7);
        let key_expr = KeyExpr::try_from("test/ext/frag/query_tail").unwrap();

        for history in [false, true] {
            let cache =
                ztimeout!(session.declare_queryable("test/ext/frag/query_tail/@adv/**")).unwrap();
            let builder = session.declare_subscriber(&key_expr).advanced().recovery(
                RecoveryConfig::default().fragments_recovery_delay(Duration::from_millis(50)),
            );
            let sub = ztimeout!(if history {
                builder.history(HistoryConfig::default())
            } else {
                builder
            })
            .unwrap();
            let misses = ztimeout!(sub.sample_miss_listener()).unwrap();
            if !history {
                ztimeout!(session
                    .put(&key_expr, "base")
                    .source_info(SourceInfo::new(source_id, 0)))
                .unwrap();
                let baseline = ztimeout!(sub.recv_async()).unwrap();
                assert_eq!(baseline.source_info().unwrap().source_sn(), 0);
                // Missing SN 1 triggers a sample-recovery query. The query's
                // trailing partial must survive delivery of complete SN 2.
                ztimeout!(session
                    .put(&key_expr, "next")
                    .source_info(SourceInfo::new(source_id, 2)))
                .unwrap();
            }
            let query = ztimeout!(cache.recv_async()).unwrap();
            let tail_sn = if history { 1 } else { 3 };
            ztimeout!(query.reply_sample(
                SampleBuilder::put(key_expr.clone(), "ab")
                    .source_info(SourceInfo::new(source_id, tail_sn))
                    .frag_info(FragInfo::new(2, 0))
                    .into(),
            ))
            .unwrap();
            drop(query);
            if !history {
                let next = ztimeout!(sub.recv_async()).unwrap();
                assert_eq!(next.source_info().unwrap().source_sn(), 2);
                assert_eq!(ztimeout!(misses.recv_async()).unwrap().nb(), 1);
            }

            let recovery = ztimeout!(cache.recv_async()).unwrap();
            assert_eq!(recovery.parameters().get("_fn"), Some("1.."));
            assert_eq!(
                recovery.parameters().get("_sn"),
                Some(if history { "1..1" } else { "3..3" })
            );
            ztimeout!(recovery.reply_sample(
                SampleBuilder::put(key_expr.clone(), "cd")
                    .source_info(SourceInfo::new(source_id, tail_sn))
                    .frag_info(FragInfo::new(2, 1))
                    .into(),
            ))
            .unwrap();
            drop(recovery);
            let sample = ztimeout!(sub.recv_async()).unwrap();
            assert_eq!(sample.source_info().unwrap().source_sn(), tail_sn);
            assert_eq!(sample.payload().try_to_string().unwrap(), "abcd");
            assert!(sub.try_recv().unwrap().is_none());
            assert!(misses.try_recv().unwrap().is_none());
        }
        ztimeout!(session.close()).unwrap();
    }

    /// History completion may wait for a fragment query. Whichever query
    /// finishes last must release the completed historical samples.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_query_completion_flushes_after_overlapping_fragment_query() {
        let mut config = Config::default();
        config.scouting.multicast.set_enabled(Some(false)).unwrap();
        config.listen.endpoints.set(vec![]).unwrap();
        let session = ztimeout!(zenoh::open(config)).unwrap();
        let source_id = EntityGlobalId::new(session.zid(), 7);
        let key_expr = KeyExpr::try_from("test/ext/pending").unwrap();

        for history_first in [false, true] {
            let received = Arc::new(Mutex::new(Vec::<Sample>::new()));
            let misses = Arc::new(Mutex::new(Vec::<u32>::new()));
            let statesref = pending_bound_state(&session, 10, received.clone(), misses.clone());
            {
                let mut states = zlock!(statesref);
                states.global_pending_queries = 1;
                states.retransmission = true;
                for (num, payload) in [(0, "ab"), (1, "cd")] {
                    handle_sample(
                        &mut states,
                        SampleBuilder::put(key_expr.clone(), payload)
                            .source_info(SourceInfo::new(source_id, 0))
                            .frag_info(FragInfo::new(2, num))
                            .into(),
                    );
                }
                states
                    .sequenced_states
                    .peek_mut(&source_id)
                    .unwrap()
                    .pending_queries = 1;
            }
            let history = InitialRepliesHandler {
                statesref: statesref.clone(),
            };
            let fragment = FragmentAttempt {
                source_id,
                statesref: statesref.clone(),
                generation: zlock!(statesref)
                    .sequenced_states
                    .peek(&source_id)
                    .unwrap()
                    .generation
                    .clone(),
                sn: WrappingSn(0),
                ranges: Vec::new(),
                attempt: Arc::new(()),
            };
            if history_first {
                drop(history);
                assert!(received.lock().unwrap().is_empty());
                drop(fragment);
            } else {
                drop(fragment);
                assert!(received.lock().unwrap().is_empty());
                drop(history);
            }
            let delivered: Vec<_> = received
                .lock()
                .unwrap()
                .iter()
                .map(|sample| sample.payload().try_to_string().unwrap().into_owned())
                .collect();
            assert_eq!(delivered, ["abcd"], "history_first={history_first}");
            assert!(misses.lock().unwrap().is_empty());
        }
        ztimeout!(session.close()).unwrap();
    }

    /// Skipping an incomplete sample must not leave it blocking subsequent
    /// fragmented samples, even without enough traffic to force eviction.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_delivery_discards_obsolete_partial_samples() {
        let mut config = Config::default();
        config.scouting.multicast.set_enabled(Some(false)).unwrap();
        config.listen.endpoints.set(vec![]).unwrap();
        let session = ztimeout!(zenoh::open(config)).unwrap();
        let source_id = EntityGlobalId::new(ZenohId::default(), 7);
        let key_expr = KeyExpr::try_from("test/ext/pending").unwrap();

        for baseline in [0u32, u32::MAX - 2] {
            for delete in [false, true] {
                for successor_first in [false, true] {
                    let received = Arc::new(Mutex::new(Vec::<Sample>::new()));
                    let misses = Arc::new(Mutex::new(Vec::<u32>::new()));
                    let statesref =
                        pending_bound_state(&session, 10, received.clone(), misses.clone());
                    let handle = |s: Sample| {
                        handle_sample(&mut zlock!(statesref), s);
                    };
                    let frag = |sn: u32, num: u32, payload: &str| {
                        SampleBuilder::put(key_expr.clone(), payload)
                            .frag_info(FragInfo::new(2, num))
                            .source_info(SourceInfo::new(source_id, sn))
                            .into()
                    };
                    let partial = baseline.wrapping_add(1);
                    let jump = baseline.wrapping_add(2);
                    let successor = baseline.wrapping_add(3);
                    handle(
                        SampleBuilder::put(key_expr.clone(), "base")
                            .source_info(SourceInfo::new(source_id, baseline))
                            .into(),
                    );
                    handle(frag(partial, 0, "lost"));
                    let complete_successor = || {
                        handle(frag(successor, 0, "ab"));
                        handle(frag(successor, 1, "cd"));
                    };
                    if successor_first {
                        complete_successor();
                    }
                    handle(if delete {
                        SampleBuilder::delete(key_expr.clone())
                            .source_info(SourceInfo::new(source_id, jump))
                            .into()
                    } else {
                        SampleBuilder::put(key_expr.clone(), "jump")
                            .source_info(SourceInfo::new(source_id, jump))
                            .into()
                    });
                    if !successor_first {
                        complete_successor();
                    }

                    // Late fragments must not resurrect the skipped sample or
                    // produce another miss notification.
                    handle(frag(partial, 1, "late"));
                    handle(frag(partial, 0, "lost"));
                    let delivered: Vec<_> = received
                        .lock()
                        .unwrap()
                        .iter()
                        .map(|s| s.source_info().unwrap().source_sn())
                        .collect();
                    assert_eq!(
                        delivered,
                        if successor_first {
                            vec![baseline, successor]
                        } else {
                            vec![baseline, jump, successor]
                        },
                        "baseline={baseline}, delete={delete}, successor_first={successor_first}"
                    );
                    assert_eq!(
                        received
                            .lock()
                            .unwrap()
                            .last()
                            .unwrap()
                            .payload()
                            .try_to_string()
                            .unwrap(),
                        "abcd"
                    );
                    assert_eq!(
                        *misses.lock().unwrap(),
                        [if successor_first { 2 } else { 1 }]
                    );
                    let pending_empty = zlock!(statesref)
                        .sequenced_states
                        .peek(&source_id)
                        .unwrap()
                        .pending_samples
                        .is_empty();
                    assert!(pending_empty);
                }
            }
        }
        ztimeout!(session.close()).unwrap();
    }

    /// With recovery enabled, a newer complete sample must remain buffered
    /// while a preceding partial sample is recovered, rather than pruning it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_recovery_preserves_partial_sample_before_complete_successor() {
        let mut config = Config::default();
        config.scouting.multicast.set_enabled(Some(false)).unwrap();
        config.listen.endpoints.set(vec![]).unwrap();
        let session = ztimeout!(zenoh::open(config)).unwrap();
        let key_expr = "test/ext/frag/preserve_partial";
        let source_id = EntityGlobalId::new(session.zid(), 7);
        // Hold the recovery query until the test has inspected the pending
        // samples, then return only the missing fragment through the reply path.
        let cache =
            ztimeout!(session.declare_queryable("test/ext/frag/preserve_partial/@adv/**")).unwrap();
        let sub = ztimeout!(session
            .declare_subscriber(key_expr)
            .advanced()
            .max_pending_samples(10)
            .recovery(
                RecoveryConfig::default().fragments_recovery_delay(Duration::from_millis(50)),
            ))
        .unwrap();
        let misses = ztimeout!(sub.sample_miss_listener()).unwrap();

        ztimeout!(session
            .put(key_expr, "base")
            .source_info(SourceInfo::new(source_id, 0)))
        .unwrap();
        let baseline = ztimeout!(sub.recv_async()).unwrap();
        assert_eq!(baseline.source_info().unwrap().source_sn(), 0);

        ztimeout!(session
            .put(key_expr, "ab")
            .source_info(SourceInfo::new(source_id, 1))
            .frag_info(FragInfo::new(2, 0)))
        .unwrap();
        ztimeout!(session
            .put(key_expr, "next")
            .source_info(SourceInfo::new(source_id, 2)))
        .unwrap();

        let query = ztimeout!(cache.recv_async()).unwrap();
        assert_eq!(query.parameters().get("_sn"), Some("1..1"));
        assert_eq!(query.parameters().get("_fn"), Some("1.."));
        let (last_delivered, pending) = {
            let states = zlock!(sub.statesref);
            let state = states.sequenced_states.peek(&source_id).unwrap();
            (
                state.last_delivered,
                state
                    .pending_samples
                    .iter()
                    .map(|(sn, sample)| (*sn, sample.is_complete()))
                    .collect::<Vec<_>>(),
            )
        };
        assert_eq!(last_delivered, Some(WrappingSn(0)));
        assert_eq!(pending, [(WrappingSn(1), false), (WrappingSn(2), true)]);
        assert!(sub.try_recv().unwrap().is_none());
        assert!(misses.try_recv().unwrap().is_none());

        ztimeout!(query.reply_sample(
            SampleBuilder::put(KeyExpr::try_from(key_expr).unwrap(), "cd")
                .source_info(SourceInfo::new(source_id, 1))
                .frag_info(FragInfo::new(2, 1))
                .into(),
        ))
        .unwrap();
        drop(query);

        for (sn, payload) in [(1, "abcd"), (2, "next")] {
            let sample = ztimeout!(sub.recv_async()).unwrap();
            assert_eq!(sample.source_info().unwrap().source_sn(), sn);
            assert_eq!(sample.payload().try_to_string().unwrap(), payload);
        }
        assert!(sub.try_recv().unwrap().is_none());
        assert!(misses.try_recv().unwrap().is_none());
        let pending_empty = zlock!(sub.statesref)
            .sequenced_states
            .peek(&source_id)
            .unwrap()
            .pending_samples
            .is_empty();
        assert!(pending_empty);
        ztimeout!(session.close()).unwrap();
    }

    /// The depth-one history fast path must also discard a partial sample
    /// when its first delivery establishes a newer sequence-number baseline.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_history_delivery_discards_obsolete_partial_samples() {
        let mut config = Config::default();
        config.scouting.multicast.set_enabled(Some(false)).unwrap();
        config.listen.endpoints.set(vec![]).unwrap();
        let session = ztimeout!(zenoh::open(config)).unwrap();
        let source_id = EntityGlobalId::new(ZenohId::default(), 7);
        let key_expr = KeyExpr::try_from("test/ext/pending").unwrap();
        let received = Arc::new(Mutex::new(Vec::<Sample>::new()));
        let misses = Arc::new(Mutex::new(Vec::<u32>::new()));
        let statesref = pending_bound_state(&session, 1, received.clone(), misses.clone());
        {
            let mut states = zlock!(statesref);
            states.global_pending_queries = 1;
            handle_sample(
                &mut states,
                SampleBuilder::put(key_expr.clone(), "partial")
                    .source_info(SourceInfo::new(source_id, 1))
                    .frag_info(FragInfo::new(2, 0))
                    .into(),
            );
            handle_sample(
                &mut states,
                SampleBuilder::put(key_expr, "new")
                    .source_info(SourceInfo::new(source_id, 2))
                    .into(),
            );
        }
        let delivered: Vec<_> = received
            .lock()
            .unwrap()
            .iter()
            .map(|s| s.source_info().unwrap().source_sn())
            .collect();
        assert_eq!(delivered, [2]);
        assert!(misses.lock().unwrap().is_empty());
        let pending_empty = zlock!(statesref)
            .sequenced_states
            .peek(&source_id)
            .unwrap()
            .pending_samples
            .is_empty();
        assert!(pending_empty);
        ztimeout!(session.close()).unwrap();
    }

    /// Eviction can make buffered successors immediately deliverable even
    /// though the pre-eviction delivery check was blocked by an incomplete head.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_pending_eviction_retries_ready_delivery_with_recovery() {
        let mut config = Config::default();
        config.scouting.multicast.set_enabled(Some(false)).unwrap();
        config.listen.endpoints.set(vec![]).unwrap();
        let session = ztimeout!(zenoh::open(config)).unwrap();
        let source_id = EntityGlobalId::new(session.zid(), 7);
        let key = "test/ext/frag/eviction_progress";

        for depth in [1, 3] {
            // A held query would prevent accidental progress from recovery.
            // No query should be needed: overflow happens before the scan tick.
            let cache =
                ztimeout!(session.declare_queryable("test/ext/frag/eviction_progress/@adv/**"))
                    .unwrap();
            let sub = ztimeout!(session
                .declare_subscriber(key)
                .advanced()
                .max_pending_samples(depth)
                .recovery(
                    RecoveryConfig::default().fragments_recovery_delay(Duration::from_secs(10)),
                ))
            .unwrap();
            let misses = ztimeout!(sub.sample_miss_listener()).unwrap();
            ztimeout!(session
                .put(key, "base")
                .source_info(SourceInfo::new(source_id, 0)))
            .unwrap();
            assert_eq!(
                ztimeout!(sub.recv_async())
                    .unwrap()
                    .source_info()
                    .unwrap()
                    .source_sn(),
                0
            );
            ztimeout!(session
                .put(key, "partial")
                .source_info(SourceInfo::new(source_id, 1))
                .frag_info(FragInfo::new(2, 0)))
            .unwrap();
            for sn in 2..=depth as u32 + 1 {
                assert!(sub.try_recv().unwrap().is_none());
                ztimeout!(session
                    .put(key, "complete")
                    .source_info(SourceInfo::new(source_id, sn)))
                .unwrap();
            }

            // Stop publishing. All complete successors must be released by
            // eviction, without another arrival or recovery timer to wake them.
            for sn in 2..=depth as u32 + 1 {
                let sample = tokio::time::timeout(Duration::from_secs(2), sub.recv_async())
                    .await
                    .expect("eviction left a complete successor buffered")
                    .unwrap();
                assert_eq!(sample.source_info().unwrap().source_sn(), sn);
                assert_eq!(sample.payload().try_to_string().unwrap(), "complete");
            }
            assert_eq!(ztimeout!(misses.recv_async()).unwrap().nb(), 1);
            assert!(misses.try_recv().unwrap().is_none());
            assert!(sub.try_recv().unwrap().is_none());
            assert!(cache.try_recv().unwrap().is_none());
            let (pending_empty, last_evicted, scan_stopped) = {
                let states = zlock!(sub.statesref);
                let state = states.sequenced_states.peek(&source_id).unwrap();
                (
                    state.pending_samples.is_empty(),
                    state.last_evicted,
                    state.frag_recovery_task.is_none(),
                )
            };
            assert!(pending_empty);
            assert_eq!(last_evicted, None);
            assert!(scan_stopped);
        }
        ztimeout!(session.close()).unwrap();
    }

    /// Retrying after eviction must stop at a surviving recoverable partial,
    /// rather than flushing every complete sample in the buffer.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_pending_eviction_preserves_recoverable_head() {
        let mut config = Config::default();
        config.scouting.multicast.set_enabled(Some(false)).unwrap();
        config.listen.endpoints.set(vec![]).unwrap();
        let session = ztimeout!(zenoh::open(config)).unwrap();
        let source_id = EntityGlobalId::new(session.zid(), 7);
        let key = KeyExpr::try_from("test/ext/pending").unwrap();
        let received = Arc::new(Mutex::new(Vec::<Sample>::new()));
        let misses = Arc::new(Mutex::new(Vec::<u32>::new()));
        let statesref = pending_bound_state(&session, 3, received.clone(), misses.clone());
        let (last_delivered, pending) = {
            let mut states = zlock!(statesref);
            states.retransmission = true;
            handle_sample(
                &mut states,
                SampleBuilder::put(key.clone(), "base")
                    .source_info(SourceInfo::new(source_id, 0))
                    .into(),
            );
            for sn in [1, 2] {
                handle_sample(
                    &mut states,
                    SampleBuilder::put(key.clone(), "a")
                        .source_info(SourceInfo::new(source_id, sn))
                        .frag_info(FragInfo::new(2, 0))
                        .into(),
                );
            }
            for sn in [3, 4] {
                handle_sample(
                    &mut states,
                    SampleBuilder::put(key.clone(), "complete")
                        .source_info(SourceInfo::new(source_id, sn))
                        .into(),
                );
            }
            let state = states.sequenced_states.peek(&source_id).unwrap();
            (
                state.last_delivered,
                state
                    .pending_samples
                    .iter()
                    .map(|(sn, sample)| (*sn, sample.is_complete()))
                    .collect::<Vec<_>>(),
            )
        };
        assert_eq!(last_delivered, Some(WrappingSn(0)));
        assert_eq!(
            pending,
            [
                (WrappingSn(2), false),
                (WrappingSn(3), true),
                (WrappingSn(4), true)
            ]
        );
        assert_eq!(received.lock().unwrap().len(), 1);
        assert!(misses.lock().unwrap().is_empty());
        handle_sample(
            &mut zlock!(statesref),
            SampleBuilder::put(key, "b")
                .source_info(SourceInfo::new(source_id, 2))
                .frag_info(FragInfo::new(2, 1))
                .into(),
        );
        let delivered: Vec<_> = received
            .lock()
            .unwrap()
            .iter()
            .map(|sample| sample.source_info().unwrap().source_sn())
            .collect();
        assert_eq!(delivered, [0, 2, 3, 4]);
        assert_eq!(*misses.lock().unwrap(), [1]);
        ztimeout!(session.close()).unwrap();
    }

    /// An incomplete head sample must not block delivery forever: once the
    /// pending buffer exceeds its bound, the incomplete head is discarded and
    /// the complete successors are delivered. No miss is reported: delivery
    /// with no `last_delivered` never closes a gap.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_pending_bound_unblocks_incomplete_head() {
        zenoh_util::init_log_from_env_or("error");
        let session = ztimeout!(zenoh::open(Config::default())).unwrap();
        let source_id = EntityGlobalId::new(ZenohId::default(), 7);
        let key_expr = KeyExpr::try_from("test/ext/pending").unwrap();

        let received = Arc::new(Mutex::new(Vec::<Sample>::new()));
        let misses = Arc::new(Mutex::new(Vec::<u32>::new()));
        let statesref = pending_bound_state(&session, 4, received.clone(), misses.clone());
        // Recovery preserves the incomplete head until recovery or eviction;
        // without recovery complete successors now advance immediately.
        zlock!(statesref).retransmission = true;

        let handle = |s: Sample| {
            let mut states = zlock!(statesref);
            let _ = handle_sample(&mut states, s);
        };
        let frag = |sn: u32, num: u32, p: &str| {
            SampleBuilder::put(key_expr.clone(), p)
                .frag_info(FragInfo::new(3, num))
                .source_info(SourceInfo::new(source_id, sn))
                .into()
        };

        // No baseline delivered yet (`last_delivered` is `None`), so every
        // sample takes the in-order path and nothing reports a miss: no
        // delivery ever closes a gap.
        // SN 1 arrives incomplete (frag 1 missing) and blocks the head.
        handle(frag(1, 0, "ab"));
        handle(frag(1, 2, "ef"));

        // Complete fragmented successors fill the buffer until the bound.
        for (sn, p0, p1, p2) in [
            (2u32, "ab", "cd", "ef"),
            (3, "gh", "ij", "kl"),
            (4, "mn", "op", "qr"),
        ] {
            for (num, p) in [(0u32, p0), (1, p1), (2, p2)] {
                handle(frag(sn, num, p));
            }
        }
        // Buffer at bound (4): SN 1 still blocks, nothing delivered yet.
        assert!(received.lock().unwrap().is_empty());
        assert_eq!(misses.lock().unwrap().len(), 0);

        // Next sample exceeds the bound: SN 1 is missed, SN 2..4 flushed.
        handle(frag(5, 0, "st"));
        handle(frag(5, 1, "uv"));
        handle(frag(5, 2, "wx"));

        let got: Vec<String> = received
            .lock()
            .unwrap()
            .iter()
            .map(|s| s.payload().try_to_string().unwrap().to_string())
            .collect();
        assert_eq!(got, ["abcdef", "ghijkl", "mnopqr", "stuvwx"]);
        assert!(misses.lock().unwrap().is_empty());

        let mut states = zlock!(statesref);
        let state = states.sequenced_states.get(&source_id).unwrap();
        assert!(state.pending_samples.is_empty());
        assert_eq!(state.last_delivered, Some(WrappingSn(5)));
    }

    /// Incomplete samples must not accumulate beyond the pending bound: the
    /// oldest are discarded without reporting a miss.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_pending_bound_enforced() {
        zenoh_util::init_log_from_env_or("error");
        let session = ztimeout!(zenoh::open(Config::default())).unwrap();
        let source_id = EntityGlobalId::new(ZenohId::default(), 7);
        let key_expr = KeyExpr::try_from("test/ext/pending").unwrap();

        let received = Arc::new(Mutex::new(Vec::<Sample>::new()));
        let misses = Arc::new(Mutex::new(Vec::<u32>::new()));
        let statesref = pending_bound_state(&session, 4, received.clone(), misses.clone());

        for sn in 0u32..10 {
            let s: Sample = SampleBuilder::put(key_expr.clone(), "x")
                .frag_info(FragInfo::new(3, 0))
                .source_info(SourceInfo::new(source_id, sn))
                .into();
            let mut states = zlock!(statesref);
            let _ = handle_sample(&mut states, s);
        }

        // 6 of the 10 incomplete samples evicted, 4 kept, none delivered,
        // and eviction is silent.
        assert!(misses.lock().unwrap().is_empty());
        assert!(received.lock().unwrap().is_empty());
        let mut states = zlock!(statesref);
        let state = states.sequenced_states.get(&source_id).unwrap();
        assert_eq!(state.pending_samples.len(), 4);
    }

    /// A delivery closing a gap reports the missed samples exactly once, when
    /// the assembled sample is delivered: fragments of the sample crossing the
    /// gap must not re-report.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_miss_reported_once_on_crossing_delivery() {
        zenoh_util::init_log_from_env_or("error");
        let session = ztimeout!(zenoh::open(Config::default())).unwrap();
        let source_id = EntityGlobalId::new(ZenohId::default(), 7);
        let key_expr = KeyExpr::try_from("test/ext/pending").unwrap();

        let received = Arc::new(Mutex::new(Vec::<Sample>::new()));
        let misses = Arc::new(Mutex::new(Vec::<u32>::new()));
        let statesref = pending_bound_state(&session, 4, received.clone(), misses.clone());

        let handle = |s: Sample| {
            let mut states = zlock!(statesref);
            let _ = handle_sample(&mut states, s);
        };
        let frag = |sn: u32, num: u32, p: &str| {
            SampleBuilder::put(key_expr.clone(), p)
                .frag_info(FragInfo::new(3, num))
                .source_info(SourceInfo::new(source_id, sn))
                .into()
        };

        // Baseline: SN 0 delivered in-order.
        handle(
            SampleBuilder::put(key_expr.clone(), "base")
                .source_info(SourceInfo::new(source_id, 0))
                .into(),
        );

        // SN 1 is lost; SN 2's fragments all cross the gap but only the
        // assembled sample's delivery reports it.
        for (num, p) in [(0u32, "ab"), (1, "cd"), (2, "ef")] {
            handle(frag(2, num, p));
        }

        let got: Vec<String> = received
            .lock()
            .unwrap()
            .iter()
            .map(|s| s.payload().try_to_string().unwrap().to_string())
            .collect();
        assert_eq!(got, ["base", "abcdef"]);
        assert_eq!(*misses.lock().unwrap(), [1]);
    }

    /// A non-fragmented sample closing a gap reports the skipped count on
    /// delivery, preserving the pre-fragmentation behavior.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_miss_jump_count_on_delivery() {
        zenoh_util::init_log_from_env_or("error");
        let session = ztimeout!(zenoh::open(Config::default())).unwrap();
        let source_id = EntityGlobalId::new(ZenohId::default(), 7);
        let key_expr = KeyExpr::try_from("test/ext/pending").unwrap();

        let received = Arc::new(Mutex::new(Vec::<Sample>::new()));
        let misses = Arc::new(Mutex::new(Vec::<u32>::new()));
        let statesref = pending_bound_state(&session, 4, received.clone(), misses.clone());

        let handle = |s: Sample| {
            let mut states = zlock!(statesref);
            let _ = handle_sample(&mut states, s);
        };
        let plain = |sn: u32, p: &str| {
            SampleBuilder::put(key_expr.clone(), p)
                .source_info(SourceInfo::new(source_id, sn))
                .into()
        };

        handle(plain(0, "base"));
        // SN 1 and 2 are lost; SN 3 closes the gap.
        handle(plain(3, "jump"));

        let got: Vec<String> = received
            .lock()
            .unwrap()
            .iter()
            .map(|s| s.payload().try_to_string().unwrap().to_string())
            .collect();
        assert_eq!(got, ["base", "jump"]);
        assert_eq!(*misses.lock().unwrap(), [2]);
    }

    /// The `max_pending_samples` knob overrides `history.max_samples` and
    /// defaults to `DEFAULT_MAX_PENDING_SAMPLES`; zero is rejected.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_max_pending_samples_builder() {
        zenoh_util::init_log_from_env_or("error");
        let session = ztimeout!(zenoh::open(Config::default())).unwrap();

        let sub = ztimeout!(session
            .declare_subscriber("test/ext/mps/a")
            .advanced()
            .max_pending_samples(7))
        .unwrap();
        assert_eq!(zlock!(sub.statesref).max_pending_samples, 7);

        let sub2 = ztimeout!(session
            .declare_subscriber("test/ext/mps/b")
            .advanced()
            .history(HistoryConfig::default().max_samples(5)))
        .unwrap();
        assert_eq!(zlock!(sub2.statesref).max_pending_samples, 5);

        let sub3 = ztimeout!(session
            .declare_subscriber("test/ext/mps/c")
            .advanced()
            .history(HistoryConfig::default().max_samples(5))
            .max_pending_samples(9))
        .unwrap();
        assert_eq!(zlock!(sub3.statesref).max_pending_samples, 9);

        let sub4 = ztimeout!(session.declare_subscriber("test/ext/mps/d").advanced()).unwrap();
        assert_eq!(
            zlock!(sub4.statesref).max_pending_samples,
            DEFAULT_MAX_PENDING_SAMPLES
        );

        let zero = ztimeout!(session
            .declare_subscriber("test/ext/mps/z")
            .advanced()
            .max_pending_samples(0));
        assert!(zero.is_err());

        drop((sub, sub2, sub3, sub4));
        let _ = ztimeout!(session.close());
    }

    /// The publisher timestamps only fragment 0 and attaches only to
    /// fragment 0; the assembled sample delivered to the user carries
    /// fragment 0's timestamp and attachment.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_fragment_timestamp_on_first_fragment() {
        zenoh_util::init_log_from_env_or("error");

        let mut config = Config::default();
        config.scouting.multicast.set_enabled(Some(false)).unwrap();
        config.set_mode(Some(WhatAmI::Peer)).unwrap();
        config
            .timestamping
            .set_enabled(Some(ModeDependentValue::Unique(true)))
            .unwrap();
        let session = ztimeout!(zenoh::open(config)).unwrap();
        assert!(session.hlc().is_some());

        let sub = ztimeout!(session.declare_subscriber("test/ext/frag/ts").advanced()).unwrap();
        let publ = ztimeout!(session
            .declare_publisher("test/ext/frag/ts")
            .advanced()
            .fragmentation(4)
            .cache(crate::CacheConfig::default().max_samples(10))
            .sample_miss_detection(crate::MissDetectionConfig::default()))
        .unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;

        ztimeout!(publ.put(PAYLOAD).attachment("attach")).unwrap();
        let delivered = ztimeout!(sub.recv_async()).unwrap();
        let ts_delivered = delivered.timestamp().copied();
        assert_eq!(
            delivered.attachment().unwrap().to_bytes().as_ref(),
            b"attach",
            "assembled sample must carry the attachment"
        );

        // Query the cache for fragment 0 of SN 0.
        let frags = Arc::new(Mutex::new(Vec::<(u32, Option<Timestamp>, bool)>::new()));
        let _ = ztimeout!(session
            .get(Selector::from((
                KeyExpr::try_from("test/ext/frag/ts/@adv/**").unwrap(),
                "_sn=0..0".to_string(),
            )))
            .callback({
                let frags = frags.clone();
                move |r: Reply| {
                    if let Ok(s) = r.into_result() {
                        if let Some(fi) = s.frag_info() {
                            frags.lock().unwrap().push((
                                fi.frag_num(),
                                s.timestamp().copied(),
                                s.attachment().is_some(),
                            ));
                        }
                    }
                }
            })
            .consolidation(ConsolidationMode::None)
            .accept_replies(ReplyKeyExpr::Any)
            .target(QueryTarget::All)
            .timeout(Duration::from_secs(10)));

        {
            let mut frags = frags.lock().unwrap();
            frags.sort_by_key(|(num, _, _)| *num);
            assert_eq!(frags.len(), 3, "expected all 3 fragments from cache");
            let (_, ts_frag0, att_frag0) = frags[0];
            assert_eq!(
                ts_delivered, ts_frag0,
                "assembled sample must carry fragment 0's timestamp"
            );
            assert!(ts_frag0.is_some(), "fragment 0 must be timestamped");
            assert!(att_frag0, "fragment 0 must carry the attachment");
            assert!(
                frags[1..].iter().all(|(_, ts, att)| ts.is_none() && !att),
                "fragments other than 0 must carry neither attachment nor publisher timestamp"
            );
        }

        drop((sub, publ));
        let _ = ztimeout!(session.close());
    }

    /// A fragmented sample without `source_info` cannot be reassembled: it
    /// must be dropped, with no state created and nothing delivered. A
    /// control sample with `source_info` must be accepted.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_orphan_fragment_dropped() {
        zenoh_util::init_log_from_env_or("error");
        let session = ztimeout!(zenoh::open(Config::default())).unwrap();

        let key_expr = KeyExpr::try_from("test/ext/frag/orphan").unwrap();
        let received = Arc::new(Mutex::new(Vec::<Sample>::new()));
        let received_cb = received.clone();
        let statesref = Arc::new_cyclic(|weak| {
            Mutex::new(State {
                next_id: 0,
                global_pending_queries: 0,
                sequenced_states: LruCache::unbounded(),
                timestamped_states: LruCache::unbounded(),
                session: session.downgrade(),
                key_expr: key_expr.clone().into_owned(),
                retransmission: false,
                frag_recovery_delay: RecoveryConfig::<true>::default().frag_recovery_delay,
                period: None,
                max_pending_samples: 10,
                query_target: QueryTarget::All,
                query_timeout: Duration::from_secs(10),
                max_fragments: MAX_FRAGMENTS_DEFAULT,
                callback: Some(Callback::from(move |s: Sample| {
                    received_cb.lock().unwrap().push(s);
                })),
                miss_handlers: HashMap::new(),
                token: None,
                _gc_task: AbortOnDropHandle::new(
                    ZRuntime::Application.spawn(gc_task(weak.clone(), Duration::from_secs(3600))),
                ),
            })
        });

        // Orphan fragment: `frag_info` set, no `source_info`, no timestamp.
        let orphan: Sample = SampleBuilder::put(key_expr.clone(), "4567")
            .frag_info(FragInfo::new(3, 1))
            .into();
        {
            let mut states = zlock!(statesref);
            let inserted = handle_sample(&mut states, orphan);
            assert!(!inserted.new_fragment_slot);
            assert!(
                states.sequenced_states.is_empty() && states.timestamped_states.is_empty(),
                "no state must be created for an orphan fragment"
            );
        }
        assert!(
            received.lock().unwrap().is_empty(),
            "orphan fragment must not be delivered"
        );

        // Control: the same fragment with `source_info` is accepted.
        let publ = session
            .declare_publisher("test/ext/frag/orphan/pub")
            .wait()
            .unwrap();
        let source_id = publ.id();
        let sequenced: Sample = SampleBuilder::put(key_expr.clone(), "4567")
            .frag_info(FragInfo::new(3, 1))
            .source_info(SourceInfo::new(source_id, 0))
            .into();
        {
            let mut states = zlock!(statesref);
            let inserted = handle_sample(&mut states, sequenced);
            assert!(inserted.new_fragment_slot);
            let state = states.sequenced_states.get_mut(&source_id).unwrap();
            assert_eq!(state.pending_samples.len(), 1);
        }

        drop(statesref);
        let _ = ztimeout!(session.close());
    }
}
