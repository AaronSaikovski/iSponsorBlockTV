use super::scheduler::{PlaybackSnapshot, Suppression, next_skip, resolve_suppression};
use crate::{
    api::{
        ApiError,
        sponsorblock::{ApiService, Segment},
    },
    config::{RuntimeConfig, RuntimeDevice},
    lounge::{Command, Envelope, LoungeError, LoungeEvent, LoungeTransport, SessionIds},
};
use std::{
    collections::{HashSet, VecDeque},
    sync::Arc,
    time::Duration,
};
use tokio::{
    sync::mpsc,
    task::{AbortHandle, JoinSet},
    time::Instant,
};
use tokio_util::sync::CancellationToken;

const RETRY: Duration = Duration::from_secs(10);
const AUTH_LIFETIME: Duration = Duration::from_secs(24 * 60 * 60);
const COMMAND_CAPACITY: usize = 64;

#[derive(Clone, Copy, PartialEq, Eq)]
struct Identity {
    connection: u64,
    video: u64,
}
impl Identity {
    fn of(snapshot: &PlaybackSnapshot) -> Self {
        Self {
            connection: snapshot.connection_epoch,
            video: snapshot.video_epoch,
        }
    }
}

struct Queued {
    command: Command,
    identity: Identity,
}
struct Attempt {
    id: u64,
    identity: Identity,
    uuids: Vec<String>,
    seek: bool,
    volume_change: Option<(bool, u64)>,
}

/// Only the actor mutates protocol and playback state. Child tasks return values
/// or send bounded, epoch-tagged stream batches; none can mutate the reducer.
pub(super) struct DeviceActor {
    device: RuntimeDevice,
    config: Arc<RuntimeConfig>,
    api: Arc<ApiService>,
    transport: LoungeTransport,
    jobs: JoinSet<Completion>,
    session_cancel: CancellationToken,
    video_cancel: CancellationToken,
    token: Option<String>,
    sid: String,
    gsession: String,
    aid: u64,
    offset: u64,
    attempt_id: u64,
    in_flight: Option<Attempt>,
    queue: VecDeque<Queued>,
    snapshot: PlaybackSnapshot,
    duration: Option<f64>,
    segments: Arc<[Segment]>,
    suppressed: Vec<Suppression>,
    fetch: Option<AbortHandle>,
    segments_refresh_at: Option<Instant>,
    prefetches: HashSet<String>,
    lifecycle_busy: bool,
    stream_started: bool,
    connected: bool,
    recovery_used: bool,
    shorts: bool,
    volume: f64,
    muted: bool,
    owned_mute: bool,
    ad_active: bool,
    volume_revision: u64,
    retry_at: Instant,
    auth_at: Instant,
    last_event: Instant,
}

enum Completion {
    Token(u64, Result<String, LoungeError>),
    Available(u64, Result<bool, LoungeError>),
    Connected(u64, Result<Vec<Envelope>, LoungeError>),
    Recovered(u64, Result<(), LoungeError>),
    StreamEnded(u64, Result<(), LoungeError>),
    Command(u64, Result<(), LoungeError>),
    Segments(Identity, String, Result<Arc<[Segment]>, ApiError>),
    Prefetched(u64, String),
    Reported(Result<(), ApiError>),
    Cancelled,
}

impl DeviceActor {
    pub(super) fn new(
        device: RuntimeDevice,
        config: Arc<RuntimeConfig>,
        api: Arc<ApiService>,
        transport: LoungeTransport,
    ) -> Self {
        let now = Instant::now();
        Self {
            device,
            config,
            api,
            transport,
            jobs: JoinSet::new(),
            session_cancel: CancellationToken::new(),
            video_cancel: CancellationToken::new(),
            token: None,
            sid: String::new(),
            gsession: String::new(),
            aid: 0,
            offset: 1,
            attempt_id: 0,
            in_flight: None,
            queue: VecDeque::with_capacity(COMMAND_CAPACITY),
            snapshot: PlaybackSnapshot {
                video_id: String::new(),
                state: 0,
                position: 0.0,
                speed: 1.0,
                observed: now,
                connection_epoch: 1,
                video_epoch: 1,
                revision: 0,
            },
            duration: None,
            segments: Arc::from([]),
            suppressed: Vec::new(),
            fetch: None,
            segments_refresh_at: None,
            prefetches: HashSet::new(),
            lifecycle_busy: false,
            stream_started: false,
            connected: false,
            recovery_used: false,
            shorts: false,
            volume: 100.0,
            muted: false,
            owned_mute: false,
            ad_active: false,
            volume_revision: 0,
            retry_at: now,
            auth_at: now + AUTH_LIFETIME,
            last_event: now,
        }
    }

    pub(super) async fn run(mut self, shutdown: CancellationToken) {
        tracing::info!("Monitoring configured device");
        let (sender, mut receiver) = mpsc::channel::<(u64, Envelope)>(256);
        let mut watchdog = tokio::time::interval_at(Instant::now() + RETRY, RETRY);
        loop {
            if !self.connected && !self.lifecycle_busy && Instant::now() >= self.retry_at {
                self.start_lifecycle();
            }
            if self.connected && !self.stream_started && self.has_session_ids() {
                self.start_stream(sender.clone());
            }
            self.dispatch_queued();
            let skip_at = if self.connected && self.in_flight.is_none() && self.has_session_ids() {
                next_skip(
                    &self.snapshot,
                    &self.segments,
                    &self.suppressed,
                    self.device.offset_seconds,
                    Instant::now(),
                )
                .and_then(|(_, delay)| Instant::now().checked_add(delay))
            } else {
                None
            };
            let retry_at = (!self.connected && !self.lifecycle_busy).then_some(self.retry_at);
            tokio::select! {
                biased;
                _ = shutdown.cancelled() => break,
                batch = receiver.recv() => {
                    if let Some((epoch, envelope)) = batch
                        && epoch == self.snapshot.connection_epoch {
                            self.last_event = Instant::now();
                            self.aid = envelope.aid;
                            self.event(envelope.event);
                        }
                },
                completion = self.jobs.join_next(), if !self.jobs.is_empty() => {
                    match completion {
                        Some(Ok(completion)) => self.complete(completion),
                        Some(Err(error)) if error.is_cancelled() => {},
                        Some(Err(_)) => { tracing::warn!("device child task failed; reconnecting"); self.reset(false); },
                        None => {},
                    }
                },
                _ = wait_until(skip_at) => self.dispatch_skip(),
                _ = wait_until(retry_at) => {},
                _ = watchdog.tick() => self.check_timers(),
            }
        }
        // Capture the latest AID and offset before invalidating local state.
        let terminate = self.token.take().zip(self.ids());
        self.session_cancel.cancel();
        self.video_cancel.cancel();
        self.jobs.abort_all();
        while self.jobs.join_next().await.is_some() {}
        self.queue.clear();
        self.sid.clear();
        self.gsession.clear();
        if let Some((token, ids)) = terminate
            && self
                .transport
                .clone()
                .disconnect(token, ids, self.offset)
                .await
                .is_err()
        {
            tracing::debug!("Lounge termination did not complete successfully");
        }
    }

