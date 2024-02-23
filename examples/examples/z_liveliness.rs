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
use async_std::task::sleep;
use clap::Parser;
use std::time::Duration;
use zenoh::config::Config;
use zenoh::prelude::r#async::*;
use zenoh_examples::CommonArgs;

#[async_std::main]
async fn main() {
    // Initiate logging
    env_logger::init();

    let (config, key_expr) = parse_args();

    println!("Opening session...");
    let session = zenoh::open(config).res().await.unwrap();

    println!("Declaring LivelinessToken on '{}'...", &key_expr);
    let _token = Some(
        session
            .liveliness()
            .declare_token(&key_expr)
            .res()
            .await
            .unwrap(),
    );

    println!("Press CTRL-C to undeclare LivelinessToken and quit...");
    loop {
        sleep(Duration::from_secs(1)).await;
    }
    // LivelinessTokens are automatically closed when dropped
    // Use the code below to manually undeclare it if needed
    //
    // if let Some(token) = token.take() {
    //     println!("Undeclaring LivelinessToken...");
    //     token.undeclare().res().await.unwrap();
    // }
}

#[derive(clap::Parser, Clone, PartialEq, Eq, Hash, Debug)]
struct Args {
    #[arg(short, long, default_value = "group1/zenoh-rs")]
    /// The key expression of the liveliness token.
    key: KeyExpr<'static>,
    #[command(flatten)]
    common: CommonArgs,
}

fn parse_args() -> (Config, KeyExpr<'static>) {
    let args = Args::parse();
    (args.common.into(), args.key)
}
