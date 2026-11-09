//! Bounded, cancellation-owned LAN discovery. Collectors admit candidates; only
//! this coordinator allocates probe slots, deadlines, and result identities.
pub mod cast;
pub mod cast_wire;
pub mod dial;

use crate::{api::ApiClient, http::HttpClients};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    net::SocketAddrV4,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    sync::mpsc,
    task::{JoinHandle, JoinSet},
    time::{Instant, sleep_until},
};
use tokio_util::sync::CancellationToken;

pub(crate) const ADMISSION_WINDOW: Duration = Duration::from_secs(5);
pub(crate) const PROBE_WINDOW: Duration = Duration::from_secs(45);
pub(crate) const RETRY_INTERVAL: Duration = Duration::from_secs(15);
const MAX_CANDIDATES: usize = 256;
const MAX_PROBES: usize = 8;

/// Screen IDs are opaque tokens (normally tens of bytes), not response bodies.
/// Allow ample headroom without retaining near-megabyte protocol fields.
pub const MAX_SCREEN_ID_BYTES: usize = 256;
/// Independent of candidate admissions: refreshed receivers can return new IDs.
pub const MAX_DISCOVERY_RESULTS: usize = 256;
/// Total emitted names/IDs plus the coordinator's retained ID copies. This also
/// bounds consumers that retain every device emitted during a passive session.
pub const MAX_DISCOVERY_RESULT_BYTES: usize = 64 * 1024;
const RESULT_LIMIT_WARNING: &str = "Discovery result retention limit reached; session ended";

fn validate_screen_id(id: &str) -> Result<(), &'static str> {
    if id.is_empty() || id.len() > MAX_SCREEN_ID_BYTES {
        Err("Discovery screen ID is empty or exceeds 256 bytes")
    } else {
        Ok(())
    }
}

#[derive(Default)]
struct Results {
    screen_ids: HashSet<String>,
    bytes: usize,
}

impl Results {
    /// Validate identities before calling; duplicates consume no additional
    /// budget or allocation, even when a refreshed receiver changes its name.
    fn insert(&mut self, device: &DiscoveredDevice) -> Result<bool, &'static str> {
        if self.screen_ids.contains(&device.screen_id) {
            return Ok(false);
        }
        let bytes = device
            .screen_id
            .len()
            .checked_mul(2)
            .and_then(|bytes| bytes.checked_add(device.name.len()))
            .and_then(|bytes| bytes.checked_add(self.bytes))
            .filter(|bytes| *bytes <= MAX_DISCOVERY_RESULT_BYTES)
            .ok_or(RESULT_LIMIT_WARNING)?;
        if self.screen_ids.len() >= MAX_DISCOVERY_RESULTS {
            return Err(RESULT_LIMIT_WARNING);
        }
        self.screen_ids.insert(device.screen_id.clone());
        self.bytes = bytes;
        Ok(true)
    }

    fn full(&self) -> bool {
        self.screen_ids.len() == MAX_DISCOVERY_RESULTS || self.bytes == MAX_DISCOVERY_RESULT_BYTES
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiscoveryMode {
    ActiveOnce,
    PassiveContinuous,
}

#[derive(Clone, PartialEq, Eq)]
pub struct DiscoveredDevice {
    pub screen_id: String,
    pub name: String,
}

impl std::fmt::Debug for DiscoveredDevice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DiscoveredDevice")
            .field("screen_id", &"[redacted]")
            .field("name", &"[redacted]")
            .finish()
    }
}

#[derive(Debug)]
pub enum DiscoveryEvent {
    Device(DiscoveredDevice),
    BackendWarning {
        backend: &'static str,
        message: String,
    },
    Complete,
}

#[derive(Clone, Copy)]
pub(crate) struct TransportPolicy {
    pub allow_loopback: bool,
}

#[derive(Clone)]
pub struct Discovery {
    context: ProbeContext,
    dial_transport: Option<SocketAddrV4>,
}

impl Discovery {
    pub fn new(api: ApiClient, http: HttpClients) -> Self {
        Self {
            context: ProbeContext {
                api,
                http,
                cast_transport: cast_wire::CastTransport::default(),
                mode: DiscoveryMode::ActiveOnce,
                policy: TransportPolicy {
                    allow_loopback: false,
                },
                claimed_endpoints: Arc::new(Mutex::new(HashSet::new())),
            },
            dial_transport: None,
        }
    }

    /// Constructor-only loopback fixture transport. Does not enumerate interfaces
    /// or send multicast; never selected by production CLI/environment settings.
    #[doc(hidden)]
    pub fn loopback(api: ApiClient, http: HttpClients, target: SocketAddrV4) -> Self {
        assert!(target.ip().is_loopback(), "fixture target must be loopback");
        let mut discovery = Self::new(api, http);
        discovery.context.policy.allow_loopback = true;
        discovery.dial_transport = Some(target);
        discovery
    }

