//
// Copyright (c) 2017, 2020 ADLINK Technology Inc.
//
// This program and the accompanying materials are made available under the
// terms of the Eclipse Public License 2.0 which is available at
// http://www.eclipse.org/legal/epl-2.0, or the Apache License, Version 2.0
// which is available at https://www.apache.org/licenses/LICENSE-2.0.
//
// SPDX-License-Identifier: EPL-2.0 OR Apache-2.0
//
// Contributors:
//   ADLINK zenoh team, <zenoh@adlink-labs.tech>
//

//! Publishing primitives.

use super::net::protocol::core::{key_expr, Channel};
use super::net::protocol::proto::{data_kind, DataInfo, Options};
use super::net::transport::Primitives;
use crate::prelude::*;
use crate::subscriber::Reliability;
use crate::sync::ZFuture;
use crate::Encoding;
use crate::Session;
use async_std::sync::Arc;
use std::fmt;
use std::sync::atomic::Ordering;
use zenoh_util::sync::Runnable;

/// The kind of congestion control.
pub use super::net::protocol::core::CongestionControl;

#[derive(Debug)]
pub(crate) struct PublisherState {
    pub(crate) id: Id,
    pub(crate) key_expr: KeyExpr<'static>,
}

/// A publisher.
///
/// Publishers are automatically unregistered when dropped.
pub struct Publisher<'a> {
    pub(crate) session: &'a Session,
    pub(crate) state: Arc<PublisherState>,
    pub(crate) alive: bool,
}

impl Publisher<'_> {
    /// Undeclare a [`Publisher`](Publisher) previously declared with [`publishing`](Session::publishing).
    ///
    /// Publishers are automatically unregistered when dropped, but you may want to use this function to handle errors or
    /// unregister the Publisher asynchronously.
    ///
    /// # Examples
    /// ```
    /// # async_std::task::block_on(async {
    /// use zenoh::prelude::*;
    ///
    /// let session = zenoh::open(config::peer()).await.unwrap();
    /// let publisher = session.publishing("/key/expression").await.unwrap();
    /// publisher.unregister().await.unwrap();
    /// # })
    /// ```
    #[inline]
    #[must_use = "ZFutures do nothing unless you `.wait()`, `.await` or poll them"]
    pub fn unregister(mut self) -> impl ZFuture<Output = ZResult<()>> {
        self.alive = false;
        self.session.unpublishing(self.state.id)
    }
}

impl Drop for Publisher<'_> {
    fn drop(&mut self) {
        if self.alive {
            let _ = self.session.unpublishing(self.state.id).wait();
        }
    }
}

impl fmt::Debug for Publisher<'_> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        self.state.fmt(f)
    }
}

derive_zfuture! {
    /// A builder for initializing a [`Publisher`](Publisher).
    ///
    /// The result of this builder can be accessed synchronously via [`wait()`](ZFuture::wait())
    /// or asynchronously via `.await`.
    ///
    /// # Examples
    /// ```
    /// # async_std::task::block_on(async {
    /// use zenoh::prelude::*;
    ///
    /// let session = zenoh::open(config::peer()).await.unwrap();
    /// let publisher = session.publishing("/key/expression").await.unwrap();
    /// # })
    /// ```
    #[derive(Debug, Clone)]
    pub struct PublisherBuilder<'a, 'b> {
        pub(crate) session: &'a Session,
        pub(crate) key_expr: KeyExpr<'b>,
    }
}

