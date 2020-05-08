/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/.
 *
 * Copyright 2020 Joyent, Inc.
 */

extern crate getopts;

mod fs;
mod queue;
mod s3;
mod state;
mod utils;
mod webdav;
mod worker;

use std::env;
use std::error::Error;
use std::sync::{mpsc::channel, mpsc::Sender, Arc, Mutex};
use std::vec::Vec;
use std::{thread, thread::JoinHandle};

use crate::queue::{Queue, QueueMode};
use crate::utils::*;
use crate::worker::{Worker, WorkerOptions};

use getopts::Options;

/* Default values. */
const DEF_CONCURRENCY: u32 = 1;
const DEF_SLEEP: u64 = 0;
const DEF_DISTR: &str = "128k,256k,512k";
const DEF_INTERVAL: u64 = 2;
const DEF_QUEUE_MODE: QueueMode = QueueMode::Rand;
const DEF_WORKLOAD: &str = "r,w";
const DEF_OUTPUT_FORMAT: &str = "h";

fn usage(opts: Options, msg: &str) {
    let synopsis = "\
                    Write files to a given target as quickly as possible";

    let usg = format!("chum - {}", synopsis);
    println!("{}", opts.usage(&usg));
    println!("{}", msg);
}

fn main() -> Result<(), Box<dyn Error>> {
    let args: Vec<String> = env::args().collect();
    let mut opts = Options::new();

    opts.reqopt("t", "target", "target server", "[s3|webdav|fs]:IP|PATH");

    opts.optopt(
        "c",
        "concurrency",
        &format!(
            "number of concurrent threads, \
             default: {}",
            DEF_CONCURRENCY
        ),
        "NUM",
    );
    opts.optopt(
        "s",
        "sleep",
        &format!(
            "sleep duration in millis between each \
             upload, default: {}",
            DEF_SLEEP
        ),
        "NUM",
    );
    opts.optopt(
        "d",
        "distribution",
        &format!(
            "comma-separated distribution of \
             file sizes to upload, default: {}",
            DEF_DISTR
        ),
        "NUM:COUNT,NUM:COUNT,...",
    );
    opts.optopt(
        "i",
        "interval",
        &format!(
            "interval in seconds at which to \
             report stats, default: {}",
            DEF_INTERVAL
        ),
        "NUM",
    );
    opts.optopt(
        "q",
        "queue-mode",
        &format!(
            "queue mode for read operations, default: {}",
            DEF_QUEUE_MODE
        ),
        &format!("{}|{}|{}", QueueMode::Lru, QueueMode::Mru, QueueMode::Rand),
    );
    opts.optopt(
        "w",
        "workload",
        &format!("workload of operations, default: {}", DEF_WORKLOAD),
        "OP:COUNT,OP:COUNT",
    );
    opts.optopt(
        "f",
        "format",
        &format!("statistics output format, default: {}", DEF_OUTPUT_FORMAT),
        "h|v|t",
    );
    opts.optopt(
        "m",
        "max-data",
        "maximum amount of data to write to the target, \
         default: none, '0' disables cap",
        "CAP",
    );
    opts.optopt(
        "r",
        "read-list",
        "path to a file listing files to read from server, \
         default: none (files are chosen from recent uploads)",
        "FILE",
    );
    opts.optopt(
        "p",
        "percentage",
        "fill the given filesystem path to a given percentage capacity",
        "NUM",
    );

    opts.optflag("h", "help", "print this help message");
    opts.optflag(
        "D",
        "debug",
        "enable verbose statemap tracing (may impact performance)\n\
         Must be used with the -m flag",
    );
    opts.optflag("", "no-sync", "disable synchronous writes");

    let matches = match opts.parse(&args[1..]) {
        Ok(m) => m,
        Err(f) => {
            usage(opts, &f.to_string());
            return Ok(());
        }
    };

    if matches.opt_present("h") {
        usage(opts, "");
        return Ok(());
    }

    let (tx, rx) = channel();
    let mut debug_tx: Option<Sender<state::State>> = None;

    let smap_thread = if matches.opt_present("D") {
        /*
         * The statemap format isn't a streaming format, so we need the states
         * to stop coming (i.e. the program ends) at some point. The only ways
         * to end the program are to:
         * - use a data cap
         * - send a signal
         *
         * I don't want to go through the signal handling dance, so a data cap
         * is the only way to end manta-chum in a quiescent manner.
         */
        if !matches.opt_present("m") {
            usage(opts, "-D flag must be used with -m flag");
            return Ok(());
        }

        debug_tx = Some(tx);
        Some(thread::spawn(move || {
            state::state_listener(rx);
        }))
    } else {
        None
    };

    /* Handle grabbing defaults if the user didn't provide these flags. */
    let conc = matches.opt_get_default("concurrency", DEF_CONCURRENCY)?;
    let sleep = matches.opt_get_default("sleep", DEF_SLEEP)?;
    let interval = matches.opt_get_default("interval", DEF_INTERVAL)?;
    let qmode = matches.opt_get_default("queue-mode", DEF_QUEUE_MODE)?;
    let format =
        matches.opt_get_default("format", String::from(DEF_OUTPUT_FORMAT))?;

    let format = match format.as_str() {
        "h" => OutputFormat::Human,
        "v" => OutputFormat::HumanVerbose,
        "t" => OutputFormat::Tabular,
        _ => {
            usage(opts, &format!("invalid output format '{}'", format));
            return Ok(());
        }
    };

    let ops = if matches.opt_present("workload") {
        matches.opt_str("workload").unwrap()
    } else {
        String::from(DEF_WORKLOAD)
    };
    let ops = expand_distribution(&ops)?;

    let mut workeropts = WorkerOptions {
        sync: !matches.opt_present("no-sync"),
        read_queue: false,
    };
    if ops.contains(&"r".to_owned()) || ops.contains(&"d".to_owned()) {
        workeropts.read_queue = true;
    }

    let q: Arc<Mutex<Queue<String>>> = Arc::new(Mutex::new(Queue::new(qmode)));

    if matches.opt_present("read-list") {
        let readlist = matches.opt_str("read-list").unwrap();
        match populate_queue(q.clone(), readlist) {
            Ok(_) => (),
            Err(e) => {
                usage(opts, &e.to_string());
                return Ok(());
            }
        }
    }

    let target = matches.opt_str("target").unwrap();

    /*
     * Parse the user's size distribution if one was provided, otherwise use
     * our default distr.
     */
    let user_distr = if matches.opt_present("distribution") {
        matches.opt_str("distribution").unwrap()
    } else {
        String::from(DEF_DISTR)
    };

    let distr =
        match convert_numeric_distribution(expand_distribution(&user_distr)?) {
            Ok(d) => d,
            Err(e) => {
                usage(
                    opts,
                    &format!(
                        "invalid distribution argument '{}': {}",
                        user_distr,
                        e.to_string()
                    ),
                );
                return Ok(());
            }
        };

    if conc < 1 {
        usage(opts, "concurrency must be > 1");
        return Ok(());
    }

    let mut cap: Option<DataCap> = None;
    let p = matches.opt_get("percentage")?;
    let m: Option<String> = matches.opt_get("max-data")?;

    /*
     * If the user provides both -p and -m, prefer -m since it is more specific.
     */
    if let Some(perc) = p {
        cap = Some(DataCap::Percentage(perc));
    } else if let Some(logical) = m {
        let capnum = parse_human(&logical)?;
        cap = Some(DataCap::LogicalData(capnum));
    }

    /*
     * Start the real work. Kick off worker threads and a stat listener.
     */

    let (tx, rx) = channel();

    let mut worker_threads: Vec<JoinHandle<_>> = Vec::new();
    for _ in 0..conc {
        /* There must be a way to make this more elegant. */
        let ctx = tx.clone();
        let ctarg = target.clone();
        let cdistr = distr.clone();
        let cq = q.clone();
        let cops = ops.clone();
        let dtx = debug_tx.clone();
        let wo = workeropts.clone();

        worker_threads.push(thread::spawn(move || {
            Worker::new(ctx, ctarg, cdistr, sleep, cq, cops, dtx, wo).work();
        }));
    }

    /* Kick off statistics collection and reporting. */
    let stat_thread = thread::spawn(move || {
        collect_stats(rx, interval, format, cap, target.clone());
    });

    /*
     * To make sure that the state thread exits when all worker threads exit,
     * drop our copy of the sender channel here.
     *
     * The state collection thread exits when all senders exit. This main
     * thread will live the life of the program and it will not send any states
     * through the channel.
     */
    drop(debug_tx);

    /*
     * When the stat thread exits we know that enough data was written.
     */
    stat_thread.join().expect("failed to join stat thread");

    for hdl in worker_threads {
        hdl.join().expect("failed to join worker thread");
    }

    if let Some(jh) = smap_thread {
        jh.join().expect("failed to join statemap thread");
    }

    Ok(())
}
