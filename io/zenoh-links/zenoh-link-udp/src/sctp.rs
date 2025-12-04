use core::fmt;
use std::{cell::UnsafeCell, collections::HashMap, net::SocketAddr, sync::Arc};

use async_trait::async_trait;
use tokio::sync::{oneshot, Semaphore};
use tokio_util::{bytes::Bytes, sync::CancellationToken};
use webrtc_sctp::{
    association::Association, chunk::chunk_payload_data::PayloadProtocolIdentifier, stream::Stream,
};
use zenoh_core::bail;
use zenoh_link_commons::{LinkAuthId, LinkUnicast, LinkUnicastTrait, NewLinkChannelSender};
use zenoh_protocol::{
    core::{Locator, Priority},
    transport::BatchSize,
};
use zenoh_result::{zerror, ZResult};

use crate::LinkUnicastUdp;

/// Timeout for pending SCTP handshake and stream establishments
const SCTP_ESTABLISHMENT_TIMEOUT_MS: u64 = 2000;
/// Max number of pending SCTP connections per listener
pub(crate) const SCTP_MAX_CONCURRENT_ACCEPT: usize = 100;

// Implement Conn trait for LinkUnicastUdp so it cas serve as transport for SCTP
type WebRtcUtilResult<T> = std::result::Result<T, webrtc_util::Error>;
#[async_trait]
impl webrtc_util::Conn for LinkUnicastUdp {
    async fn connect(&self, _addr: SocketAddr) -> WebRtcUtilResult<()> {
        Err(webrtc_util::Error::Other("Not applicable".to_owned()))
    }

    async fn recv(&self, buf: &mut [u8]) -> WebRtcUtilResult<usize> {
        self.read(buf, Priority::Control)
            .await
            .map_err(|e| webrtc_util::Error::Other(e.to_string()))
    }

    async fn recv_from(&self, buf: &mut [u8]) -> WebRtcUtilResult<(usize, SocketAddr)> {
        Ok((self.recv(buf).await?, self.dst_addr))
    }

    async fn send(&self, buf: &[u8]) -> WebRtcUtilResult<usize> {
        self.write(buf, Priority::Control)
            .await
            .map_err(|e| webrtc_util::Error::Other(e.to_string()))
    }

    async fn send_to(&self, _buf: &[u8], _target: SocketAddr) -> WebRtcUtilResult<usize> {
        Err(webrtc_util::Error::Other("Not applicable".to_owned()))
    }

    fn local_addr(&self) -> WebRtcUtilResult<SocketAddr> {
        Ok(self.src_addr)
    }

    fn remote_addr(&self) -> Option<SocketAddr> {
        Some(self.dst_addr)
    }

    async fn close(&self) -> WebRtcUtilResult<()> {
        (self as &dyn LinkUnicastTrait)
            .close()
            .await
            .map_err(|e| webrtc_util::Error::Other(e.to_string()))
    }

    fn as_any(&self) -> &(dyn std::any::Any + Send + Sync) {
        self
    }
}

/// Wrapper around UDP link which adds SCTP reliability and stream-multiplexing
pub(crate) struct LinkUnicastSctp {
    udp_link: Arc<LinkUnicastUdp>,
    sctp_association: Arc<Association>,
    send: [Arc<Stream>; Priority::NUM],
    recv: [UnsafeCell<Option<RecvStream>>; Priority::NUM],
}

unsafe impl Sync for LinkUnicastSctp {}

impl LinkUnicastSctp {
    pub(crate) fn spawn_acceptor(
        udp_link: LinkUnicastUdp,
        manager: NewLinkChannelSender,
        token: CancellationToken,
        sctp_acceptors: Arc<Semaphore>,
        handle: tokio::runtime::Handle,
    ) {
        let acceptor_task = async move {
            let _permit = sctp_acceptors.acquire().await?;

            let udp_link = Arc::new(udp_link);
            let config = webrtc_sctp::association::Config {
                net_conn: udp_link.clone(),
                // set to 0 to use library defaults
                max_receive_buffer_size: 0,
                max_message_size: 0,
                // following fields are only relevant for logging
                name: "zenoh_server".to_owned(),
                local_port: udp_link.src_addr.port(),
                remote_port: udp_link.dst_addr.port(),
            };
            let sctp_association = Arc::new(
                Association::server(config)
                    .await
                    .map_err(|e| format!("failed to create SCTP association: {e}"))?,
            );
            // server accepts a stream initiated by the client
            let control_stream = sctp_association
                .accept_stream()
                .await
                .ok_or_else(|| zerror!("failed to accept SCTP stream"))?;
            control_stream.set_default_payload_type(PayloadProtocolIdentifier::Binary);
            control_stream.set_reliability_params(
                false,
                webrtc_sctp::stream::ReliabilityType::Reliable,
                0,
            );

            let (send, recv) =
                Self::init_multistream(&sctp_association, control_stream, false).await?;

            ZResult::Ok(Self {
                udp_link,
                sctp_association,
                send: send.try_into().expect(
                    "number of outgoing streams should be equal to number of message priorities",
                ),
                recv: recv.try_into().unwrap(),
            })
        };
        let accept_with_timeout = async {
            tokio::select! {
                _ = tokio::time::sleep(tokio::time::Duration::from_millis(SCTP_ESTABLISHMENT_TIMEOUT_MS)) => bail!("timeout!"),
                res = acceptor_task => res,
            }
        };
        handle.spawn(async move {
            tokio::select! {
                _ = token.cancelled() => {},
                result = accept_with_timeout => {
                    match result {
                        Ok(link) => if let Err(e) = manager.send_async(LinkUnicast(Arc::new(link))).await {
                            tracing::error!("{}-{}: {}", file!(), line!(), e)
                        },
                        Err(e) => tracing::error!("Could not accept SCTP-over-UDP connection: {e}"),
                    }
                },
            }
        });
    }

