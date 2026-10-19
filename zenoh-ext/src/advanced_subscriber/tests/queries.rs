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
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use zenoh::{
    bytes::ZBytes,
    internal::{zlock, ztimeout},
    key_expr::KeyExpr,
    sample::{FragInfo, Sample, SampleBuilder, SourceInfo},
    session::EntityGlobalId,
    Config, Wait,
};

use super::QueryContext;
use crate::{
    advanced_subscriber::{handle_sample, tests::frag_recovery_state},
    utils::WrappingSn,
    AdvancedSubscriberBuilderExt, HistoryConfig, RecoveryConfig,
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
        builder
            .recovery(RecoveryConfig::default().fragments_recovery_delay(Duration::from_secs(60)))
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