    pub fn start(&self, mode: DiscoveryMode) -> DiscoverySession {
        let cancel = CancellationToken::new();
        let (tx, rx) = mpsc::channel(64);
        let mut context = self.context.clone();
        context.mode = mode;
        // A Discovery can be reused; all deduplication belongs to one session.
        context.claimed_endpoints = Arc::new(Mutex::new(HashSet::new()));
        let worker = tokio::spawn(run_session(
            context,
            self.dial_transport,
            cancel.clone(),
            tx,
            Instant::now(),
        ));
        DiscoverySession {
            cancel,
            rx,
            worker: Some(worker),
            mode,
            completed: false,
        }
    }
}

pub struct DiscoverySession {
    cancel: CancellationToken,
    rx: mpsc::Receiver<DiscoveryEvent>,
    worker: Option<JoinHandle<()>>,
    mode: DiscoveryMode,
    completed: bool,
}

impl DiscoverySession {
    pub async fn next(&mut self) -> Option<DiscoveryEvent> {
        match self.rx.recv().await {
            Some(DiscoveryEvent::Complete) => {
                self.completed = true;
                Some(DiscoveryEvent::Complete)
            }
            event @ Some(_) => event,
            None if self.mode == DiscoveryMode::ActiveOnce
                && !self.completed
                && !self.cancel.is_cancelled() =>
            {
                // Completion remains observable even when a slow consumer filled
                // its result queue at the hard session deadline.
                self.completed = true;
                Some(DiscoveryEvent::Complete)
            }
            None => None,
        }
    }

    pub async fn stop(mut self) {
        self.cancel.cancel();
        self.rx.close();
        if let Some(worker) = self.worker.take() {
            let _ = worker.await;
        }
    }
}

impl Drop for DiscoverySession {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

/// Both collectors use one admission/deadline/probe pool, including metadata and
/// metadata-derived Cast endpoints. No backend may spawn independent probes.
pub(crate) enum Candidate {
    Dial(dial::Candidate),
    Cast(cast::Candidate),
}

impl Candidate {
    fn keys(&self) -> Vec<String> {
        match self {
            Self::Dial(candidate) => candidate.keys(),
            Self::Cast(candidate) => candidate.keys(),
        }
    }
    fn backend(&self) -> &'static str {
        match self {
            Self::Dial(_) => "DIAL",
            Self::Cast(_) => "Cast",
        }
    }
    fn retry_interval(&self) -> Duration {
        match self {
            Self::Dial(_) => RETRY_INTERVAL,
            Self::Cast(candidate) => candidate.retry_interval(),
        }
    }
}

pub(crate) enum BackendEvent {
    Candidate {
        candidate: Candidate,
        observed_at: Instant,
    },
    /// Replace current work only if the coordinator can admit the new probe.
    Refresh {
        candidate: Candidate,
        observed_at: Instant,
    },
    Warning {
        backend: &'static str,
        message: &'static str,
    },
    Remove {
        key: String,
    },
}

#[derive(Clone)]
pub(crate) struct ProbeContext {
    pub api: ApiClient,
    pub http: HttpClients,
    pub cast_transport: cast_wire::CastTransport,
    pub mode: DiscoveryMode,
    pub policy: TransportPolicy,
    // Prevent multiple advertisements from launching the same application URL.
    // Only active probes use this set; its size is bounded by admissions.
    pub claimed_endpoints: Arc<Mutex<HashSet<String>>>,
}

struct Admission {
    last: Instant,
    pending: bool,
    succeeded: bool,
    generation: u64,
    token: CancellationToken,
}
struct Pending {
    candidate: Candidate,
    record: usize,
    generation: u64,
    deadline: Instant,
}
struct ProbeResult {
    record: usize,
    generation: u64,
    backend: &'static str,
    result: Result<Option<DiscoveredDevice>, &'static str>,
}

struct Admissions {
    mode: DiscoveryMode,
    end: Option<Instant>,
    queue: VecDeque<Pending>,
    records: Vec<Admission>,
    aliases: HashMap<String, usize>,
}

impl Admissions {
    fn new(mode: DiscoveryMode, end: Option<Instant>) -> Self {
        Self {
            mode,
            end,
            queue: VecDeque::new(),
            records: Vec::new(),
            aliases: HashMap::new(),
        }
    }

