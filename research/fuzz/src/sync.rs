//! Distributed fuzzing: workers on any machine report to one master.
//!
//! One TCP connection per worker, one JSON object per line. The worker
//! sends `hello`, then a `sync` about twice a second: its counters, the
//! inputs it found novel (with the blocks and edges they reached) and its
//! new findings. The master keeps what is new campaign-wide (`Hub`) and
//! answers with the shared entries the worker hasn't seen. Workers fuzz on
//! when the master is away and reconnect; nothing is lost but what was
//! queued for longer than `MAX_QUEUED` entries.

use crate::hub::{Entry, FindingReport, Hub, InstanceStats};
use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

pub const PROTOCOL: u32 = 1;
/// Default master address. Bind other interfaces explicitly (`--listen`).
pub const DEFAULT_ADDRESS: &str = "127.0.0.1:7979";
/// Largest message accepted (a batch of entries of up to 4 KiB each).
const MAX_MESSAGE: u64 = 64 << 20;
/// Shared entries per answer; a worker that is behind catches up over
/// several syncs.
const BATCH: usize = 512;
const SYNC_EVERY: Duration = Duration::from_millis(500);
const MAX_QUEUED: usize = 10_000;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Hello {
    pub protocol: u32,
    #[serde(default)]
    pub token: Option<String>,
    pub instance: String,
    /// Harness name and block count: workers must fuzz the same target.
    pub harness: String,
    pub blocks: usize,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ToMaster {
    Hello(Hello),
    Sync {
        cursor: usize,
        stats: InstanceStats,
        entries: Vec<Entry>,
        findings: Vec<FindingReport>,
    },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ToWorker {
    Welcome,
    Refused(String),
    Update {
        entries: Vec<Entry>,
        cursor: usize,
        stop: bool,
    },
}

fn send<T: Serialize>(stream: &mut TcpStream, message: &T) -> std::io::Result<()> {
    let mut line = serde_json::to_vec(message)?;
    line.push(b'\n');
    stream.write_all(&line)
}

fn receive<T: for<'de> Deserialize<'de>>(reader: &mut BufReader<TcpStream>) -> std::io::Result<T> {
    let mut line = Vec::new();
    reader.by_ref().take(MAX_MESSAGE).read_until(b'\n', &mut line)?;
    if line.last() != Some(&b'\n') {
        return Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "connection closed or message too large",
        ));
    }
    Ok(serde_json::from_slice(&line)?)
}

fn lock(hub: &Mutex<Hub>) -> std::sync::MutexGuard<'_, Hub> {
    hub.lock().unwrap_or_else(|e| e.into_inner())
}

/// Accept workers on `listener` for `hub` until the process exits.
pub fn serve(
    listener: TcpListener,
    hub: Arc<Mutex<Hub>>,
    token: Option<String>,
    harness: String,
    blocks: usize,
) -> JoinHandle<()> {
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let hub = hub.clone();
            let token = token.clone();
            let harness = harness.clone();
            std::thread::spawn(move || {
                let peer = stream.peer_addr().map(|a| a.to_string()).unwrap_or_default();
                if let Err(error) = session(stream, &hub, token.as_deref(), &harness, blocks) {
                    if error.kind() != std::io::ErrorKind::UnexpectedEof {
                        eprintln!("worker {peer}: {error}");
                    }
                }
            });
        }
    })
}

fn session(
    mut stream: TcpStream,
    hub: &Mutex<Hub>,
    token: Option<&str>,
    harness: &str,
    blocks: usize,
) -> std::io::Result<()> {
    stream.set_nodelay(true)?;
    stream.set_read_timeout(Some(Duration::from_secs(60)))?;
    stream.set_write_timeout(Some(Duration::from_secs(60)))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let ToMaster::Hello(hello) = receive(&mut reader)? else {
        return send(&mut stream, &ToWorker::Refused("say hello first".into()));
    };
    let refusal = if hello.protocol != PROTOCOL {
        Some(format!("protocol {} (master speaks {PROTOCOL})", hello.protocol))
    } else if token.is_some() && hello.token.as_deref() != token {
        Some("wrong token".to_string())
    } else if hello.harness != harness || hello.blocks != blocks {
        Some(format!(
            "harness {} with {} blocks (master fuzzes {harness} with {blocks})",
            hello.harness, hello.blocks
        ))
    } else if hello.instance.is_empty() {
        Some("empty instance name".to_string())
    } else {
        None
    };
    if let Some(reason) = refusal {
        return send(&mut stream, &ToWorker::Refused(reason));
    }
    send(&mut stream, &ToWorker::Welcome)?;
    loop {
        let ToMaster::Sync {
            cursor,
            mut stats,
            entries,
            findings,
        } = receive(&mut reader)?
        else {
            return Ok(());
        };
        stats.name.clone_from(&hello.instance);
        let (entries, cursor, stop) = {
            let mut hub = lock(hub);
            hub.submit(stats, entries, findings)
                .map_err(|error| std::io::Error::other(error.to_string()))?;
            let (entries, cursor) = hub.entries_since(cursor, &hello.instance, BATCH);
            (entries, cursor, hub.stop)
        };
        send(
            &mut stream,
            &ToWorker::Update {
                entries,
                cursor,
                stop,
            },
        )?;
    }
}

