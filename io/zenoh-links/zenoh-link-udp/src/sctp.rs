use core::fmt;
use std::{net::SocketAddr, sync::Arc};

use async_trait::async_trait;
use tokio::sync::Semaphore;
use tokio_util::{bytes::Bytes, sync::CancellationToken};
use webrtc_sctp::{
    association::Association, chunk::chunk_payload_data::PayloadProtocolIdentifier, stream::Stream,
};
use zenoh_core::bail;
use zenoh_link_commons::{LinkAuthId, LinkUnicast, LinkUnicastTrait, NewLinkChannelSender};
use zenoh_protocol::{core::Locator, transport::BatchSize};
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
        self.read(buf)
            .await
            .map_err(|e| webrtc_util::Error::Other(e.to_string()))
    }

    async fn recv_from(&self, buf: &mut [u8]) -> WebRtcUtilResult<(usize, SocketAddr)> {
        Ok((self.recv(buf).await?, self.dst_addr))
    }

    async fn send(&self, buf: &[u8]) -> WebRtcUtilResult<usize> {
        self.write(buf)
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
    sctp_association: Association,
    // single stream for now
    stream: Arc<Stream>,
}

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
            let sctp_association = Association::server(config)
                .await
                .map_err(|e| format!("failed to create SCTP association: {e}"))?;
            // server accepts a stream initiated by the client
            let stream = sctp_association
                .accept_stream()
                .await
                .ok_or_else(|| zerror!("failed to accept SCTP stream"))?;
            stream.set_default_payload_type(PayloadProtocolIdentifier::Binary);
            stream.set_reliability_params(false, webrtc_sctp::stream::ReliabilityType::Reliable, 0);

            ZResult::Ok(Self {
                udp_link,
                sctp_association,
                stream,
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
            let sctp_association = Association::client(config)
                .await
                .map_err(|e| format!("failed to create SCTP association: {e}"))?;

            // as client, open stream to initiate Zenoh handshake
            let stream = sctp_association
                .open_stream(0, PayloadProtocolIdentifier::Binary)
                .await
                .map_err(|e| format!("failed to open SCTP stream: {e}"))?;
            stream.set_reliability_params(false, webrtc_sctp::stream::ReliabilityType::Reliable, 0);

            ZResult::Ok(Self {
                udp_link,
                sctp_association,
                stream,
            })
        };
        tokio::select! {
            _ = tokio::time::sleep(tokio::time::Duration::from_millis(SCTP_ESTABLISHMENT_TIMEOUT_MS)) => bail!("timeout!"),
            res = open_sctp => res,
        }
        .map_err(|e| zerror!("Could not open SCTP-over-UDP connection: {e}").into())
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

    async fn write(&self, buffer: &[u8]) -> ZResult<usize> {
        // This copy is necessary, calls to write return before finishing to write on the wire
        Ok(self.stream.write(&Bytes::copy_from_slice(buffer)).await?)
    }

    async fn write_all(&self, buffer: &[u8]) -> ZResult<()> {
        let mut written: usize = 0;
        while written < buffer.len() {
            written += self.write(&buffer[written..]).await?;
        }
        Ok(())
    }

    async fn read(&self, buffer: &mut [u8]) -> ZResult<usize> {
        Ok(self.stream.read(buffer).await?)
    }

    async fn read_exact(&self, buffer: &mut [u8]) -> ZResult<()> {
        let mut read: usize = 0;
        while read < buffer.len() {
            let n = self.stream.read(&mut buffer[read..]).await?;
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