    fn has_session_ids(&self) -> bool {
        !self.sid.is_empty() && !self.gsession.is_empty()
    }

    fn ids(&self) -> Option<SessionIds> {
        if !self.has_session_ids() {
            None
        } else {
            Some(SessionIds {
                sid: self.sid.clone(),
                gsession: self.gsession.clone(),
                aid: self.aid,
            })
        }
    }

    fn reset(&mut self, clear_token: bool) {
        self.session_cancel.cancel();
        self.video_cancel.cancel();
        self.session_cancel = CancellationToken::new();
        self.video_cancel = self.session_cancel.child_token();
        if let Some(fetch) = self.fetch.take() {
            fetch.abort();
        }
        self.snapshot.connection_epoch += 1;
        self.snapshot.video_epoch += 1;
        self.snapshot.revision += 1;
        self.snapshot.state = 0;
        self.snapshot.video_id.clear();
        self.snapshot.speed = 1.0;
        self.snapshot.observed = Instant::now();
        self.segments = Arc::from([]);
        self.segments_refresh_at = None;
        self.suppressed.clear();
        self.queue.clear();
        self.prefetches.clear();
        // Do not erase an in-flight attempt: its completion releases the sole
        // command slot, but its old epochs can never report or enqueue work.
        self.sid.clear();
        self.gsession.clear();
        self.aid = 0;
        self.offset = 1;
        self.connected = false;
        self.stream_started = false;
        self.lifecycle_busy = false;
        // The same physical device may remain muted through a reconnect.
        // Retain ownership and its last volume until a real volume observation
        // or our restoration resolves it; losing the session is not an unmute.
        if clear_token {
            self.token = None;
        }
        self.retry_at = Instant::now() + RETRY;
        tracing::info!(
            retry_seconds = RETRY.as_secs(),
            "Device session reset; reconnect scheduled"
        );
    }

    fn start_lifecycle(&mut self) {
        self.lifecycle_busy = true;
        let epoch = self.snapshot.connection_epoch;
        let transport = self.transport.clone();
        let cancel = self.session_cancel.clone();
        if let Some(token) = self.token.clone() {
            self.jobs.spawn(async move {
                tokio::select! {
                    _ = cancel.cancelled() => Completion::Cancelled,
                    result = transport.availability(token) => Completion::Available(epoch, result),
                }
            });
        } else {
            self.jobs.spawn(async move {
                tokio::select! {
                    _ = cancel.cancelled() => Completion::Cancelled,
                    result = transport.refresh_token() => Completion::Token(epoch, result),
                }
            });
        }
    }

    fn connect(&mut self) {
        let Some(token) = self.token.clone() else {
            return;
        };
        self.lifecycle_busy = true;
        let epoch = self.snapshot.connection_epoch;
        let transport = self.transport.clone();
        let cancel = self.session_cancel.clone();
        self.jobs.spawn(async move {
            tokio::select! {
                _ = cancel.cancelled() => Completion::Cancelled,
                result = transport.connect(token) => Completion::Connected(epoch, result),
            }
        });
    }

    fn start_stream(&mut self, sender: mpsc::Sender<(u64, Envelope)>) {
        let (Some(token), Some(ids)) = (self.token.clone(), self.ids()) else {
            return;
        };
        self.stream_started = true;
        let epoch = self.snapshot.connection_epoch;
        let transport = self.transport.clone();
        let cancel = self.session_cancel.clone();
        self.jobs.spawn(async move {
            tokio::select! {
                _ = cancel.cancelled() => Completion::Cancelled,
                result = async {
                    let mut stream = transport.subscribe(token, ids).await?;
                    while let Some(batch) = stream.next_batch().await? {
                        for envelope in batch {
                            if sender.send((epoch, envelope)).await.is_err() { return Ok(()); }
                        }
                    }
                    Ok(())
                } => Completion::StreamEnded(epoch, result),
            }
        });
    }

    fn handle_error(&mut self, error: LoungeError) {
        tracing::warn!(error = %error, "Lounge operation failed; retrying");
        match error {
            LoungeError::Expired => self.reset(true),
            LoungeError::ConnectionDenied if !self.recovery_used => {
                self.recovery_used = true;
                let token = self.token.clone();
                self.reset(false);
                if let Some(token) = token {
                    self.lifecycle_busy = true;
                    let epoch = self.snapshot.connection_epoch;
                    let transport = self.transport.clone();
                    let cancel = self.session_cancel.clone();
                    self.jobs.spawn(async move { tokio::select! {
                        _ = cancel.cancelled() => Completion::Cancelled,
                        result = transport.recover(token) => Completion::Recovered(epoch, result),
                    }});
                }
            }
            _ => self.reset(false),
        }
    }