#[derive(Debug, Default)]
struct Outgoing {
    stats: InstanceStats,
    entries: Vec<Entry>,
    findings: Vec<FindingReport>,
}

impl Outgoing {
    fn add(&mut self, more: Outgoing) {
        self.stats = more.stats;
        self.entries.extend(more.entries);
        self.findings.extend(more.findings);
        if self.entries.len() > MAX_QUEUED {
            // Saved locally anyway; the master only misses them.
            let drop = self.entries.len() - MAX_QUEUED;
            self.entries.drain(..drop);
        }
    }
}

/// A worker's link to its master, run on its own thread so the network
/// never stalls fuzzing.
#[derive(Debug)]
pub struct Client {
    outgoing: Option<mpsc::Sender<Outgoing>>,
    incoming: mpsc::Receiver<Vec<Entry>>,
    stop: Arc<AtomicBool>,
    refused: Arc<Mutex<Option<String>>>,
    thread: Option<JoinHandle<()>>,
}

impl Client {
    pub fn start(address: String, hello: Hello) -> Self {
        let (outgoing, from_worker) = mpsc::channel::<Outgoing>();
        let (to_worker, incoming) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let refused = Arc::new(Mutex::new(None));
        let thread = {
            let stop = stop.clone();
            let refused = refused.clone();
            std::thread::spawn(move || link(address, hello, from_worker, to_worker, stop, refused))
        };
        Client {
            outgoing: Some(outgoing),
            incoming,
            stop,
            refused,
            thread: Some(thread),
        }
    }

    /// Queue a report; it goes out with the next sync.
    pub fn send(&self, stats: InstanceStats, entries: Vec<Entry>, findings: Vec<FindingReport>) {
        if let Some(outgoing) = &self.outgoing {
            let _ = outgoing.send(Outgoing {
                stats,
                entries,
                findings,
            });
        }
    }

    /// Entries other instances shared since the last call.
    pub fn received(&self) -> Vec<Entry> {
        self.incoming.try_iter().flatten().collect()
    }

    /// The master asked every worker to finish, or refused this one.
    pub fn stop_requested(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }

    pub fn refusal(&self) -> Option<String> {
        self.refused.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Send the last report (`stats.done` set) and wait briefly for it to
    /// reach the master.
    pub fn finish(mut self, mut stats: InstanceStats, entries: Vec<Entry>, findings: Vec<FindingReport>) {
        stats.done = true;
        self.send(stats, entries, findings);
        // Hanging up tells the link thread this is the last report.
        drop(self.outgoing.take());
        if let Some(thread) = self.thread.take() {
            let deadline = Instant::now() + Duration::from_secs(10);
            while !thread.is_finished() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(20));
            }
            if thread.is_finished() {
                let _ = thread.join();
            }
        }
    }
}

