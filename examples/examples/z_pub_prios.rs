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

use clap::Parser;
use zenoh::{
    bytes::ZBytes,
    qos::{CongestionControl, Priority},
    Wait,
};
use zenoh_examples::CommonArgs;

fn main() {
    zenoh::init_log_from_env_or("error");
    let args = Args::parse();

    let payload_size = args.payload_size.max(4);
    let session = zenoh::open(args.common).wait().unwrap();

    let publishers: Vec<(u8, _)> = (1u8..=7u8)
        .map(|pv| {
            let prio = Priority::try_from(pv).unwrap();
            let key_expr = format!("test/prio/{}", pv);
            let publisher = session
                .declare_publisher(key_expr)
                .congestion_control(CongestionControl::Drop)
                .priority(prio)
                .wait()
                .unwrap();
            (pv, publisher)
        })
        .collect();

    let mut handles = Vec::new();
    for (_prio_value, publisher) in publishers {
        handles.push(std::thread::spawn(move || {
            let mut seq: u32 = 0;
            loop {
                let mut buf = vec![0u8; payload_size];
                buf[..4].copy_from_slice(&seq.to_le_bytes());
                let data: ZBytes = buf.into();
                publisher.put(data).wait().unwrap();
                seq = seq.wrapping_add(1);
            }
        }));
    }

    println!("Press CTRL-C to quit...");
    for h in handles {
        let _ = h.join();
    }
}

#[derive(Parser, Clone, PartialEq, Eq, Hash, Debug)]
struct Args {
    /// Sets the size of the payload to publish
    payload_size: usize,
    #[command(flatten)]
    common: CommonArgs,
}