    fn invalidate(&mut self, record: usize, cancel: &CancellationToken) {
        let prior = &mut self.records[record];
        prior.token.cancel();
        prior.generation += 1;
        prior.token = cancel.child_token();
        prior.pending = false;
        prior.succeeded = false;
        prior.last = Instant::now() - PROBE_WINDOW;
        self.queue.retain(|pending| pending.record != record);
    }

    /// A refresh and its admission are one synchronous coordinator operation.
    /// Rejected late work must not cancel a still-valid admitted probe.
    fn admit(
        &mut self,
        candidate: Candidate,
        observed_at: Instant,
        refresh: bool,
        cancel: &CancellationToken,
    ) -> Result<(), ()> {
        // A blocked collector cannot extend the active admission window.
        if self
            .end
            .is_some_and(|end| observed_at >= end || Instant::now() >= end)
        {
            return Ok(());
        }
        let keys = candidate.keys();
        let retry_interval = candidate.retry_interval();
        let existing = keys.iter().find_map(|key| self.aliases.get(key).copied());
        let record = if let Some(record) = existing {
            if refresh {
                self.invalidate(record, cancel);
            } else {
                let prior = &self.records[record];
                if (self.mode == DiscoveryMode::ActiveOnce && prior.generation == 0)
                    || prior.pending
                    || prior.succeeded
                    || observed_at < prior.last + retry_interval
                {
                    return Ok(());
                }
            }
            // Retain only current aliases, bounding storage across mDNS updates.
            if self.records[record].generation != 0 {
                self.aliases.retain(|_, value| *value != record);
                for key in &keys {
                    self.aliases.insert(key.clone(), record);
                }
            }
            record
        } else {
            if self.records.len() == MAX_CANDIDATES {
                return Err(());
            }
            let record = self.records.len();
            for key in keys {
                self.aliases.insert(key, record);
            }
            self.records.push(Admission {
                last: observed_at,
                pending: false,
                succeeded: false,
                generation: 0,
                token: cancel.child_token(),
            });
            record
        };
        self.records[record].last = observed_at;
        self.records[record].pending = true;
        self.queue.push_back(Pending {
            candidate,
            record,
            generation: self.records[record].generation,
            deadline: observed_at + PROBE_WINDOW,
        });
        Ok(())
    }
}

async fn emit(
    tx: &mpsc::Sender<DiscoveryEvent>,
    cancel: &CancellationToken,
    end: Option<Instant>,
    event: DiscoveryEvent,
) -> bool {
    tokio::select! {
        biased;
        _ = cancel.cancelled() => false,
        _ = deadline(end) => false,
        result = tx.send(event) => result.is_ok(),
    }
}

async fn deadline(end: Option<Instant>) {
    match end {
        Some(end) => sleep_until(end).await,
        None => std::future::pending().await,
    }
}

async fn run_session(
    context: ProbeContext,
    dial_transport: Option<SocketAddrV4>,
    cancel: CancellationToken,
    output: mpsc::Sender<DiscoveryEvent>,
    start: Instant,
) {
    let admission_end =
        (context.mode == DiscoveryMode::ActiveOnce).then_some(start + ADMISSION_WINDOW);
    let (input_tx, input) = mpsc::channel(64);
    let collector_cancel = cancel.child_token();
    let mut collectors = JoinSet::new();
    collectors.spawn(dial::collect(
        input_tx.clone(),
        collector_cancel.clone(),
        admission_end,
        dial_transport,
        context.policy,
    ));
    if dial_transport.is_none() {
        collectors.spawn(cast::collect(
            input_tx.clone(),
            collector_cancel.clone(),
            admission_end,
            context.http.clone(),
        ));
    }
    drop(input_tx);
    run_coordinator(
        context,
        cancel,
        output,
        start,
        input,
        collector_cancel,
        collectors,
    )
    .await;
}