fn link(
    address: String,
    hello: Hello,
    from_worker: mpsc::Receiver<Outgoing>,
    to_worker: mpsc::Sender<Vec<Entry>>,
    stop: Arc<AtomicBool>,
    refused: Arc<Mutex<Option<String>>>,
) {
    let mut pending = Outgoing::default();
    let mut connection: Option<(TcpStream, BufReader<TcpStream>)> = None;
    let mut cursor = 0usize;
    let mut backoff = Duration::from_millis(250);
    let mut next_attempt = Instant::now();
    let mut last_words = false;
    let mut final_attempts = 0;
    loop {
        match from_worker.recv_timeout(SYNC_EVERY) {
            Ok(report) => pending.add(report),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => last_words = true,
        }
        while let Ok(report) = from_worker.try_recv() {
            pending.add(report);
        }
        if pending.stats.name.is_empty() {
            // Nothing to say before the first report.
            if last_words {
                return;
            }
            continue;
        }
        if connection.is_none() && Instant::now() >= next_attempt {
            match connect(&address, &hello) {
                Ok(Ok(link)) => {
                    connection = Some(link);
                    backoff = Duration::from_millis(250);
                }
                Ok(Err(reason)) => {
                    eprintln!("master {address} refused this worker: {reason}");
                    *refused.lock().unwrap_or_else(|e| e.into_inner()) = Some(reason);
                    stop.store(true, Ordering::Relaxed);
                    return;
                }
                Err(_) => {
                    next_attempt = Instant::now() + backoff;
                    backoff = (backoff * 2).min(Duration::from_secs(10));
                }
            }
        }
        let mut delivered = false;
        if let Some((stream, reader)) = &mut connection {
            let message = ToMaster::Sync {
                cursor,
                stats: pending.stats.clone(),
                entries: pending.entries.clone(),
                findings: pending.findings.clone(),
            };
            match send(stream, &message).and_then(|()| receive::<ToWorker>(reader)) {
                Ok(ToWorker::Update {
                    entries,
                    cursor: next,
                    stop: stopping,
                }) => {
                    pending.entries.clear();
                    pending.findings.clear();
                    cursor = next;
                    if !entries.is_empty() {
                        let _ = to_worker.send(entries);
                    }
                    if stopping {
                        stop.store(true, Ordering::Relaxed);
                    }
                    delivered = true;
                }
                Ok(_) | Err(_) => connection = None,
            }
        }
        if last_words {
            final_attempts += 1;
            if delivered || final_attempts >= 5 {
                return;
            }
        }
    }
}

type Link = (TcpStream, BufReader<TcpStream>);

fn connect(address: &str, hello: &Hello) -> std::io::Result<Result<Link, String>> {
    let target = address
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| std::io::Error::other("address resolves to nothing"))?;
    let mut stream = TcpStream::connect_timeout(&target, Duration::from_secs(3))?;
    stream.set_nodelay(true)?;
    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
    stream.set_write_timeout(Some(Duration::from_secs(30)))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    send(&mut stream, &ToMaster::Hello(hello.clone()))?;
    match receive::<ToWorker>(&mut reader)? {
        ToWorker::Welcome => Ok(Ok((stream, reader))),
        ToWorker::Refused(reason) => Ok(Err(reason)),
        ToWorker::Update { .. } => Err(std::io::Error::other("unexpected answer to hello")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CorpusStore;

    fn entry(data: &[u8], blocks: &[u64]) -> Entry {
        Entry {
            hash: CorpusStore::hash(data),
            data: data.to_vec(),
            blocks: Some(blocks.to_vec()),
            edges: Vec::new(),
            findings: Vec::new(),
        }
    }

    fn stats(name: &str) -> InstanceStats {
        InstanceStats {
            name: name.into(),
            ..Default::default()
        }
    }

    fn hello(name: &str, token: Option<&str>) -> Hello {
        Hello {
            protocol: PROTOCOL,
            token: token.map(str::to_string),
            instance: name.into(),
            harness: "h".into(),
            blocks: 3,
        }
    }

    fn wait(mut what: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !what() {
            assert!(Instant::now() < deadline, "timed out");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn workers_share_entries_through_the_master() {
        let hub = Arc::new(Mutex::new(Hub::new("h", Vec::new(), None, None)));
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap().to_string();
        serve(listener, hub.clone(), Some("secret".into()), "h".into(), 3);

        let a = Client::start(address.clone(), hello("a", Some("secret")));
        let b = Client::start(address.clone(), hello("b", Some("secret")));
        a.send(stats("a"), vec![entry(b"one", &[1])], vec![]);
        b.send(stats("b"), vec![entry(b"two", &[2])], vec![]);
        let mut got_a = Vec::new();
        let mut got_b = Vec::new();
        wait(|| {
            got_a.extend(a.received());
            got_b.extend(b.received());
            got_a.len() == 1 && got_b.len() == 1
        });
        assert_eq!(got_a[0].data, b"two");
        assert_eq!(got_b[0].data, b"one");
        assert_eq!(lock(&hub).corpus_len(), 2);

        // Stopping reaches workers; the last report marks them done.
        lock(&hub).stop = true;
        wait(|| a.stop_requested());
        a.finish(stats("a"), vec![], vec![]);
        wait(|| lock(&hub).instances().any(|row| row.stats.name == "a" && row.stats.done));

        let intruder = Client::start(address, hello("c", Some("guess")));
        intruder.send(stats("c"), vec![entry(b"three", &[3])], vec![]);
        wait(|| intruder.refusal().is_some());
        assert_eq!(intruder.refusal().as_deref(), Some("wrong token"));
        assert_eq!(lock(&hub).corpus_len(), 2);
    }
}