    fn complete(&mut self, completion: Completion) {
        let current = self.snapshot.connection_epoch;
        match completion {
            Completion::Token(epoch, result) if epoch == current => {
                self.lifecycle_busy = false;
                match result {
                    Ok(token) => {
                        self.token = Some(token);
                        self.auth_at = Instant::now() + AUTH_LIFETIME;
                        self.retry_at = Instant::now();
                    }
                    Err(error) => self.handle_error(error),
                }
            }
            Completion::Available(epoch, result) if epoch == current => {
                self.lifecycle_busy = false;
                match result {
                    Ok(true) => self.connect(),
                    Ok(false) => self.retry_at = Instant::now() + RETRY,
                    Err(error) => self.handle_error(error),
                }
            }
            Completion::Connected(epoch, result) if epoch == current => {
                self.lifecycle_busy = false;
                match result {
                    Ok(batch) => {
                        self.connected = true;
                        self.last_event = Instant::now();
                        self.batch(batch);
                        if self.connected && !self.has_session_ids() {
                            tracing::warn!(
                                "Lounge handshake did not provide both session identifiers"
                            );
                            self.reset(false);
                        } else if self.connected {
                            tracing::info!("Connected to YouTube");
                        }
                    }
                    Err(error) => self.handle_error(error),
                }
            }
            Completion::Recovered(epoch, result) if epoch == current => {
                tracing::warn!(
                    succeeded = result.is_ok(),
                    "Screen identity recovery finished; force-close and reopen YouTube on the device"
                );
                self.reset(true);
            }
            Completion::StreamEnded(epoch, result) if epoch == current => match result {
                Ok(()) => self.reset(false),
                Err(error) => self.handle_error(error),
            },
            Completion::Command(id, result) => {
                if !self
                    .in_flight
                    .as_ref()
                    .is_some_and(|attempt| attempt.id == id)
                {
                    return;
                }
                let attempt = self.in_flight.take().expect("matched attempt");
                if attempt.identity.connection != self.snapshot.connection_epoch {
                    return;
                }
                if let Some((muted, revision)) = attempt.volume_change {
                    if result.is_ok() && revision == self.volume_revision {
                        self.muted = muted;
                        if !muted {
                            self.owned_mute = false;
                        }
                    } else if result.is_err()
                        && muted
                        && (revision == self.volume_revision || !self.muted)
                    {
                        // Only an unconfirmed failed mute relinquishes ownership.
                        self.owned_mute = false;
                        self.queue.retain(|queued| {
                            !matches!(queued.command, Command::SetVolume { muted: false, .. })
                        });
                    }
                    if result.is_ok()
                        && !muted
                        && self.ad_active
                        && self.config.mute_ads
                        && !self.muted
                        && !self.owned_mute
                    {
                        self.enqueue(Command::SetVolume {
                            volume: self.volume,
                            muted: true,
                        });
                    }
                }
                if attempt.identity != Identity::of(&self.snapshot) {
                    return;
                }
                if result.is_ok() && attempt.seek {
                    tracing::info!(segments = attempt.uuids.len(), "SponsorBlock seek accepted");
                }
                match result {
                    Ok(())
                        if attempt.seek
                            && self.config.skip_count_tracking
                            && !attempt.uuids.is_empty() =>
                    {
                        let api = self.api.clone();
                        let cancel = self.video_cancel.clone();
                        self.jobs.spawn(async move { tokio::select! {
                            _ = cancel.cancelled() => Completion::Cancelled,
                            result = api.report_viewed(&attempt.uuids) => Completion::Reported(result),
                        }});
                    }
                    Ok(()) => {}
                    Err(error) => {
                        // Ambiguous mutating requests are never replayed. Keep
                        // seek suppression until an actual resolving observation.
                        if matches!(
                            error,
                            LoungeError::Expired | LoungeError::UnknownSid | LoungeError::Gone
                        ) {
                            self.handle_error(error);
                        } else {
                            tracing::warn!(error = %error, "Lounge command failed; refreshing playback without replay");
                            if attempt.seek {
                                self.enqueue(Command::GetNowPlaying);
                            }
                        }
                    }
                }
            }
            Completion::Segments(identity, video, result)
                if identity == Identity::of(&self.snapshot) && video == self.snapshot.video_id =>
            {
                self.fetch = None;
                match result {
                    Ok(segments) => {
                        self.segments = segments;
                        // Periodically consult the API cache again so its
                        // expiring empty/unlocked entries can be refreshed.
                        self.segments_refresh_at = Some(Instant::now() + Duration::from_secs(300));
                    }
                    Err(error) => {
                        self.segments_refresh_at = None;
                        tracing::warn!(error = %error, "Segment lookup failed");
                    }
                }
            }
            Completion::Prefetched(epoch, video) if epoch == current => {
                self.prefetches.remove(&video);
            }
            Completion::Reported(Err(error)) => {
                tracing::warn!(error = %error, "Viewed-segment reporting failed; not retrying")
            }
            _ => {}
        }
    }

    fn batch(&mut self, batch: Vec<Envelope>) {
        let epoch = self.snapshot.connection_epoch;
        self.last_event = Instant::now();
        for envelope in batch {
            if self.snapshot.connection_epoch != epoch {
                break;
            }
            self.aid = envelope.aid;
            self.event(envelope.event);
        }
    }

