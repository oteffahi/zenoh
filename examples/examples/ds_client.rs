//
// Copyright (c) 2023 ZettaScale Technology
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
use std::time::Duration;

use clap::Parser;
use zenoh::{
    config::ZenohId,
    query::{QueryTarget, Selector},
    Config,
};
use zenoh_examples::CommonArgs;

#[tokio::main]
async fn main() {
    // initiate logging
    zenoh::init_log_from_env_or("error");

    let (config, _, payload, _, timeout) = parse_args();

    println!("Opening session...");
    let session = zenoh::open(config).await.unwrap();

    // connected peers' ZIDs
    let peers: Vec<ZenohId> = session.info().peers_zid().await.collect();
    if peers.is_empty() {
        println!("No peers in the network");
        return;
    }
    // to avoid broadcasting the same file multiple times, need to have a way to target only one queryable in the network
    let query_target = peers.get(0).unwrap().to_string();
    let selector = format!("DS/STORE/{query_target}");
    println!("Sending Query '{selector}'...");
    let replies = session
        .get(&selector)
        .payload(payload.unwrap_or_default())
        .timeout(timeout)
        .await
        .unwrap();
    while let Ok(reply) = replies.recv_async().await {
        match reply.result() {
            Ok(sample) => {
                // Refer to z_bytes.rs to see how to deserialize different types of message
                let payload = sample
                    .payload()
                    .deserialize::<String>()
                    .unwrap_or_else(|e| format!("{}", e));
                println!(
                    ">> Received ('{}': '{}')",
                    sample.key_expr().as_str(),
                    payload,
                );
            }
            Err(err) => {
                let payload = err
                    .payload()
                    .deserialize::<String>()
                    .unwrap_or_else(|e| format!("{}", e));
                println!(">> Received (ERROR: '{}')", payload);
            }
        }
    }
}

#[derive(clap::ValueEnum, Clone, Copy, Debug)]
#[value(rename_all = "SCREAMING_SNAKE_CASE")]
enum Qt {
    BestMatching,
    All,
    AllComplete,
}

#[derive(Parser, Clone, Debug)]
struct Args {
    #[arg(short, long, default_value = "demo/example/**")]
    /// The selection of resources to query
    selector: Selector<'static>,
    #[arg(short, long)]
    /// An optional payload to put in the query.
    payload: Option<String>,
    #[arg(short, long, default_value = "BEST_MATCHING")]
    /// The target queryables of the query.
    target: Qt,
    #[arg(short = 'o', long, default_value = "10000")]
    /// The query timeout in milliseconds.
    timeout: u64,
    #[command(flatten)]
    common: CommonArgs,
}

fn parse_args() -> (
    Config,
    Selector<'static>,
    Option<String>,
    QueryTarget,
    Duration,
) {
    let args = Args::parse();
    (
        args.common.into(),
        args.selector,
        args.payload,
        match args.target {
            Qt::BestMatching => QueryTarget::BestMatching,
            Qt::All => QueryTarget::All,
            Qt::AllComplete => QueryTarget::AllComplete,
        },
        Duration::from_millis(args.timeout),
    )
}
