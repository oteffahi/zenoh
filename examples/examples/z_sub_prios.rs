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

use std::{collections::HashMap, time::Instant};

use clap::Parser;
use zenoh::{qos::Priority, Config, Wait};
use zenoh_examples::CommonArgs;

struct PrioStats {
    received: u64,
    lost: u64,
    last_seq: Option<u32>,
}

struct Stats {
    prios: HashMap<u8, PrioStats>,
    round_count: usize,
    round_size: usize,
    finished_rounds: usize,
    round_start: Instant,
    global_start: Option<Instant>,
}

impl Stats {
    fn new(round_size: usize) -> Self {
        Stats {
            prios: HashMap::new(),
            round_count: 0,
            round_size,
            finished_rounds: 0,
            round_start: Instant::now(),
            global_start: None,
        }
    }

    fn update(&mut self, prio: u8, seq: u32) {
        let p = self.prios.entry(prio).or_insert(PrioStats {
            received: 0,
            lost: 0,
            last_seq: None,
        });

        if let Some(last) = p.last_seq {
            let expected = last.wrapping_add(1);
            if seq != expected {
                let lost = seq.wrapping_sub(expected);
                p.lost += lost as u64;
            }
        }
        p.last_seq = Some(seq);
        p.received += 1;

        if self.round_count == 0 {
            self.round_start = Instant::now();
            if self.global_start.is_none() {
                self.global_start = Some(self.round_start);
            }
            self.round_count += 1;
        } else if self.round_count < self.round_size {
            self.round_count += 1;
        } else {
            self.print_round();
            self.finished_rounds += 1;
            self.round_count = 0;
        }
    }

    fn print_round(&self) {
        let elapsed = self.round_start.elapsed().as_secs_f64();
        let throughput = (self.round_size as f64) / elapsed;
        println!(
            "Round {} ({throughput:.0} msg/s):",
            self.finished_rounds + 1
        );
        let mut prios: Vec<_> = self.prios.iter().collect();
        prios.sort_by_key(|(k, _)| *k);
        for (&prio, stats) in &prios {
            println!(
                "  {:<16} recv={:<10} lost={:<10} last_seq={}",
                prio_name(prio),
                stats.received,
                stats.lost,
                stats.last_seq.map_or(0, |s| s)
            );
        }
    }
}

impl Drop for Stats {
    fn drop(&mut self) {
        let Some(global_start) = self.global_start else {
            return;
        };
        let elapsed = global_start.elapsed().as_secs_f64();
        let total = self.round_size * self.finished_rounds + self.round_count;
        let throughput = total as f64 / elapsed;
        println!("Received {total} messages over {elapsed:.2}s: {throughput:.0} msg/s");
        println!("Final per-priority stats:");
        let mut prios: Vec<_> = self.prios.iter().collect();
        prios.sort_by_key(|(k, _)| *k);
        for (&prio, stats) in &prios {
            println!(
                "  {:<16} recv={}  lost={}",
                prio_name(prio),
                stats.received,
                stats.lost
            );
        }
    }
}

fn prio_name(val: u8) -> &'static str {
    Priority::try_from(val)
        .map(|p| match p {
            Priority::RealTime => "RealTime",
            Priority::InteractiveHigh => "InteractiveHigh",
            Priority::InteractiveLow => "InteractiveLow",
            Priority::DataHigh => "DataHigh",
            Priority::Data => "Data",
            Priority::DataLow => "DataLow",
            Priority::Background => "Background",
        })
        .unwrap_or("Unknown")
}

fn main() {
    zenoh::init_log_from_env_or("error");

    let (config, m, n) = parse_args();

    let session = zenoh::open(config).wait().unwrap();

    let key_expr = "test/prio/**";

    let mut stats = Stats::new(n);
    session
        .declare_subscriber(key_expr)
        .callback_mut(move |sample| {
            let payload = sample.payload().to_bytes();
            if payload.len() < 4 {
                return;
            }
            let prio = sample
                .key_expr()
                .as_str()
                .rsplit('/')
                .next()
                .and_then(|s| s.parse::<u8>().ok());
            let Some(prio) = prio else {
                return;
            };
            let seq = u32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]]);
            stats.update(prio, seq);
            if stats.finished_rounds >= m {
                std::process::exit(0);
            }
        })
        .background()
        .wait()
        .unwrap();

    println!("Press CTRL-C to quit...");
    std::thread::park();
}

#[derive(clap::Parser, Clone, PartialEq, Eq, Hash, Debug)]
struct Args {
    #[arg(short, long, default_value = "10")]
    /// Number of throughput measurements.
    samples: usize,
    #[arg(short, long, default_value = "100000")]
    /// Number of messages in each throughput measurements.
    number: usize,
    #[command(flatten)]
    common: CommonArgs,
}

fn parse_args() -> (Config, usize, usize) {
    let args = Args::parse();
    (args.common.into(), args.samples, args.number)
}