    fn event(&mut self, event: LoungeEvent) {
        match event {
            LoungeEvent::Connected { sid } => {
                if !self.sid.is_empty() && self.sid != sid {
                    self.reset(false);
                } else {
                    self.sid = sid;
                }
            }
            LoungeEvent::Session { gsession } => {
                if !self.gsession.is_empty() && self.gsession != gsession {
                    self.reset(false);
                } else {
                    self.gsession = gsession;
                }
            }
            LoungeEvent::StateChange {
                position,
                duration,
                state,
            } => {
                self.observe(position, duration, state);
                if self.snapshot.state == 1 {
                    self.restore_mute();
                }
            }
            LoungeEvent::NowPlaying {
                video_id,
                position,
                duration,
                state,
            } => {
                if video_id != self.snapshot.video_id {
                    self.video_cancel.cancel();
                    self.video_cancel = self.session_cancel.child_token();
                    self.snapshot.video_epoch += 1;
                    self.snapshot.video_id = video_id;
                    self.suppressed.clear();
                    self.queue.clear();
                    if let Some(fetch) = self.fetch.take() {
                        fetch.abort();
                    }
                    self.segments = Arc::from([]);
                    self.segments_refresh_at = None;
                }
                self.observe(position, duration, Some(state));
                if state == 1 {
                    self.restore_mute();
                }
            }
            LoungeEvent::PlaybackSpeed { speed } => {
                self.rebase(None);
                self.snapshot.speed = speed;
                self.invalidate_schedule();
                self.enqueue(Command::GetNowPlaying);
            }
            LoungeEvent::AdState {
                ended,
                skip_enabled,
            } => {
                if ended {
                    self.restore_mute();
                    self.enqueue(Command::GetNowPlaying);
                } else {
                    self.ad(skip_enabled);
                }
            }
            LoungeEvent::AdPlaying {
                content_video_id,
                skip_enabled,
            } => {
                self.prefetch(content_video_id);
                self.ad(skip_enabled);
            }
            LoungeEvent::Volume { volume, muted } => {
                if let Some(volume) = volume {
                    self.volume = volume;
                }
                if let Some(muted) = muted {
                    self.volume_revision += 1;
                    self.muted = muted;
                    if !muted {
                        self.owned_mute = false;
                    }
                }
            }
            LoungeEvent::AutoplayUpNext { video_id } => self.prefetch(video_id),
            LoungeEvent::LoungeStatus { kids, .. } => {
                if kids {
                    tracing::warn!(
                        "YouTube Kids profile cannot be controlled; waiting for a supported profile"
                    );
                    self.reset(false);
                }
            }
            LoungeEvent::Disconnected { shorts } => {
                self.shorts = shorts;
                self.reset(true);
            }
            LoungeEvent::Subtitles { video_id } => {
                if self.shorts
                    && let Some(video_id) = video_id.filter(|id| !id.is_empty())
                {
                    self.shorts = false;
                    self.enqueue(Command::SetPlaylist { video_id });
                }
            }
            LoungeEvent::AutoplayMode { enabled } => {
                if enabled != self.config.auto_play {
                    self.enqueue(Command::SetAutoplayMode {
                        enabled: self.config.auto_play,
                    });
                }
            }
            LoungeEvent::Malformed { kind } => {
                tracing::warn!(kind, "Ignoring malformed Lounge event payload")
            }
            LoungeEvent::Unknown => tracing::debug!("Ignoring unsupported Lounge event"),
            LoungeEvent::Noop => {}
        }
    }

    fn check_timers(&mut self) {
        let now = Instant::now();
        if self.token.is_some() && now >= self.auth_at {
            self.reset(true);
        } else if self.connected && now.duration_since(self.last_event) > Duration::from_secs(60) {
            tracing::warn!("Lounge subscription inactive; reconnecting");
            self.reset(false);
        }
    }

    fn rebase(&mut self, position: Option<f64>) {
        let now = Instant::now();
        if let Some(position) = position {
            self.snapshot.position = position;
        } else if self.snapshot.state == 1
            && self.snapshot.speed.is_finite()
            && self.snapshot.speed > 0.0
        {
            let estimated = self.snapshot.position
                + now
                    .saturating_duration_since(self.snapshot.observed)
                    .as_secs_f64()
                    * self.snapshot.speed;
            if estimated.is_finite() {
                self.snapshot.position = estimated;
            }
        }
        self.snapshot.observed = now;
    }

    fn observe(&mut self, position: Option<f64>, duration: Option<f64>, state: Option<i32>) {
        if let Some(position) = position {
            self.snapshot.position = position;
        }
        self.snapshot.observed = Instant::now();
        if let Some(duration) = duration {
            self.duration = Some(duration);
        }
        if let Some(state) = state {
            self.snapshot.state = state;
        }
        if position.is_some() {
            resolve_suppression(&mut self.suppressed, &self.snapshot);
        }
        self.invalidate_schedule();
    }

    fn invalidate_schedule(&mut self) {
        self.snapshot.revision += 1;
        self.fetch_segments();
    }

    fn fetch_segments(&mut self) {
        // Playback revisions retain both pending requests and completed data.
        // After a failure, the next observation retries; successful results
        // (including empty ones) are reused until the cache refresh deadline.
        if self.fetch.is_some()
            || self
                .segments_refresh_at
                .is_some_and(|deadline| Instant::now() < deadline)
            || self.snapshot.video_id.is_empty()
            || !self.snapshot.speed.is_finite()
            || self.snapshot.speed <= 0.0
        {
            return;
        }
        let identity = Identity::of(&self.snapshot);
        let video = self.snapshot.video_id.clone();
        let api = self.api.clone();
        let cancel = self.video_cancel.clone();
        self.fetch = Some(self.jobs.spawn(async move {
            tokio::select! {
                _ = cancel.cancelled() => Completion::Cancelled,
                result = api.segments(&video) => Completion::Segments(identity, video, result),
            }
        }));
    }

    fn prefetch(&mut self, video: Option<String>) {
        let Some(video) = video.filter(|video| !video.is_empty()) else {
            return;
        };
        if self.prefetches.len() >= 8 || !self.prefetches.insert(video.clone()) {
            return;
        }
        let api = self.api.clone();
        let cancel = self.session_cancel.clone();
        let epoch = self.snapshot.connection_epoch;
        self.jobs.spawn(async move {
            tokio::select! {
                _ = cancel.cancelled() => Completion::Cancelled,
                _ = api.segments(&video) => Completion::Prefetched(epoch, video),
            }
        });
    }

    fn enqueue(&mut self, command: Command) {
        // Coalesce idempotent state requests, not seek attempts. This also keeps
        // rapid ad/volume/autoplay observations from filling the bounded queue.
        let kind = std::mem::discriminant(&command);
        self.queue
            .retain(|queued| std::mem::discriminant(&queued.command) != kind);
        if self.queue.len() == COMMAND_CAPACITY {
            tracing::warn!("Lounge command queue full; rejecting unsent command");
            return;
        }
        self.queue.push_back(Queued {
            command,
            identity: Identity::of(&self.snapshot),
        });
    }

