//
// Copyright (c) 2024 ZettaScale Technology
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
use std::net::SocketAddr;

use tokio::net::{TcpListener, TcpSocket, TcpStream};
use zenoh_result::{zerror, ZResult};

pub struct TcpSocketUtils;

impl TcpSocketUtils {
    /// Build a new TCP listener bound to `addr` with the given configration parameters
    pub fn listen(addr: &SocketAddr, iface: Option<&str>) -> ZResult<(TcpListener, SocketAddr)> {
        let socket = match addr {
            SocketAddr::V4(_) => TcpSocket::new_v4(),
            SocketAddr::V6(_) => TcpSocket::new_v6(),
        }?;

        if let Some(iface) = iface {
            zenoh_util::net::set_bind_to_device_tcp_socket(&socket, iface)?;
        }

        // Build a TcpListener from TcpSocket
        // https://docs.rs/tokio/latest/tokio/net/struct.TcpSocket.html
        socket.set_reuseaddr(true)?;
        socket.bind(*addr).map_err(|e| zerror!("{}: {}", addr, e))?;
        // backlog (the maximum number of pending connections are queued): 1024
        let listener = socket
            .listen(1024)
            .map_err(|e| zerror!("{}: {}", addr, e))?;

        let local_addr = listener
            .local_addr()
            .map_err(|e| zerror!("{}: {}", addr, e))?;

        Ok((listener, local_addr))
    }

    /// Connect to a TCP socket address at `dst_addr` with the given configuration parameters
    pub async fn connect(
        dst_addr: &SocketAddr,
        iface: Option<&str>,
    ) -> ZResult<(TcpStream, SocketAddr, SocketAddr)> {
        let socket = match dst_addr {
            SocketAddr::V4(_) => TcpSocket::new_v4(),
            SocketAddr::V6(_) => TcpSocket::new_v6(),
        }?;

        if let Some(iface) = iface {
            zenoh_util::net::set_bind_to_device_tcp_socket(&socket, iface)?;
        }

        // Build a TcpStream from TcpSocket
        // https://docs.rs/tokio/latest/tokio/net/struct.TcpSocket.html
        let stream = socket
            .connect(*dst_addr)
            .await
            .map_err(|e| zerror!("{}: {}", dst_addr, e))?;

        let src_addr = stream
            .local_addr()
            .map_err(|e| zerror!("{}: {}", dst_addr, e))?;

        let dst_addr = stream
            .peer_addr()
            .map_err(|e| zerror!("{}: {}", dst_addr, e))?;

        Ok((stream, src_addr, dst_addr))
    }
}
