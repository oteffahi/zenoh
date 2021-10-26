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
use async_std::task::sleep;
use clap::{App, Arg};
use std::time::Duration;
use zenoh::prelude::*;

#[async_std::main]
async fn main() {
    // Initiate logging
    env_logger::init();

    let (config, path, value) = parse_args();

    println!("Open session");
    let session = zenoh::open(config).await.unwrap();

    print!("Register key expression {}", path);
    let expr_id = session.register_expr(&path).await.unwrap();
    println!(" => ExprId {}", expr_id);

    println!("Register Publisher on {}", expr_id);
    let _publisher = session.publishing(expr_id).await.unwrap();

    for idx in 0..u32::MAX {
        sleep(Duration::from_secs(1)).await;
        let buf = format!("[{:4}] {}", idx, value);
        println!("Put Data ('{}': '{}')", expr_id, buf);
        session.put(expr_id, buf).await.unwrap();
    }
}

fn parse_args() -> (Properties, String, String) {
    let args = App::new("zenoh pub example")
        .arg(
            Arg::from_usage("-m, --mode=[MODE] 'The zenoh session mode (peer by default).")
                .possible_values(&["peer", "client"]),
        )
        .arg(Arg::from_usage(
            "-e, --peer=[LOCATOR]...  'Peer locators used to initiate the zenoh session.'",
        ))
        .arg(Arg::from_usage(
            "-l, --listener=[LOCATOR]...   'Locators to listen on.'",
        ))
        .arg(
            Arg::from_usage("-p, --path=[PATH]        'The key expression to publish onto.'")
                .default_value("/demo/example/zenoh-rs-pub"),
        )
        .arg(
            Arg::from_usage("-v, --value=[VALUE]      'The value to publish.'")
                .default_value("Pub from Rust!"),
        )
        .arg(Arg::from_usage(
            "-c, --config=[FILE]      'A configuration file.'",
        ))
        .get_matches();

    let mut config = if let Some(conf_file) = args.value_of("config") {
        Properties::from(std::fs::read_to_string(conf_file).unwrap())
    } else {
        Properties::default()
    };
    for key in ["mode", "peer", "listener"].iter() {
        if let Some(value) = args.values_of(key) {
            config.insert(key.to_string(), value.collect::<Vec<&str>>().join(","));
        }
    }
    if args.is_present("no-multicast-scouting") {
        config.insert("multicast_scouting".to_string(), "false".to_string());
    }

    let path = args.value_of("path").unwrap();
    let value = args.value_of("value").unwrap();

    (config, path.to_string(), value.to_string())
}