    pub(crate) async fn open(udp_link: LinkUnicastUdp) -> ZResult<Self> {
        let udp_link = Arc::new(udp_link);
        let config = webrtc_sctp::association::Config {
            net_conn: udp_link.clone(),
            // set to 0 to use library defaults
            max_receive_buffer_size: 0,
            max_message_size: 0,
            // following fields are only relevant for logging
            name: "zenoh_client".to_owned(),
            local_port: udp_link.src_addr.port(),
            remote_port: udp_link.dst_addr.port(),
        };
        let open_sctp = async {
            let sctp_association = Arc::new(
                Association::client(config)
                    .await
                    .map_err(|e| format!("failed to create SCTP association: {e}"))?,
            );

            // as client, open stream to initiate Zenoh handshake
            let control_stream = sctp_association
                .open_stream(0, PayloadProtocolIdentifier::Binary)
                .await
                .map_err(|e| format!("failed to open SCTP stream: {e}"))?;
            control_stream.set_reliability_params(
                false,
                webrtc_sctp::stream::ReliabilityType::Reliable,
                0,
            );

            let (send, recv) =
                Self::init_multistream(&sctp_association, control_stream, true).await?;

            ZResult::Ok(Self {
                udp_link,
                sctp_association,
                send: send.try_into().expect(
                    "number of outgoing streams should be equal to number of message priorities",
                ),
                recv: recv.try_into().unwrap(),
            })
        };
        tokio::select! {
            _ = tokio::time::sleep(tokio::time::Duration::from_millis(SCTP_ESTABLISHMENT_TIMEOUT_MS)) => bail!("timeout!"),
            res = open_sctp => res,
        }
        .map_err(|e| zerror!("Could not open SCTP-over-UDP connection: {e}").into())
    }

    async fn init_multistream(
        sctp_association: &Arc<Association>,
        control_stream: Arc<Stream>,
        self_is_client: bool,
    ) -> ZResult<(Vec<Arc<Stream>>, Vec<UnsafeCell<Option<RecvStream>>>)> {
        let mut send = vec![control_stream.clone()];
        let mut recv = vec![UnsafeCell::new(Some(RecvStream::Accepted(control_stream)))];
        let mut priority_txs = HashMap::new();
        // For each priority, creates a channel to notify the acceptation and initialize
        // the stream to pending
        for prio in 1..Priority::NUM {
            let (tx, rx) = oneshot::channel();
            priority_txs.insert(prio, tx);
            recv.push(UnsafeCell::new(Some(RecvStream::Pending(rx))));
        }
        open_streams(&sctp_association, &mut send, self_is_client).await?;
        tokio::task::spawn(RecvStream::acceptor_task(
            sctp_association.clone(),
            priority_txs,
            !self_is_client,
        ));
        Ok((send, recv))
    }

    /// Retrieved the read-stream mapped to the priority
    ///
    /// The stream may be pending, in which case we wait until it is accepted.
    ///
    /// # Safety
    ///
    /// There should be only one caller per priority.
    #[allow(clippy::mut_from_ref)]
    async unsafe fn read_stream(&self, priority: Priority) -> ZResult<Arc<Stream>> {
        match unsafe { &mut *self.recv[priority as usize].get() }
            .as_mut()
            .expect("multistream should have been started")
        {
            stream @ RecvStream::Pending(_) => {
                let RecvStream::Pending(rx) = stream else {
                    unreachable!()
                };
                let recv = rx.await.map_err(|_| zerror!("Connection closed"))?;
                *stream = RecvStream::Accepted(recv);
                let RecvStream::Accepted(recv) = stream else {
                    unreachable!()
                };
                Ok(recv.clone())
            }
            RecvStream::Accepted(recv) => Ok(recv.clone()),
        }
    }
}

// All IO should be performed on SCTP stream objects.
// Other trait methods are forwarded to underlying UDP link
#[async_trait]
impl LinkUnicastTrait for LinkUnicastSctp {
    async fn close(&self) -> ZResult<()> {
        // closing the association closes the underlying LinkUnicastUdp
        Ok(self.sctp_association.close().await?)
    }