impl<'a> Runnable for PublisherBuilder<'a, '_> {
    type Output = ZResult<Publisher<'a>>;

    fn run(&mut self) -> Self::Output {
        log::trace!("publishing({:?})", self.key_expr);
        let mut state = zwrite!(self.session.state);
        let id = state.decl_id_counter.fetch_add(1, Ordering::SeqCst);
        let key_expr = state.localkey_to_expr(&self.key_expr)?;
        let pub_state = Arc::new(PublisherState {
            id,
            key_expr: self.key_expr.to_owned(),
        });
        let declared_pub = if let Some(join_pub) = state
            .join_publications
            .iter()
            .find(|s| key_expr::include(s, &key_expr))
        {
            let joined_pub = state.publishers.values().any(|p| {
                key_expr::include(join_pub, &state.localkey_to_expr(&p.key_expr).unwrap())
            });
            (!joined_pub).then(|| join_pub.clone().into())
        } else {
            let twin_pub = state.publishers.values().any(|p| {
                state.localkey_to_expr(&p.key_expr).unwrap()
                    == state.localkey_to_expr(&pub_state.key_expr).unwrap()
            });
            (!twin_pub).then(|| self.key_expr.clone())
        };

        state.publishers.insert(id, pub_state.clone());

        if let Some(res) = declared_pub {
            let primitives = state.primitives.as_ref().unwrap().clone();
            drop(state);
            primitives.decl_publisher(&res, None);
        }

        Ok(Publisher {
            session: self.session,
            state: pub_state,
            alive: true,
        })
    }
}

derive_zfuture! {
    /// A builder for initializing a `write` operation ([`put`](crate::Session::put) or [`delete`](crate::Session::delete)).
    ///
    /// The `write` operation can be run synchronously via [`wait()`](ZFuture::wait()) or asynchronously via `.await`.
    ///
    /// # Examples
    /// ```
    /// # async_std::task::block_on(async {
    /// use zenoh::prelude::*;
    /// use zenoh::publisher::CongestionControl;
    ///
    /// let session = zenoh::open(config::peer()).await.unwrap();
    /// session
    ///     .put("/key/expression", "value")
    ///     .encoding(Encoding::TEXT_PLAIN)
    ///     .congestion_control(CongestionControl::Block)
    ///     .await
    ///     .unwrap();
    /// # })
    /// ```
    #[derive(Debug, Clone)]
    pub struct Writer<'a> {
        pub(crate) session: &'a Session,
        pub(crate) key_expr: KeyExpr<'a>,
        pub(crate) value: Option<Value>,
        pub(crate) kind: Option<ZInt>,
        pub(crate) congestion_control: CongestionControl,
        pub(crate) priority: Priority,
    }
}

impl<'a> Writer<'a> {
    /// Change the `congestion_control` to apply when routing the data.
    #[inline]
    pub fn congestion_control(mut self, congestion_control: CongestionControl) -> Writer<'a> {
        self.congestion_control = congestion_control;
        self
    }

    /// Change the kind of the written data.
    #[inline]
    pub fn kind(mut self, kind: SampleKind) -> Self {
        self.kind = Some(kind as ZInt);
        self
    }

    /// Change the encoding of the written data.
    #[inline]
    pub fn encoding<IntoEncoding>(mut self, encoding: IntoEncoding) -> Self
    where
        IntoEncoding: Into<Encoding>,
    {
        if let Some(mut payload) = self.value.as_mut() {
            payload.encoding = encoding.into();
        } else {
            self.value = Some(Value::empty().encoding(encoding.into()));
        }
        self
    }

    /// Change the priority of the written data.
    #[inline]
    pub fn priority(mut self, priority: Priority) -> Writer<'a> {
        self.priority = priority;
        self
    }
}

impl Runnable for Writer<'_> {
    type Output = ZResult<()>;

    fn run(&mut self) -> Self::Output {
        log::trace!("write({:?}, [...])", self.key_expr);
        let state = zread!(self.session.state);
        let primitives = state.primitives.as_ref().unwrap().clone();
        drop(state);

        let value = self.value.take().unwrap();
        let mut info = DataInfo::new();
        info.kind = match self.kind {
            Some(data_kind::DEFAULT) => None,
            kind => kind,
        };
        info.encoding = if value.encoding != Encoding::default() {
            Some(value.encoding)
        } else {
            None
        };
        info.timestamp = self.session.runtime.new_timestamp();
        let data_info = if info.has_options() { Some(info) } else { None };

        primitives.send_data(
            &self.key_expr,
            value.payload.clone(),
            Channel {
                priority: self.priority.into(),
                reliability: Reliability::Reliable, // @TODO: need to check subscriptions to determine the right reliability value
            },
            self.congestion_control,
            data_info.clone(),
            None,
        );
        self.session
            .handle_data(true, &self.key_expr, data_info, value.payload);
        Ok(())
    }
}