    fn dispatch_queued(&mut self) {
        if !self.connected || self.in_flight.is_some() || !self.has_session_ids() {
            return;
        }
        while let Some(mut queued) = self.queue.pop_front() {
            if queued.identity != Identity::of(&self.snapshot) {
                continue;
            }
            if matches!(queued.command, Command::SkipAd) && !self.ad_active {
                continue;
            }
            if let Command::SetVolume { volume, muted } = &mut queued.command {
                *volume = self.volume;
                if *muted {
                    if !self.ad_active || self.muted || self.owned_mute {
                        continue;
                    }
                    self.owned_mute = true;
                } else {
                    if self.ad_active || !self.owned_mute {
                        continue;
                    }
                }
            }
            self.dispatch(queued.command, Vec::new(), false);
            break;
        }
    }

    fn dispatch_skip(&mut self) {
        if self.in_flight.is_some() || !self.connected || !self.has_session_ids() {
            return;
        }
        let Some((index, delay)) = next_skip(
            &self.snapshot,
            &self.segments,
            &self.suppressed,
            self.device.offset_seconds,
            Instant::now(),
        ) else {
            return;
        };
        if !delay.is_zero() {
            return;
        }
        let segment = &self.segments[index];
        let end = segment.end;
        let uuids = segment.uuids.clone();
        self.suppressed.push(Suppression {
            connection_epoch: self.snapshot.connection_epoch,
            video_epoch: self.snapshot.video_epoch,
            start: segment.start,
            end,
        });
        self.dispatch(Command::SeekTo { seconds: end }, uuids, true);
    }

    fn dispatch(&mut self, command: Command, uuids: Vec<String>, seek: bool) {
        let (Some(token), Some(ids)) = (self.token.clone(), self.ids()) else {
            return;
        };
        let id = self.attempt_id;
        self.attempt_id += 1;
        let offset = self.offset;
        self.offset += 1;
        let volume_change = match &command {
            Command::SetVolume { muted, .. } => Some((*muted, self.volume_revision)),
            _ => None,
        };
        self.in_flight = Some(Attempt {
            id,
            identity: Identity::of(&self.snapshot),
            uuids,
            seek,
            volume_change,
        });
        let transport = self.transport.clone();
        let cancel = self.session_cancel.clone();
        self.jobs.spawn(async move {
            // Cancellation still delivers the attempt ID, so an old session's
            // cancelled request cannot permanently occupy the command slot.
            let result = tokio::select! {
                _ = cancel.cancelled() => Err(LoungeError::MissingSession),
                result = transport.command(token, ids, offset, command) => result,
            };
            Completion::Command(id, result)
        });
    }

    fn ad(&mut self, skip_enabled: bool) {
        if !self.ad_active {
            tracing::info!(
                skip_enabled,
                skip_ads = self.config.skip_ads,
                mute_ads = self.config.mute_ads,
                "YouTube advertisement detected"
            );
        }
        self.ad_active = true;
        self.queue
            .retain(|queued| !matches!(queued.command, Command::SetVolume { muted: false, .. }));
        // Ads invalidate content seeks even when the server omits a state event.
        self.rebase(None);
        self.snapshot.state = 1081;
        self.invalidate_schedule();
        if self.config.skip_ads && skip_enabled {
            self.enqueue(Command::SkipAd);
        }
        if self.config.mute_ads && !self.muted && !self.owned_mute {
            self.enqueue(Command::SetVolume {
                volume: self.volume,
                muted: true,
            });
        }
    }

    fn restore_mute(&mut self) {
        if self.ad_active {
            tracing::info!("YouTube advertisement ended; returning to content");
        }
        self.ad_active = false;
        self.queue.retain(|queued| {
            !matches!(
                queued.command,
                Command::SkipAd | Command::SetVolume { muted: true, .. }
            )
        });
        if self.config.mute_ads && self.owned_mute {
            self.enqueue(Command::SetVolume {
                volume: self.volume,
                muted: false,
            });
        }
    }
}