    async fn write(&self, buffer: &[u8], priority: Priority) -> ZResult<usize> {
        // This copy is necessary, calls to write return before finishing to write on the wire
        let buff = Bytes::copy_from_slice(buffer);
        Ok(self.send[priority as usize].write(&buff).await?)
    }

    async fn write_all(&self, buffer: &[u8], priority: Priority) -> ZResult<()> {
        let mut written: usize = 0;
        while written < buffer.len() {
            written += self.write(&buffer[written..], priority).await?;
        }
        Ok(())
    }

    async fn read(&self, buffer: &mut [u8], priority: Priority) -> ZResult<usize> {
        let stream = unsafe { self.read_stream(priority).await? };
        Ok(stream.read(buffer).await?)
    }

    async fn read_exact(&self, buffer: &mut [u8], priority: Priority) -> ZResult<()> {
        let stream = unsafe { self.read_stream(priority).await? };
        let mut read: usize = 0;
        while read < buffer.len() {
            let n = stream.read(&mut buffer[read..]).await?;
            read += n;
        }
        Ok(())
    }

    #[inline(always)]
    fn get_src(&self) -> &Locator {
        self.udp_link.get_src()
    }

    #[inline(always)]
    fn get_dst(&self) -> &Locator {
        self.udp_link.get_dst()
    }

    #[inline(always)]
    fn get_mtu(&self) -> BatchSize {
        // TODO: check MTU computation for SCTP based on underlying UDP
        self.udp_link.get_mtu()
    }

    #[inline(always)]
    fn get_interface_names(&self) -> Vec<String> {
        self.udp_link.get_interface_names()
    }

    #[inline(always)]
    fn is_reliable(&self) -> bool {
        // this SCTP link should be set to use reliable streams
        true
    }

    #[inline(always)]
    fn is_streamed(&self) -> bool {
        // SCTP is message-based stream multiplexing. Messages are delimited,
        // therefore it is not streamed as per Zenoh's definition of a streamed link
        false
    }

    #[inline(always)]
    fn supports_priorities(&self) -> bool {
        true
    }

    #[inline(always)]
    fn get_auth_id(&self) -> &LinkAuthId {
        self.udp_link.get_auth_id()
    }
}

// display as UDP link
impl fmt::Display for LinkUnicastSctp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.udp_link.fmt(f)
    }
}

impl fmt::Debug for LinkUnicastSctp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Sctp-Udp")
            .field("src", &self.udp_link.src_addr)
            .field("dst", &self.udp_link.dst_addr)
            .finish()
    }
}

async fn open_streams(
    association: &Association,
    streams: &mut Vec<Arc<Stream>>,
    self_is_client: bool,
) -> ZResult<()> {
    let open_stream = |prio| async move {
        let open = association
            .open_stream(
                prio as u16 + get_stream_id_offset(self_is_client),
                PayloadProtocolIdentifier::Binary,
            )
            .await;
        ZResult::Ok(open.map_err(|e| zerror!("Cannot open stream: {e}"))?)
    };

    for i in 1..Priority::NUM {
        let s = open_stream(i).await?;
        streams.push(s);
    }
    Ok(())
}

/// A maybe-pending [`Stream`].
///
/// `webrtc_sctp` streams are only "accepted" when data is received, so they start with a "pending" state,
/// and are notified by [`RecvStream::acceptor_task`].
enum RecvStream {
    /// A pending channel waiting for [`RecvStream::acceptor_task`] notification.
    Pending(oneshot::Receiver<Arc<Stream>>),
    /// An accepted stream
    Accepted(Arc<Stream>),
}

impl RecvStream {
    /// Instantiate a task to accept incoming streams and notify the associated pending channel.
    ///
    /// Streams are mapped to their priority using their index, see [`UniStreams`].
    /// The task stop when all streams have been received, or with connection errors; there is no
    /// cancellation to handle as the connection will be closed eventually, triggering an error
    /// if the task is still alive.
    async fn acceptor_task(
        association: Arc<Association>,
        mut priority_txs: HashMap<usize, oneshot::Sender<Arc<Stream>>>,
        remote_is_client: bool,
    ) -> ZResult<()> {
        while !priority_txs.is_empty() {
            let stream = association
                .accept_stream()
                .await
                .ok_or_else(|| zerror!("Cannot accept SCTP stream"))?;
            let prio = stream
                .stream_identifier()
                .checked_sub(get_stream_id_offset(remote_is_client))
                .ok_or_else(|| zerror!("Incoming SCTP stream with invalid stream id"))?
                as usize;
            if let Some(tx) = priority_txs.remove(&prio) {
                tx.send(stream).ok();
            }
        }
        Ok(())
    }
}

fn get_stream_id_offset(is_client: bool) -> u16 {
    // 0:    client/server control stream
    // 1-7:  client streams prio=1-7
    // 8:    server stream prio=1
    // 9-15: server streams prio=1-7
    match is_client {
        true => 0,
        false => Priority::NUM as u16 - 1,
    }
}