async fn run_coordinator(
    context: ProbeContext,
    cancel: CancellationToken,
    output: mpsc::Sender<DiscoveryEvent>,
    start: Instant,
    mut input: mpsc::Receiver<BackendEvent>,
    collector_cancel: CancellationToken,
    mut collectors: JoinSet<()>,
) {
    let admission_end =
        (context.mode == DiscoveryMode::ActiveOnce).then_some(start + ADMISSION_WINDOW);
    let end = admission_end.map(|end| end + PROBE_WINDOW);
    let mut probes = JoinSet::new();
    let mut admissions = Admissions::new(context.mode, admission_end);
    let mut results = Results::default();
    let mut overflow_warned = false;
    let mut input_closed = false;
    loop {
        while probes.len() < MAX_PROBES {
            let Some(pending) = admissions.queue.pop_front() else {
                break;
            };
            if pending.generation != admissions.records[pending.record].generation {
                continue;
            }
            if Instant::now() >= pending.deadline {
                admissions.records[pending.record].pending = false;
                continue;
            }
            let context = context.clone();
            let token = admissions.records[pending.record].token.clone();
            probes.spawn(async move {
                let backend = pending.candidate.backend();
                let result = tokio::select! {
                    biased;
                    _ = token.cancelled() => Ok(None),
                    _ = sleep_until(pending.deadline) => Err("Discovery probe timed out"),
                    result = async {
                        match pending.candidate {
                            Candidate::Dial(candidate) => dial::probe(candidate, &context).await,
                            Candidate::Cast(candidate) => cast::probe(candidate, &context).await,
                        }
                    } => result,
                };
                ProbeResult {
                    record: pending.record,
                    generation: pending.generation,
                    backend,
                    result,
                }
            });
        }
        if input_closed && probes.is_empty() && admissions.queue.is_empty() {
            break;
        }
        tokio::select! {
            biased;
            _ = cancel.cancelled() => break,
            _ = deadline(end) => break,
            result = probes.join_next(), if !probes.is_empty() => {
                match result {
                    Some(Ok(result)) => {
                        if result.generation != admissions.records[result.record].generation { continue; }
                        admissions.records[result.record].pending = false;
                        match result.result {
                            Ok(Some(device)) => {
                                admissions.records[result.record].succeeded = true;
                                if let Err(message) = validate_screen_id(&device.screen_id) {
                                    if !emit(&output, &cancel, end, DiscoveryEvent::BackendWarning {
                                        backend: result.backend, message: message.into(),
                                    }).await { break; }
                                    continue;
                                }
                                let exhausted = match results.insert(&device) {
                                    Ok(false) => continue,
                                    Ok(true) => {
                                        if !emit(&output, &cancel, end, DiscoveryEvent::Device(device)).await { break; }
                                        results.full()
                                    }
                                    Err(_) => true,
                                };
                                if exhausted {
                                    // Stop network work before waiting for a slow result
                                    // consumer; cancellation still interrupts the warning.
                                    input.close();
                                    collector_cancel.cancel();
                                    probes.abort_all();
                                    if emit(&output, &cancel, end, DiscoveryEvent::BackendWarning {
                                        backend: "Discovery", message: RESULT_LIMIT_WARNING.into(),
                                    }).await && context.mode == DiscoveryMode::PassiveContinuous {
                                        let _ = emit(&output, &cancel, end, DiscoveryEvent::Complete).await;
                                    }
                                    break;
                                }
                            }
                            Ok(None) => {},
                            Err(message) => if !emit(&output, &cancel, end, DiscoveryEvent::BackendWarning {
                                backend: result.backend, message: message.into(),
                            }).await { break; },
                        }
                    }
                    Some(Err(_)) => {
                        let delivered = emit(&output, &cancel, end, DiscoveryEvent::BackendWarning {
                            backend: "Discovery", message: "Discovery worker ended unexpectedly".into(),
                        }).await;
                        if !delivered { break; }
                    }
                    None => {},
                }
            }
            event = input.recv(), if !input_closed => {
                match event {
                    None => input_closed = true,
                    Some(BackendEvent::Remove { key }) => {
                        if let Some(&record) = admissions.aliases.get(&key) {
                            admissions.invalidate(record, &cancel);
                        }
                    }
                    Some(BackendEvent::Warning { backend, message }) => {
                        if !emit(&output, &cancel, end, DiscoveryEvent::BackendWarning { backend, message: message.into() }).await { break; }
                    }
                    Some(event @ (BackendEvent::Candidate { .. } | BackendEvent::Refresh { .. })) => {
                        let (candidate, observed_at, refresh) = match event {
                            BackendEvent::Candidate { candidate, observed_at } => (candidate, observed_at, false),
                            BackendEvent::Refresh { candidate, observed_at } => (candidate, observed_at, true),
                            _ => unreachable!(),
                        };
                        if admissions.admit(candidate, observed_at, refresh, &cancel).is_err() && !overflow_warned {
                            overflow_warned = true;
                            if !emit(&output, &cancel, end, DiscoveryEvent::BackendWarning {
                                backend: "Discovery", message: "Discovery candidate limit (256) reached; additional devices ignored".into(),
                            }).await { break; }
                        }
                    }
                }
            }
        }
    }
    // Cancelling the owned child token rather than the public session token
    // preserves next()'s completion semantics on a naturally finished scan.
    input.close();
    collector_cancel.cancel();
    probes.abort_all();
    while collectors.join_next().await.is_some() {}
    while probes.join_next().await.is_some() {}
    if !cancel.is_cancelled() && context.mode == DiscoveryMode::ActiveOnce {
        let _ = output.try_send(DiscoveryEvent::Complete);
    }
}