async fn wait_until(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending::<()>().await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::ConfigDocument,
        http::{Endpoints, HttpClients},
    };
    use serde_json::json;

    fn actor() -> DeviceActor {
        let document: ConfigDocument = serde_json::from_value(json!({
            "devices":[{"screen_id":"test-screen"}],"mute_ads":true,"skip_ads":true
        }))
        .unwrap();
        let config = Arc::new(document.runtime_config().unwrap());
        let base = url::Url::parse("http://127.0.0.1:9/").unwrap();
        let endpoints = Endpoints {
            lounge: base.clone(),
            youtube: base.clone(),
            sponsorblock: base.clone(),
        };
        let transport = LoungeTransport::new(
            HttpClients::new(false, false).unwrap(),
            base,
            "test-screen".into(),
            config.join_name.clone(),
        )
        .unwrap();
        let api = Arc::new(ApiService::new(&config, endpoints, false).unwrap());
        DeviceActor::new(config.devices[0].clone(), config, api, transport)
    }

    fn dispatched(actor: &mut DeviceActor) {
        actor.snapshot.video_id = "test-video".into();
        actor.snapshot.state = 1;
        actor.in_flight = Some(Attempt {
            id: 17,
            identity: Identity::of(&actor.snapshot),
            uuids: vec!["test-uuid".into()],
            seek: true,
            volume_change: None,
        });
        actor.suppressed.push(Suppression {
            connection_epoch: actor.snapshot.connection_epoch,
            video_epoch: actor.snapshot.video_epoch,
            start: 0.0,
            end: 5.0,
        });
    }

    #[tokio::test]
    async fn target_observation_before_http_completion_still_reports_once() {
        let mut actor = actor();
        dispatched(&mut actor);
        actor.snapshot.position = 5.0;
        actor.snapshot.revision += 1;
        resolve_suppression(&mut actor.suppressed, &actor.snapshot);
        assert!(actor.suppressed.is_empty());
        actor.complete(Completion::Command(17, Ok(())));
        assert_eq!(actor.jobs.len(), 1);
        assert!(actor.in_flight.is_none());
        actor.complete(Completion::Command(17, Ok(())));
        assert_eq!(
            actor.jobs.len(),
            1,
            "duplicate completion must not report again"
        );
    }

    #[tokio::test]
    async fn pause_speed_and_revision_changes_do_not_erase_attempt_identity() {
        let mut actor = actor();
        dispatched(&mut actor);
        actor.snapshot.state = 2;
        actor.snapshot.speed = 2.0;
        actor.snapshot.revision += 9;
        actor.complete(Completion::Command(17, Ok(())));
        assert_eq!(actor.jobs.len(), 1);
        assert_eq!(
            actor.suppressed.len(),
            1,
            "HTTP success cannot resolve suppression"
        );
        actor.snapshot.state = 1;
        assert!(
            next_skip(
                &actor.snapshot,
                &[Segment {
                    start: 0.0,
                    end: 5.0,
                    uuids: vec![]
                }],
                &actor.suppressed,
                0.0,
                actor.snapshot.observed
            )
            .is_none()
        );
    }

    #[tokio::test]
    async fn video_and_connection_epochs_reject_old_success() {
        for connection in [false, true] {
            let mut actor = actor();
            dispatched(&mut actor);
            if connection {
                actor.snapshot.connection_epoch += 1;
            } else {
                actor.snapshot.video_epoch += 1;
            }
            actor.complete(Completion::Command(17, Ok(())));
            assert!(actor.jobs.is_empty());
            assert!(actor.in_flight.is_none());
        }
    }

    #[tokio::test]
    async fn ambiguous_failure_keeps_suppression_and_refreshes_once() {
        let mut actor = actor();
        dispatched(&mut actor);
        actor.complete(Completion::Command(17, Err(LoungeError::Status(500))));
        assert_eq!(actor.suppressed.len(), 1);
        assert!(actor.jobs.is_empty());
        assert_eq!(actor.queue.len(), 1);
        assert!(matches!(actor.queue[0].command, Command::GetNowPlaying));
        actor.complete(Completion::Command(17, Err(LoungeError::Status(500))));
        assert_eq!(actor.queue.len(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn segment_metadata_survives_playback_revisions_including_empty_results() {
        for segments in [
            Arc::<[Segment]>::from([]),
            Arc::from([Segment {
                start: 0.0,
                end: 5.0,
                uuids: vec![],
            }]),
        ] {
            let mut actor = actor();
            actor.snapshot.video_id = "current".into();
            actor.snapshot.position = 20.0;
            let identity = Identity::of(&actor.snapshot);
            let revision = actor.snapshot.revision;
            let (send, receive) = tokio::sync::oneshot::channel();
            actor.fetch = Some(actor.jobs.spawn(async move {
                Completion::Segments(identity, "current".into(), receive.await.unwrap())
            }));

            actor.event(LoungeEvent::StateChange {
                position: Some(12.0),
                duration: None,
                state: Some(2),
            });
            actor.event(LoungeEvent::NowPlaying {
                video_id: "current".into(),
                position: Some(1.0),
                duration: None,
                state: 1,
            });
            actor.event(LoungeEvent::PlaybackSpeed { speed: 2.0 });
            assert!(actor.snapshot.revision > revision);
            assert!(actor.fetch.is_some());
            assert_eq!(actor.jobs.len(), 1, "observations must not restart lookup");

            send.send(Ok(segments.clone())).unwrap();
            let completion = actor.jobs.join_next().await.unwrap().unwrap();
            actor.complete(completion);
            assert!(actor.fetch.is_none());
            assert!(Arc::ptr_eq(&actor.segments, &segments));
            assert_eq!(
                next_skip(
                    &actor.snapshot,
                    &actor.segments,
                    &actor.suppressed,
                    0.0,
                    Instant::now(),
                ),
                (!segments.is_empty()).then_some((0, Duration::ZERO)),
                "completion must schedule from the latest position",
            );

            actor.observe(Some(2.0), None, Some(2));
            actor.event(LoungeEvent::PlaybackSpeed { speed: 0.0 });
            actor.observe(Some(1.0), None, Some(1));
            actor.event(LoungeEvent::PlaybackSpeed { speed: 1.0 });
            assert!(Arc::ptr_eq(&actor.segments, &segments));
            assert!(
                actor.jobs.is_empty(),
                "completed metadata must not be refetched"
            );
        }
    }

    #[tokio::test]
    async fn failed_segment_lookup_retries_on_next_observation_without_duplicate_fetches() {
        let mut actor = actor();
        actor.snapshot.video_id = "current".into();
        let identity = Identity::of(&actor.snapshot);
        let (send, receive) = tokio::sync::oneshot::channel();
        actor.fetch = Some(actor.jobs.spawn(async move {
            Completion::Segments(identity, "current".into(), receive.await.unwrap())
        }));
        send.send(Err(ApiError::Status {
            service: "SponsorBlock",
            status: 503,
        }))
        .unwrap();
        let completion = actor.jobs.join_next().await.unwrap().unwrap();
        actor.complete(completion);
        assert!(actor.fetch.is_none());
        assert!(
            actor.jobs.is_empty(),
            "failure alone must not trigger a retry"
        );

        actor.observe(Some(1.0), None, Some(1));
        assert!(actor.fetch.is_some());
        assert_eq!(actor.jobs.len(), 1);
        actor.observe(Some(2.0), None, Some(2));
        actor.event(LoungeEvent::PlaybackSpeed { speed: 2.0 });
        assert_eq!(
            actor.jobs.len(),
            1,
            "pending retry must survive observations"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn successful_segment_results_refresh_only_after_deadline_on_observation() {
        for segments in [
            Arc::<[Segment]>::from([]),
            Arc::from([Segment {
                start: 0.0,
                end: 5.0,
                uuids: vec![],
            }]),
        ] {
            let mut actor = actor();
            actor.snapshot.video_id = "current".into();
            actor.complete(Completion::Segments(
                Identity::of(&actor.snapshot),
                "current".into(),
                Ok(segments.clone()),
            ));
            tokio::time::advance(Duration::from_secs(299)).await;
            actor.observe(Some(1.0), None, Some(1));
            assert!(actor.jobs.is_empty());
            tokio::time::advance(Duration::from_secs(1)).await;
            assert!(actor.jobs.is_empty(), "refresh requires an observation");
            actor.observe(Some(2.0), None, Some(1));
            assert!(actor.fetch.is_some());
            assert_eq!(actor.jobs.len(), 1);
            assert!(
                Arc::ptr_eq(&actor.segments, &segments),
                "refresh must retain completed metadata until replacement"
            );
            actor.observe(Some(3.0), None, Some(1));
            assert_eq!(actor.jobs.len(), 1, "refresh must not be restarted");
        }
    }

    #[tokio::test]
    async fn stale_video_and_session_segment_results_cannot_change_scheduling() {
        let mut actor = actor();
        actor.snapshot.video_id = "current".into();
        let identity = Identity::of(&actor.snapshot);
        let segments: Arc<[Segment]> = Arc::from([Segment {
            start: 0.0,
            end: 5.0,
            uuids: vec![],
        }]);
        for (stale_identity, video) in [
            (
                Identity {
                    connection: identity.connection + 1,
                    ..identity
                },
                "current",
            ),
            (
                Identity {
                    video: identity.video + 1,
                    ..identity
                },
                "current",
            ),
            (identity, "previous"),
        ] {
            actor.complete(Completion::Segments(
                stale_identity,
                video.into(),
                Ok(segments.clone()),
            ));
            assert!(actor.segments.is_empty());
        }
        actor.complete(Completion::Segments(
            identity,
            "current".into(),
            Ok(segments),
        ));
        assert_eq!(actor.segments.len(), 1);
    }

    #[tokio::test]
    async fn video_and_session_changes_cancel_pending_segment_lookup() {
        for reconnect in [false, true] {
            let mut actor = actor();
            actor.snapshot.video_id = "current".into();
            actor.segments = Arc::from([Segment {
                start: 0.0,
                end: 5.0,
                uuids: vec![],
            }]);
            let cancel = actor.video_cancel.clone();
            let pending = tokio::spawn(std::future::pending::<Completion>());
            actor.fetch = Some(pending.abort_handle());

            if reconnect {
                actor.reset(false);
            } else {
                actor.event(LoungeEvent::NowPlaying {
                    video_id: "next".into(),
                    position: Some(0.0),
                    duration: None,
                    state: 1,
                });
                assert!(actor.fetch.is_some(), "new video needs its own lookup");
            }
            assert!(cancel.is_cancelled());
            assert!(actor.segments.is_empty());
            assert!(matches!(pending.await, Err(error) if error.is_cancelled()));
        }
    }

    #[tokio::test(start_paused = true)]
    async fn reset_invalidates_all_unsent_work_and_retries_after_ten_seconds() {
        let mut actor = actor();
        dispatched(&mut actor);
        actor.token = Some("private-token".into());
        actor.sid = "sid".into();
        actor.gsession = "gsession".into();
        actor.enqueue(Command::SkipAd);
        let before = Identity::of(&actor.snapshot);
        let now = Instant::now();
        actor.reset(true);
        assert_eq!(actor.retry_at, now + RETRY);
        assert!(actor.token.is_none());
        assert!(actor.ids().is_none());
        assert!(actor.queue.is_empty());
        assert!(actor.suppressed.is_empty());
        assert!(Identity::of(&actor.snapshot) != before);
        assert!(
            actor.in_flight.is_some(),
            "sent attempt survives until completion"
        );
        actor.complete(Completion::Command(17, Err(LoungeError::MissingSession)));
        assert!(actor.in_flight.is_none());
        assert!(actor.queue.is_empty());
    }

    #[tokio::test]
    async fn screen_recovery_is_attempted_only_once_per_actor_lifetime() {
        let mut actor = actor();
        actor.token = Some("private-token".into());
        actor.handle_error(LoungeError::ConnectionDenied);
        assert!(actor.recovery_used);
        assert_eq!(actor.jobs.len(), 1);
        let epoch = actor.snapshot.connection_epoch;
        actor.complete(Completion::Recovered(epoch, Ok(())));
        assert!(actor.token.is_none());
        actor.token = Some("new-token".into());
        actor.handle_error(LoungeError::ConnectionDenied);
        assert_eq!(
            actor.jobs.len(),
            1,
            "second denial must not spawn another recovery"
        );
    }

    #[tokio::test]
    async fn kids_profile_drops_commands_and_shorts_recovery_is_single_use() {
        let mut actor = actor();
        actor.enqueue(Command::GetNowPlaying);
        actor.event(LoungeEvent::LoungeStatus {
            name: None,
            kids: true,
        });
        assert!(!actor.connected);
        assert!(actor.queue.is_empty());
        actor.event(LoungeEvent::Disconnected { shorts: true });
        actor.event(LoungeEvent::Subtitles { video_id: None });
        assert!(actor.shorts);
        actor.event(LoungeEvent::Subtitles {
            video_id: Some("short-video".into()),
        });
        assert!(!actor.shorts);
        assert_eq!(actor.queue.len(), 1);
        actor.event(LoungeEvent::Subtitles {
            video_id: Some("other-video".into()),
        });
        assert_eq!(actor.queue.len(), 1);
    }

    #[tokio::test]
    async fn invalid_speed_requests_state_and_optional_volume_preserves_ownership() {
        let mut actor = actor();
        actor.event(LoungeEvent::PlaybackSpeed { speed: 0.0 });
        assert_eq!(actor.snapshot.speed, 0.0);
        assert!(matches!(actor.queue[0].command, Command::GetNowPlaying));
        actor.owned_mute = true;
        actor.muted = true;
        actor.event(LoungeEvent::Volume {
            volume: Some(31.0),
            muted: None,
        });
        assert!(actor.owned_mute);
        assert_eq!(actor.volume, 31.0);
        actor.event(LoungeEvent::Volume {
            volume: None,
            muted: Some(false),
        });
        assert!(!actor.owned_mute);
    }

    #[tokio::test(start_paused = true)]
    async fn watchdog_includes_noop_and_daily_refresh_clears_auth() {
        let mut actor = actor();
        actor.connected = true;
        actor.token = Some("token".into());
        tokio::time::advance(Duration::from_secs(60)).await;
        actor.check_timers();
        assert!(actor.connected, "exactly sixty seconds is not expired");
        actor.batch(vec![Envelope {
            aid: 7,
            event: LoungeEvent::Noop,
        }]);
        tokio::time::advance(Duration::from_secs(60)).await;
        actor.check_timers();
        assert!(actor.connected);
        tokio::time::advance(Duration::from_secs(1)).await;
        actor.check_timers();
        assert!(!actor.connected);
        assert!(actor.token.is_some());
        actor.connected = true;
        actor.last_event = Instant::now();
        actor.auth_at = Instant::now();
        actor.check_timers();
        assert!(actor.token.is_none());
        assert!(!actor.connected);
    }

    #[tokio::test]
    async fn absent_position_and_duration_preserve_observations() {
        let mut actor = actor();
        actor.snapshot.position = 12.5;
        actor.duration = Some(90.0);
        actor.event(LoungeEvent::NowPlaying {
            video_id: String::new(),
            position: None,
            duration: None,
            state: 0,
        });
        assert_eq!(actor.snapshot.position, 12.5);
        assert_eq!(actor.duration, Some(90.0));
    }

    #[tokio::test]
    async fn rejected_mute_never_authorizes_restoring_a_users_volume() {
        let mut actor = actor();
        actor.owned_mute = true;
        actor.in_flight = Some(Attempt {
            id: 1,
            identity: Identity::of(&actor.snapshot),
            uuids: vec![],
            seek: false,
            volume_change: Some((true, 0)),
        });
        actor.restore_mute();
        assert_eq!(actor.queue.len(), 1);
        actor.complete(Completion::Command(1, Err(LoungeError::Status(500))));
        assert!(!actor.owned_mute);
        assert!(actor.queue.is_empty());
    }

    #[tokio::test]
    async fn reconnect_preserves_owned_mute_and_last_volume_for_restoration() {
        let mut actor = actor();
        actor.owned_mute = true;
        actor.muted = true;
        actor.volume = 27.0;
        actor.reset(false);
        actor.restore_mute();
        assert!(matches!(
            actor.queue[0].command,
            Command::SetVolume {
                volume: 27.0,
                muted: false
            }
        ));
    }

    #[tokio::test]
    async fn failed_restoration_retains_ownership_until_success_or_observation() {
        let mut actor = actor();
        actor.connected = true;
        actor.token = Some("token".into());
        actor.sid = "sid".into();
        actor.gsession = "session".into();
        actor.owned_mute = true;
        actor.muted = true;
        actor.restore_mute();
        actor.dispatch_queued();
        assert!(actor.owned_mute, "dispatch is not proof of restoration");
        let id = actor.in_flight.as_ref().unwrap().id;
        actor.complete(Completion::Command(id, Err(LoungeError::Status(500))));
        assert!(actor.owned_mute);
        actor.restore_mute();
        actor.dispatch_queued();
        let id = actor.in_flight.as_ref().unwrap().id;
        actor.complete(Completion::Command(id, Ok(())));
        assert!(!actor.owned_mute);
        assert!(!actor.muted);
    }

    #[tokio::test]
    async fn confirmed_mute_survives_ambiguous_http_failure() {
        let mut actor = actor();
        actor.owned_mute = true;
        actor.in_flight = Some(Attempt {
            id: 1,
            identity: Identity::of(&actor.snapshot),
            uuids: vec![],
            seek: false,
            volume_change: Some((true, actor.volume_revision)),
        });
        actor.event(LoungeEvent::Volume {
            volume: Some(37.0),
            muted: Some(true),
        });
        actor.restore_mute();
        actor.complete(Completion::Command(1, Err(LoungeError::Status(500))));
        assert!(actor.owned_mute);
        assert!(matches!(
            actor.queue[0].command,
            Command::SetVolume {
                volume: 37.0,
                muted: false
            }
        ));
    }

    #[tokio::test]
    async fn new_ad_invalidates_pending_restore_and_ad_end_requests_playback() {
        let mut actor = actor();
        actor.muted = true;
        actor.owned_mute = true;
        actor.ad(true);
        actor.event(LoungeEvent::AdState {
            ended: true,
            skip_enabled: false,
        });
        assert!(
            actor
                .queue
                .iter()
                .any(|queued| matches!(queued.command, Command::GetNowPlaying))
        );
        assert!(
            !actor
                .queue
                .iter()
                .any(|queued| matches!(queued.command, Command::SkipAd))
        );
        actor.ad(true);
        assert!(
            !actor
                .queue
                .iter()
                .any(|queued| matches!(queued.command, Command::SetVolume { muted: false, .. }))
        );
        assert!(actor.owned_mute);
    }

    #[tokio::test]
    async fn new_ad_after_restore_dispatch_is_remuted_after_restore_completes() {
        for observed_before_response in [false, true] {
            let mut actor = actor();
            actor.owned_mute = true;
            actor.muted = true;
            actor.in_flight = Some(Attempt {
                id: 1,
                identity: Identity::of(&actor.snapshot),
                uuids: vec![],
                seek: false,
                volume_change: Some((false, actor.volume_revision)),
            });
            actor.ad(false);
            if observed_before_response {
                actor.event(LoungeEvent::Volume {
                    volume: None,
                    muted: Some(false),
                });
            }
            actor.complete(Completion::Command(1, Ok(())));
            assert!(
                actor
                    .queue
                    .iter()
                    .any(|queued| matches!(queued.command, Command::SetVolume { muted: true, .. }))
            );
        }
    }
}
