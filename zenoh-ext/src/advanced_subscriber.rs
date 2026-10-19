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
//! Advanced subscriber API and source-identity routing.
//!
//! Implementation responsibilities:
//! - `source`: arrival classification, buffered samples, ordering, and delivery.
//! - `recovery`: when to query and how query completion permits delivery.
//! - `queries`: request dispatch and reservation/reply lifetimes.
//! - `runtime`: subscriptions, heartbeat/liveliness callbacks, and retention.
//! - `crate::fragmentation`: one sample's fragment assembly and retry state.
//!
//! Publications and query replies use the same arrival path. Publications can
//! schedule immediate recovery after unlocking; query replies leave scheduling
//! to query completion. See the sibling modules for these ordering invariants.

use std::{fmt, future::IntoFuture, time::Instant};

use lru::LruCache;
use tokio_util::task::AbortOnDropHandle;
use zenoh::{
    handlers::{Callback, CallbackParameter, IntoHandler},
    internal::bail,
    key_expr::KeyExpr,
    liveliness::{LivelinessSubscriberBuilder, LivelinessToken},
    pubsub::{SubscriberBuilder, SubscriberUndeclaration},
    sample::{Locality, Sample},
    session::EntityGlobalId,
    Resolvable, Session, Wait, KE_ADV_PREFIX, KE_PUB, KE_STARSTAR,
};
#[zenoh_macros::unstable]
use {
    std::collections::HashMap,
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

use crate::fragmentation::{validate_fragment_metadata, FragInsertError, MAX_FRAGMENTS_DEFAULT};

mod queries;
mod recovery;
mod runtime;
mod source;
use source::{ArrivalContext, DeliveryContext, ReceiveOutcome, SequencedSource, TimestampedSource};

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
    ///
    /// Valid samples, fragments, and duplicates refresh the source's activity.
    /// Rejected fragments and samples already marked evicted or abandoned do not.
    /// Sources marked alive through publisher detection are retained while alive.
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
/// * fragment reassembly, bounded by [`max_fragments`](crate::AdvancedSubscriberBuilder::max_fragments)
///   and [`max_pending_samples`](crate::AdvancedSubscriberBuilder::max_pending_samples), with missing-fragment queries when recovery is enabled
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
    /// A new per-SN entry, not progress within an existing assembly.
    new_fragment_slot: bool,
}

fn rejected_fragment(sn: u32, error: FragInsertError) -> SampleInsertion {
    tracing::warn!("AdvancedSubscriber: rejected fragment (sn={sn}): {error:?}");
    SampleInsertion::default()
}

/// Route input by its ordering identity.
///
/// Stateful arrival processing belongs to the source; this boundary preserves
/// the source-cache access operations.
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
    if let Some(info) = sample.source_info().cloned() {
        let source_id = *info.source_id();
        let new_source = !states.sequenced_states.contains(&source_id);
        if new_source {
            // Reject malformed first fragments before allocating any source
            // state, changing the LRU, or starting recovery timers.
            if let Some(fi) = sample.frag_info() {
                if let Err(error) =
                    validate_fragment_metadata(fi.frag_num(), fi.frag_count(), states.max_fragments)
                {
                    return rejected_fragment(info.source_sn(), error);
                }
            }
            states
                .sequenced_states
                .put(source_id, SequencedSource::default());
        }
        let source = states.sequenced_states.peek_mut(&source_id).unwrap();
        let outcome = source.receive(
            sample,
            &info,
            &ArrivalContext {
                delivery: DeliveryContext {
                    callback,
                    miss_handlers: &states.miss_handlers,
                    source_id,
                },
                history_pending: states.global_pending_queries != 0,
                retransmission: states.retransmission,
                max_pending_samples: states.max_pending_samples,
                max_fragments: states.max_fragments,
            },
        );
        match outcome {
            Ok(ReceiveOutcome::Accepted { new_fragment_slot }) => {
                // Keep time and LRU order in sync: only valid activity touches
                // either, including valid duplicates with no new entry.
                source.latest_access = Instant::now();
                states.sequenced_states.promote(&source_id);
                SampleInsertion {
                    new_source,
                    new_fragment_slot,
                }
            }
            Ok(ReceiveOutcome::Ignored) => SampleInsertion::default(),
            Err(error) => rejected_fragment(info.source_sn(), error),
        }
    } else if let Some(timestamp) = sample.timestamp() {
        let source = states
            .timestamped_states
            .get_or_insert_mut(*timestamp.get_id(), Default::default);
        source.receive(
            sample,
            callback,
            states.global_pending_queries,
            states.max_pending_samples,
        );
        SampleInsertion::default()
    } else {
        callback.call(sample);
        SampleInsertion::default()
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
                    ZRuntime::Application.spawn(runtime::gc_task(weak.clone(), retention_period)),
                ),
            })
        });
        let subscriber =
            runtime::declare_publications(conf.session, &key_expr, conf.origin, &statesref)?;
        tracing::debug!("Create AdvancedSubscriber{{key_expr: {}}}", key_expr);
        if let Some(history) = conf.history.as_ref() {
            runtime::query_history(&statesref, &key_expr, history);
        }
        let liveliness_subscriber = conf
            .history
            .as_ref()
            .filter(|history| history.liveliness)
            .map(|history| {
                runtime::declare_liveliness(conf.session, &key_expr, &statesref, history)
            })
            .transpose()?;
        let heartbeat_subscriber = if retransmission.is_some_and(|r| r.heartbeat) {
            Some(runtime::declare_heartbeat(
                conf.session,
                &key_expr,
                conf.origin,
                &statesref,
            )?)
        } else {
            None
        };
        if conf.liveliness {
            runtime::declare_token(conf.session, &key_expr, &subscriber, &statesref, meta)?;
        }
        Ok(AdvancedSubscriber {
            statesref,
            subscriber,
            receiver,
            liveliness_subscriber,
            heartbeat_subscriber,
        })
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
//
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
#[path = "advanced_subscriber/tests/mod.rs"]
mod tests;
