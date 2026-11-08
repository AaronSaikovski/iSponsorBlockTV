//! Native mDNS/metadata discovery feeding the shared bounded probe coordinator.
//! Unclassified metadata hosts retry until classified; endpoint changes invalidate
//! queued MDX snapshots without resetting the session's independent launch claims.
// Metadata shapes and polling policy adapted from pychromecast 14.0.10 dial.py
// and discovery.py, Copyright (c) 2013 Paulus Schoutsen, MIT License:
// Permission is hereby granted, free of charge, to any person obtaining a copy
// of this software and associated documentation files (the "Software"), to deal
// in the Software without restriction, including without limitation the rights
// to use, copy, modify, merge, publish, distribute, sublicense, and/or sell copies
// of the Software, and to permit persons to whom the Software is furnished to do
// so, subject to the following conditions: The above copyright notice and this
// permission notice shall be included in all copies or substantial portions of
// the Software. THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND,
// EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF
// MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO
// EVENT SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES
// OR OTHER LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE,
// ARISING FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER
// DEALINGS IN THE SOFTWARE.
use super::{
    BackendEvent, Candidate as BackendCandidate, DiscoveredDevice, DiscoveryMode, ProbeContext,
    cast_wire,
};
use crate::http::{CastLocalClient, HttpClients};
use mdns_sd::{ResolvedService, ScopedIp, ServiceDaemon, ServiceEvent};
use serde_json::Value;
use std::{
    collections::{HashMap, HashSet},
    net::{IpAddr, SocketAddr, SocketAddrV6},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    sync::mpsc,
    time::{Instant, interval, sleep_until},
};
use tokio_util::sync::CancellationToken;

const SERVICE: &str = "_googlecast._tcp.local.";
const LIMIT: usize = 256;

#[derive(Clone, Debug)]
pub struct Subnet {
    pub address: IpAddr,
    pub mask: IpAddr,
    pub index: u32,
}
impl Subnet {
    pub fn contains(&self, address: SocketAddr) -> bool {
        match (self.address, self.mask, address) {
            (IpAddr::V4(local), IpAddr::V4(mask), SocketAddr::V4(remote)) => {
                let mask = u32::from(mask);
                let remote = u32::from(*remote.ip());
                let host = remote & !mask;
                (u32::from(local) & mask == remote & mask)
                    && (!mask <= 1 || (host != 0 && host != !mask))
            }
            (IpAddr::V6(local), IpAddr::V6(mask), SocketAddr::V6(remote)) => {
                (!remote.ip().is_unicast_link_local() || remote.scope_id() == self.index)
                    && u128::from(local) & u128::from(mask)
                        == u128::from(*remote.ip()) & u128::from(mask)
            }
            _ => false,
        }
    }
}

pub fn unicast(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(ip) => {
            !ip.is_unspecified() && !ip.is_loopback() && !ip.is_multicast() && !ip.is_broadcast()
        }
        IpAddr::V6(ip) => {
            !ip.is_unspecified()
                && !ip.is_loopback()
                && !ip.is_multicast()
                && ip.to_ipv4().is_none()
        }
    }
}

pub fn polling_allowed(audio: bool, model: &str) -> bool {
    audio && !model.starts_with("HK") && !model.starts_with("JBL")
}

#[derive(Clone, Debug)]
pub struct DeviceMetadata {
    pub uuid: Option<String>,
    pub name: String,
    pub model: String,
    pub audio: bool,
    pub multizone: bool,
}
pub fn device_metadata(value: &Value) -> Result<DeviceMetadata, &'static str> {
    if !value.is_object() {
        return Err("Invalid Cast metadata");
    }
    let device = value.get("device_info");
    let text = |key: &str| device.and_then(|d| d.get(key)).and_then(Value::as_str);
    let capabilities = device.and_then(|d| d.get("capabilities"));
    Ok(DeviceMetadata {
        uuid: text("ssdp_udn")
            .or_else(|| value.get("ssdp_udn").and_then(Value::as_str))
            .and_then(normalize_uuid),
        name: text("name")
            .or_else(|| value.get("name").and_then(Value::as_str))
            .unwrap_or("Unknown Chromecast")
            .into(),
        model: text("model_name").unwrap_or("Unknown model name").into(),
        audio: capabilities
            .and_then(|c| c.get("display_supported"))
            .and_then(Value::as_bool)
            == Some(false),
        multizone: device.is_some()
            && capabilities
                .and_then(|c| c.get("multizone_supported"))
                .and_then(Value::as_bool)
                .unwrap_or(true),
    })
}
fn normalize_uuid(value: &str) -> Option<String> {
    uuid::Uuid::parse_str(value.strip_prefix("uuid:").unwrap_or(value))
        .ok()
        .map(|id| id.simple().to_string())
}

#[derive(Clone, Debug)]
pub struct GroupEndpoint {
    pub uuid: Option<String>,
    pub name: String,
    pub address: SocketAddr,
    pub known_port: bool,
}
/// Only literal, on-link addresses may become learned hosts. A remote leader's
/// advertised eureka port is not its Cast port: learn that host at 8009 instead.
pub fn group_endpoints(value: &Value, host: SocketAddr, subnets: &[Subnet]) -> Vec<GroupEndpoint> {
    let mut result = Vec::new();
    for kind in ["dynamic_groups", "groups"] {
        let Some(groups) = value
            .get("multizone")
            .and_then(|v| v.get(kind))
            .and_then(Value::as_array)
        else {
            continue;
        };
        for group in groups.iter().take(LIMIT) {
            let leader = group
                .get("elected_leader")
                .and_then(Value::as_str)
                .unwrap_or("");
            let (address, known_port) = if leader == "self" {
                let Some(port) = group
                    .get("cast_port")
                    .and_then(Value::as_u64)
                    .and_then(|p| u16::try_from(p).ok())
                    .filter(|p| *p > 0)
                else {
                    continue;
                };
                let mut address = host;
                address.set_port(port);
                (address, true)
            } else {
                let parsed = leader.parse::<SocketAddr>().ok().or_else(|| {
                    let (ip, port) = leader.rsplit_once(':')?;
                    port.parse::<u16>().ok()?;
                    Some(SocketAddr::new(
                        ip.trim_matches(['[', ']']).parse::<IpAddr>().ok()?,
                        8009,
                    ))
                });
                let Some(mut address) = parsed else {
                    continue;
                };
                address.set_port(8009);
                if let SocketAddr::V6(v6) = &mut address
                    && v6.ip().is_unicast_link_local()
                    && v6.scope_id() == 0
                    && let Some(subnet) = subnets.iter().find(|subnet| subnet.contains(host))
                {
                    v6.set_scope_id(subnet.index);
                }
                (address, false)
            };
            if !unicast(address.ip()) || !subnets.iter().any(|subnet| subnet.contains(address)) {
                continue;
            }
            result.push(GroupEndpoint {
                uuid: group
                    .get("uuid")
                    .and_then(Value::as_str)
                    .and_then(normalize_uuid),
                name: group
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("Unknown group name")
                    .into(),
                address,
                known_port,
            });
            if result.len() == LIMIT {
                return result;
            }
        }
    }
    result
}

#[derive(Clone)]
struct Source {
    name: String,
    addresses: Vec<SocketAddr>,
}
#[derive(Default)]
struct Device {
    mdns: HashMap<String, Source>,
    hosts: HashMap<SocketAddr, Source>,
}
impl Device {
    fn source(&self) -> Option<Source> {
        let first = self
            .mdns
            .values()
            .next()
            .or_else(|| self.hosts.values().next())?;
        let mut source = Source {
            name: first.name.clone(),
            addresses: Vec::new(),
        };
        for value in self.mdns.values().chain(self.hosts.values()) {
            for address in &value.addresses {
                if !source.addresses.contains(address) {
                    source.addresses.push(*address);
                }
            }
        }
        Some(source)
    }

    /// Return whether an existing MDX probe's effective address set changed.
    fn set_host_source(&mut self, host: SocketAddr, source: Source) -> bool {
        let prior = self.hosts.get(&host);
        let other_contains = |address: &SocketAddr| {
            self.mdns
                .values()
                .any(|value| value.addresses.contains(address))
                || self
                    .hosts
                    .iter()
                    .any(|(owner, value)| *owner != host && value.addresses.contains(address))
        };
        let changed = (!self.mdns.is_empty() || !self.hosts.is_empty())
            && (source.addresses.iter().any(|address| {
                !other_contains(address)
                    && !prior.is_some_and(|value| value.addresses.contains(address))
            }) || prior.is_some_and(|value| {
                value
                    .addresses
                    .iter()
                    .any(|address| !other_contains(address) && !source.addresses.contains(address))
            }));
        self.hosts.insert(host, source);
        changed
    }
}
struct Host {
    model: String,
    audio: bool,
    attempted: bool,
    classified: bool,
    failures: u8,
    next: Instant,
}
impl Host {
    fn take_poll(&mut self, now: Instant) -> bool {
        if now < self.next
            || (self.attempted && !polling_allowed(!self.classified || self.audio, &self.model))
        {
            return false;
        }
        self.attempted = true;
        self.next = now + Duration::from_secs(30);
        true
    }

    fn metadata_success(&mut self, metadata: &DeviceMetadata) {
        self.failures = 0;
        self.classified = true;
        self.audio = metadata.audio;
        // mDNS model is primary, particularly the known unsafe polling prefixes.
        if self.model.is_empty()
            || metadata.model.starts_with("HK")
            || metadata.model.starts_with("JBL")
        {
            self.model = metadata.model.clone();
        }
    }
}
struct Registry {
    devices: HashMap<String, Device>,
    hosts: HashMap<SocketAddr, Host>,
    subnets: Vec<Subnet>,
    overflow: bool,
}
impl Registry {
    fn room(&mut self) -> bool {
        let room = self.devices.len() + self.hosts.len() < LIMIT;
        self.overflow |= !room;
        room
    }
    fn learn_host(&mut self, mut address: SocketAddr, model: &str) -> bool {
        address.set_port(8009);
        if let Some(record) = self.hosts.get_mut(&address) {
            // Group leaders start without a model; later mDNS must still apply
            // explicit polling exclusions without resetting the retry deadline.
            if !model.is_empty()
                && (record.model.is_empty() || model.starts_with("HK") || model.starts_with("JBL"))
            {
                record.model = model.into();
            }
            return false;
        }
        if !self.room() {
            return false;
        }
        let audio = matches!(
            model.to_ascii_lowercase().as_str(),
            "chromecast audio"
                | "google home"
                | "google home mini"
                | "google home max"
                | "google nest mini"
                | "nest audio"
        );
        self.hosts.insert(
            address,
            Host {
                model: model.into(),
                audio,
                attempted: false,
                classified: false,
                failures: 0,
                next: Instant::now(),
            },
        );
        true
    }
    fn remove_source(&mut self, service: &str) -> Vec<String> {
        let mut removed = Vec::new();
        self.devices.retain(|uuid, device| {
            device.mdns.remove(service);
            let keep = !device.mdns.is_empty() || !device.hosts.is_empty();
            if !keep {
                removed.push(uuid.clone());
            }
            keep
        });
        removed
    }
    fn remove_host_sources(&mut self, host: SocketAddr) -> Vec<String> {
        let mut removed = Vec::new();
        self.devices.retain(|uuid, device| {
            device.hosts.remove(&host);
            let keep = !device.mdns.is_empty() || !device.hosts.is_empty();
            if !keep {
                removed.push(uuid.clone());
            }
            keep
        });
        removed
    }
    fn metadata_failure(&mut self, host: SocketAddr) -> Vec<String> {
        let Some(record) = self.hosts.get_mut(&host) else {
            return Vec::new();
        };
        record.failures = record.failures.saturating_add(1);
        if record.failures >= 5 {
            self.remove_host_sources(host)
        } else {
            Vec::new()
        }
    }
}

type Shared = Arc<Mutex<Registry>>;
#[derive(Clone)]
enum Work {
    Mdx(String),
    Metadata(SocketAddr),
}
#[derive(Clone)]
pub(crate) struct Candidate {
    work: Work,
    registry: Shared,
    tx: mpsc::Sender<BackendEvent>,
}
impl Candidate {
    pub(crate) fn keys(&self) -> Vec<String> {
        match &self.work {
            Work::Metadata(host) => vec![format!("cast-meta:{host}")],
            Work::Mdx(uuid) => {
                let mut keys = vec![format!("cast-uuid:{uuid}")];
                if let Some(source) = self
                    .registry
                    .lock()
                    .unwrap()
                    .devices
                    .get(uuid)
                    .and_then(Device::source)
                {
                    keys.extend(
                        source
                            .addresses
                            .iter()
                            .map(|address| format!("cast-endpoint:{address}")),
                    );
                }
                keys
            }
        }
    }
    pub(crate) fn retry_interval(&self) -> Duration {
        Duration::from_secs(if matches!(self.work, Work::Metadata(_)) {
            30
        } else {
            15
        })
    }
}

async fn admit(
    tx: &mpsc::Sender<BackendEvent>,
    registry: &Shared,
    work: Work,
    refresh: bool,
) -> bool {
    let candidate = BackendCandidate::Cast(Candidate {
        work,
        registry: registry.clone(),
        tx: tx.clone(),
    });
    let observed_at = Instant::now();
    let event = if refresh {
        BackendEvent::Refresh {
            candidate,
            observed_at,
        }
    } else {
        BackendEvent::Candidate {
            candidate,
            observed_at,
        }
    };
    tx.send(event).await.is_ok()
}
async fn remove(tx: &mpsc::Sender<BackendEvent>, uuids: Vec<String>) {
    for uuid in uuids {
        if tx
            .send(BackendEvent::Remove {
                key: format!("cast-uuid:{uuid}"),
            })
            .await
            .is_err()
        {
            break;
        }
    }
}

fn resolved(
    registry: &mut Registry,
    service: &ResolvedService,
) -> (Vec<String>, Vec<String>, bool) {
    if service.get_fullname().contains("._sub.") {
        return (vec![], vec![], false);
    }
    let Some(uuid) = service
        .get_property("id")
        .and_then(|p| normalize_uuid(p.val_str()))
    else {
        return (vec![], vec![], false);
    };
    if registry.devices.get(&uuid).is_some_and(|device| {
        device.mdns.len() >= LIMIT && !device.mdns.contains_key(service.get_fullname())
    }) {
        registry.overflow = true;
        return (vec![], vec![], false);
    }
    let name = service
        .get_property("fn")
        .map(|p| p.val_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("Unknown Chromecast")
        .to_owned();
    let model = service
        .get_property("md")
        .map(|p| p.val_str())
        .unwrap_or("");
    let port = if service.get_port() == 0 {
        8009
    } else {
        service.get_port()
    };
    let addresses: Vec<_> = service
        .get_addresses()
        .iter()
        .filter_map(|ip| {
            let address = match ip {
                ScopedIp::V4(v4) => SocketAddr::new(IpAddr::V4(*v4.addr()), port),
                ScopedIp::V6(v6) => {
                    SocketAddr::V6(SocketAddrV6::new(*v6.addr(), port, 0, v6.scope_id().index))
                }
                _ => return None,
            };
            (unicast(address.ip())
                && registry
                    .subnets
                    .iter()
                    .any(|subnet| subnet.contains(address)))
            .then_some(address)
        })
        .take(LIMIT)
        .collect();
    let changed = registry
        .devices
        .get(&uuid)
        .and_then(|device| device.mdns.get(service.get_fullname()))
        .is_some_and(|source| {
            source.addresses.len() != addresses.len()
                || source
                    .addresses
                    .iter()
                    .any(|address| !addresses.contains(address))
        });
    let mut removed = registry.remove_source(service.get_fullname());
    if addresses.is_empty() {
        return (vec![], removed, false);
    }
    if !registry.devices.contains_key(&uuid) && !registry.room() {
        return (vec![], removed, false);
    }
    registry
        .devices
        .entry(uuid.clone())
        .or_default()
        .mdns
        .insert(
            service.get_fullname().into(),
            Source {
                name,
                addresses: addresses.clone(),
            },
        );
    removed.retain(|old| old != &uuid);
    for address in addresses {
        registry.learn_host(address, model);
    }
    (vec![uuid], removed, changed)
}

/// mDNS daemon owns its implementation thread; explicit shutdown is awaited on
/// every exit. Network probes run exclusively in the central eight-worker pool.
pub(crate) async fn collect(
    tx: mpsc::Sender<BackendEvent>,
    cancel: CancellationToken,
    admission_end: Option<Instant>,
    _http: HttpClients,
) {
    let interfaces = match if_addrs::get_if_addrs() {
        Ok(interfaces) => interfaces,
        Err(_) => {
            let _ = tx
                .send(BackendEvent::Warning {
                    backend: "Cast",
                    message: "Unable to enumerate Cast LAN interfaces",
                })
                .await;
            return;
        }
    };
    let subnets = interfaces
        .into_iter()
        .filter(|i| i.is_oper_up() && !i.is_loopback())
        .map(|i| {
            let mask = match &i.addr {
                if_addrs::IfAddr::V4(v) => IpAddr::V4(v.netmask),
                if_addrs::IfAddr::V6(v) => IpAddr::V6(v.netmask),
            };
            Subnet {
                address: i.ip(),
                mask,
                index: i.index.unwrap_or(0),
            }
        })
        .collect::<Vec<_>>();
    if subnets.is_empty() {
        let _ = tx
            .send(BackendEvent::Warning {
                backend: "Cast",
                message: "No usable Cast LAN interfaces",
            })
            .await;
        return;
    }
    let daemon = match ServiceDaemon::new() {
        Ok(daemon) => daemon,
        Err(_) => {
            let _ = tx
                .send(BackendEvent::Warning {
                    backend: "Cast",
                    message: "Unable to start Cast mDNS discovery",
                })
                .await;
            return;
        }
    };
    let events = daemon.browse(SERVICE);
    if let Ok(events) = events {
        let registry = Arc::new(Mutex::new(Registry {
            devices: HashMap::new(),
            hosts: HashMap::new(),
            subnets,
            overflow: false,
        }));
        let operation = async {
            let mut tick = interval(Duration::from_secs(1));
            let mut warned = false;
            loop {
                tokio::select! {
                    event = events.recv_async() => match event {
                        Ok(ServiceEvent::ServiceResolved(service)) => {
                            let (uuids, removed, refresh) = resolved(&mut registry.lock().unwrap(), &service);
                            remove(&tx, removed).await;
                            for uuid in uuids { if !admit(&tx, &registry, Work::Mdx(uuid), refresh).await { return; } }
                        }
                        Ok(ServiceEvent::ServiceRemoved(_, name)) => {
                            let removed = registry.lock().unwrap().remove_source(&name);
                            remove(&tx, removed).await;
                        }
                        Err(_) => { let _ = tx.send(BackendEvent::Warning { backend: "Cast", message: "Cast mDNS discovery stopped unexpectedly" }).await; return; }
                        _ => {},
                    },
                    _ = tick.tick() => {
                        let (metadata, devices, overflow) = {
                            let mut registry = registry.lock().unwrap();
                            let now = Instant::now();
                            let metadata = registry.hosts.iter_mut().filter_map(|(address, host)| {
                                host.take_poll(now).then_some(*address)
                            }).collect::<Vec<_>>();
                            (metadata, registry.devices.keys().cloned().collect::<Vec<_>>(), registry.overflow)
                        };
                        for address in metadata { if !admit(&tx, &registry, Work::Metadata(address), false).await { return; } }
                        // Central admission enforces the 15-second unsuccessful retry floor.
                        for uuid in devices { if !admit(&tx, &registry, Work::Mdx(uuid), false).await { return; } }
                        if overflow && !warned {
                            warned = true;
                            let _ = tx.send(BackendEvent::Warning { backend: "Cast", message: "Cast candidate limit (256) reached" }).await;
                        }
                    }
                }
            }
        };
        tokio::select! {
            _ = cancel.cancelled() => {},
            _ = async { if let Some(end) = admission_end { sleep_until(end).await; } else { std::future::pending::<()>().await; } } => {},
            _ = operation => {},
        }
    } else {
        let _ = tx
            .send(BackendEvent::Warning {
                backend: "Cast",
                message: "Unable to browse Cast mDNS service",
            })
            .await;
    }
    let _ = daemon.stop_browse(SERVICE);
    loop {
        match daemon.shutdown() {
            Ok(stopped) => {
                let _ = stopped.recv_async().await;
                break;
            }
            Err(mdns_sd::Error::Again) => tokio::time::sleep(Duration::from_millis(10)).await,
            Err(_) => break,
        }
    }
}

async fn metadata_get(
    http: &CastLocalClient,
    host: SocketAddr,
    path: &str,
    secure: bool,
) -> Result<Value, &'static str> {
    let mut address = host;
    address.set_port(if secure { 8443 } else { 8008 });
    let mut response = http
        .get_metadata(address, path, false)
        .await
        .map_err(|_| "Cast metadata request failed")?;
    if response.status == reqwest::StatusCode::FORBIDDEN {
        response = http
            .get_metadata(address, path, true)
            .await
            .map_err(|_| "Cast metadata request failed")?;
    }
    if !response.status.is_success() {
        return Err("Cast metadata returned an unsuccessful status");
    }
    serde_json::from_slice(&response.body).map_err(|_| "Invalid Cast metadata JSON")
}

async fn metadata_probe(
    host: SocketAddr,
    candidate: &Candidate,
    context: &ProbeContext,
) -> Result<Option<DiscoveredDevice>, &'static str> {
    let path = "/setup/eureka_info?params=device_info,name";
    let value = match metadata_get(&context.http.cast_local, host, path, true).await {
        Ok(value) => Ok(value),
        Err(_) => metadata_get(&context.http.cast_local, host, path, false).await,
    };
    let metadata = match value.and_then(|value| device_metadata(&value)) {
        Ok(metadata) => metadata,
        Err(error) => {
            let removed = candidate.registry.lock().unwrap().metadata_failure(host);
            remove(&candidate.tx, removed).await;
            return Err(error);
        }
    };
    let groups = if metadata.multizone {
        metadata_get(
            &context.http.cast_local,
            host,
            "/setup/eureka_info?params=multizone",
            true,
        )
        .await
        .ok()
    } else {
        None
    };
    let (uuids, removed, learned) = {
        let mut registry = candidate.registry.lock().unwrap();
        if let Some(record) = registry.hosts.get_mut(&host) {
            record.metadata_success(&metadata);
        }
        let endpoints = groups
            .as_ref()
            .map(|value| group_endpoints(value, host, &registry.subnets))
            .unwrap_or_default();
        let mut sources = Vec::new();
        if let Some(uuid) = metadata.uuid {
            sources.push((
                uuid,
                Source {
                    name: metadata.name,
                    addresses: vec![host],
                },
            ));
        }
        let mut learned = Vec::new();
        for group in endpoints {
            if registry.learn_host(group.address, "") {
                let mut address = group.address;
                address.set_port(8009);
                learned.push(address);
            }
            if group.known_port
                && let Some(uuid) = group.uuid
            {
                sources.push((
                    uuid,
                    Source {
                        name: group.name,
                        addresses: vec![group.address],
                    },
                ));
            }
        }
        let previous: HashSet<_> = registry
            .devices
            .iter()
            .filter(|(_, device)| device.hosts.contains_key(&host))
            .map(|(uuid, _)| uuid.clone())
            .collect();
        let mut uuids = Vec::new();
        let mut removed = Vec::new();
        for (uuid, source) in sources {
            if !registry.devices.contains_key(&uuid) && !registry.room() {
                continue;
            }
            let refresh = registry
                .devices
                .entry(uuid.clone())
                .or_default()
                .set_host_source(host, source);
            uuids.push((uuid, refresh));
        }
        for old in previous {
            if !uuids.iter().any(|(uuid, _)| uuid == &old)
                && let Some(device) = registry.devices.get_mut(&old)
            {
                device.hosts.remove(&host);
                if device.mdns.is_empty() && device.hosts.is_empty() {
                    registry.devices.remove(&old);
                    removed.push(old);
                }
            }
        }
        (uuids, removed, learned)
    };
    remove(&candidate.tx, removed).await;
    for (uuid, refresh) in uuids {
        admit(&candidate.tx, &candidate.registry, Work::Mdx(uuid), refresh).await;
    }
    // Active admission rejects late groups; passive mode queues learned hosts now.
    for address in learned {
        admit(
            &candidate.tx,
            &candidate.registry,
            Work::Metadata(address),
            false,
        )
        .await;
    }
    Ok(None)
}

pub(crate) async fn probe(
    candidate: Candidate,
    context: &ProbeContext,
) -> Result<Option<DiscoveredDevice>, &'static str> {
    match &candidate.work {
        Work::Metadata(host) => metadata_probe(*host, &candidate, context).await,
        Work::Mdx(uuid) => {
            let source = candidate
                .registry
                .lock()
                .unwrap()
                .devices
                .get(uuid)
                .and_then(Device::source);
            let Some(source) = source else {
                return Ok(None);
            };
            let mut failed = false;
            for address in source.addresses {
                if !unicast(address.ip()) {
                    continue;
                }
                let claims = context.claimed_endpoints.clone();
                let launch_keys = [
                    format!("cast-uuid:{uuid}"),
                    format!("cast-endpoint:{address}"),
                ];
                let permission = move || {
                    let mut claimed = claims.lock().unwrap();
                    if launch_keys.iter().any(|key| claimed.contains(key)) {
                        return false;
                    }
                    claimed.extend(launch_keys.iter().cloned());
                    true
                };
                let result = cast_wire::probe_with_permission(
                    address,
                    context.mode == DiscoveryMode::ActiveOnce,
                    CancellationToken::new(),
                    permission,
                )
                .await;
                failed |= result.is_err();
                if let Ok(Some(screen_id)) = result {
                    // mDNS removal or host expiry while the request was in flight
                    // cannot resurrect a device with no remaining source.
                    if candidate
                        .registry
                        .lock()
                        .unwrap()
                        .devices
                        .get(uuid)
                        .and_then(Device::source)
                        .is_some_and(|s| s.addresses.contains(&address))
                    {
                        return Ok(Some(DiscoveredDevice {
                            screen_id,
                            name: source.name,
                        }));
                    }
                    return Ok(None);
                }
            }
            if failed {
                Err("Cast MDX transport or negotiation failed")
            } else {
                Ok(None)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry() -> Registry {
        Registry {
            devices: HashMap::new(),
            hosts: HashMap::new(),
            overflow: false,
            subnets: vec![Subnet {
                address: "192.168.1.2".parse().unwrap(),
                mask: "255.255.255.0".parse().unwrap(),
                index: 1,
            }],
        }
    }
    fn announcement(address: &str, port: u16) -> ResolvedService {
        mdns_sd::ServiceInfo::new(
            SERVICE,
            "fixture",
            "fixture.local.",
            address,
            port,
            [
                ("id", "12345678123412341234123456789abc"),
                ("fn", "[literal] receiver"),
                ("md", "Nest Audio"),
            ]
            .as_slice(),
        )
        .unwrap()
        .as_resolved_service()
    }
    #[test]
    fn mdns_updates_remove_old_endpoints_and_keep_advertised_ports() {
        let mut registry = registry();
        let service = announcement("192.168.1.3", 12345);
        let (ids, removed, refresh) = resolved(&mut registry, &service);
        assert!(!refresh);
        assert!(removed.is_empty());
        assert_eq!(ids.len(), 1);
        let uuid = ids[0].clone();
        assert_eq!(
            registry.devices[&uuid].source().unwrap().addresses[0].port(),
            12345
        );
        assert!(resolved(&mut registry, &service).1.is_empty());
        let update = announcement("192.168.1.4", 54321);
        let (updated, removed, refresh) = resolved(&mut registry, &update);
        assert_eq!(updated, vec![uuid.clone()]);
        assert!(removed.is_empty());
        assert!(refresh);
        assert_eq!(
            registry.devices[&uuid].source().unwrap().addresses,
            vec!["192.168.1.4:54321".parse::<SocketAddr>().unwrap()]
        );
        assert_eq!(registry.remove_source(update.get_fullname()), vec![uuid]);
        assert!(registry.devices.is_empty());
    }
    #[test]
    fn five_failed_audio_polls_remove_only_host_derived_sources() {
        let mut registry = registry();
        let service = announcement("192.168.1.3", 8009);
        let uuid = resolved(&mut registry, &service).0.remove(0);
        let host: SocketAddr = "192.168.1.3:8009".parse().unwrap();
        let source = Source {
            name: "metadata".into(),
            addresses: vec![host],
        };
        registry
            .devices
            .get_mut(&uuid)
            .unwrap()
            .hosts
            .insert(host, source.clone());
        registry
            .devices
            .entry("group".into())
            .or_default()
            .hosts
            .insert(host, source);
        for _ in 0..4 {
            assert!(registry.metadata_failure(host).is_empty());
            assert!(registry.devices.contains_key("group"));
        }
        assert_eq!(registry.metadata_failure(host), vec!["group".to_owned()]);
        assert!(registry.devices.contains_key(&uuid));
        assert!(registry.devices[&uuid].hosts.is_empty());
        assert_eq!(
            registry.devices[&uuid].source().unwrap().name,
            "[literal] receiver"
        );
    }
    #[test]
    fn unclassified_metadata_failure_remains_retryable_until_classified() {
        let mut registry = registry();
        let host = "192.168.1.4:8009".parse().unwrap();
        assert!(registry.learn_host(host, ""));
        let now = registry.hosts[&host].next;
        assert!(registry.hosts.get_mut(&host).unwrap().take_poll(now));
        assert!(registry.metadata_failure(host).is_empty());
        assert!(!registry.learn_host(host, ""));
        assert!(
            !registry
                .hosts
                .get_mut(&host)
                .unwrap()
                .take_poll(now + Duration::from_secs(29))
        );
        assert!(
            registry
                .hosts
                .get_mut(&host)
                .unwrap()
                .take_poll(now + Duration::from_secs(30))
        );
        let metadata = device_metadata(&serde_json::json!({
            "device_info": {
                "model_name": "Recovered display",
                "capabilities": { "display_supported": true }
            }
        }))
        .unwrap();
        registry
            .hosts
            .get_mut(&host)
            .unwrap()
            .metadata_success(&metadata);
        assert_eq!(registry.hosts[&host].failures, 0);
        assert!(
            !registry
                .hosts
                .get_mut(&host)
                .unwrap()
                .take_poll(now + Duration::from_secs(60))
        );
    }

    #[test]
    fn metadata_polling_preserves_audio_policy_and_model_exclusions() {
        for (model, audio, retries) in [
            ("Nest Audio", true, true),
            ("Display", false, false),
            ("HK speaker", true, false),
            ("JBL speaker", true, false),
        ] {
            let mut registry = registry();
            let host = "192.168.1.4:8009".parse().unwrap();
            registry.learn_host(host, model);
            let now = registry.hosts[&host].next;
            let record = registry.hosts.get_mut(&host).unwrap();
            assert!(record.take_poll(now));
            if model.starts_with("HK") || model.starts_with("JBL") {
                // Even failed initial metadata must not repeatedly poll these models.
                assert!(!record.take_poll(now + Duration::from_secs(30)));
            }
            let metadata = device_metadata(&serde_json::json!({
                "device_info": {
                    "model_name": "Metadata model",
                    "capabilities": { "display_supported": !audio }
                }
            }))
            .unwrap();
            record.metadata_success(&metadata);
            assert_eq!(record.take_poll(now + Duration::from_secs(60)), retries);
        }
    }

    #[test]
    fn learned_host_applies_later_mdns_polling_exclusions() {
        for model in ["HK speaker", "JBL speaker"] {
            let mut registry = registry();
            let host = "192.168.1.4:8009".parse().unwrap();
            registry.learn_host(host, "");
            let now = registry.hosts[&host].next;
            assert!(registry.hosts.get_mut(&host).unwrap().take_poll(now));
            registry.metadata_failure(host);
            assert!(!registry.learn_host(host, model));
            assert!(
                !registry
                    .hosts
                    .get_mut(&host)
                    .unwrap()
                    .take_poll(now + Duration::from_secs(30))
            );
        }
    }

    #[test]
    fn first_metadata_alternate_invalidates_existing_mdns_addresses() {
        let mut registry = registry();
        let uuid = resolved(&mut registry, &announcement("192.168.1.3", 8009))
            .0
            .remove(0);
        let device = registry.devices.get_mut(&uuid).unwrap();
        let original = device.source().unwrap();
        let host = "192.168.1.4:8009".parse().unwrap();
        let alternate = "192.168.1.4:12345".parse().unwrap();
        let source = Source {
            name: "Metadata group".into(),
            addresses: vec![alternate],
        };
        assert!(device.set_host_source(host, source.clone()));
        assert_eq!(
            original.addresses,
            vec!["192.168.1.3:8009".parse::<SocketAddr>().unwrap()]
        );
        assert!(device.source().unwrap().addresses.contains(&alternate));
        assert!(!device.set_host_source(host, source));
        // Removing the alternate also changes the effective address set.
        assert!(device.set_host_source(host, original.clone()));
        // A second host advertising only an existing mDNS address adds no endpoint.
        assert!(!device.set_host_source("192.168.1.5:8009".parse().unwrap(), original,));
    }

    #[test]
    fn metadata_source_changes_do_not_invalidate_unchanged_effective_addresses() {
        let mut device = Device::default();
        let host = "192.168.1.3:8009".parse().unwrap();
        let alternate = "192.168.1.4:8009".parse().unwrap();
        device.mdns.insert(
            "fixture".into(),
            Source {
                name: "Receiver".into(),
                addresses: vec![host, alternate],
            },
        );
        assert!(!device.set_host_source(
            host,
            Source {
                name: "Metadata".into(),
                addresses: vec![host],
            }
        ));
        assert!(!device.set_host_source(
            host,
            Source {
                name: "Metadata".into(),
                addresses: vec![alternate],
            }
        ));
    }

    fn coordinate(
        event: BackendEvent,
        admissions: &mut super::super::Admissions,
        cancel: &CancellationToken,
    ) {
        match event {
            BackendEvent::Candidate {
                candidate,
                observed_at,
            } => {
                admissions
                    .admit(candidate, observed_at, false, cancel)
                    .unwrap();
            }
            BackendEvent::Refresh {
                candidate,
                observed_at,
            } => {
                admissions
                    .admit(candidate, observed_at, true, cancel)
                    .unwrap();
            }
            BackendEvent::Remove { key } => {
                let record = admissions.aliases[&key];
                admissions.invalidate(record, cancel);
            }
            BackendEvent::Warning { .. } => panic!("unexpected warning"),
        }
    }

    async fn metadata_refresh_admission(observation_delay: Duration, processing_delay: Duration) {
        let start = Instant::now();
        let end = start + super::super::ADMISSION_WINDOW;
        let cancel = CancellationToken::new();
        let mut admissions = super::super::Admissions::new(DiscoveryMode::ActiveOnce, Some(end));
        let mut registry = registry();
        let uuid = resolved(&mut registry, &announcement("192.168.1.3", 8009))
            .0
            .remove(0);
        let registry = Arc::new(Mutex::new(registry));
        let (tx, mut rx) = mpsc::channel(4);
        assert!(admit(&tx, &registry, Work::Mdx(uuid.clone()), false).await);
        coordinate(rx.recv().await.unwrap(), &mut admissions, &cancel);
        let original = admissions.queue.pop_front().unwrap();
        let original_token = admissions.records[original.record].token.clone();
        let original_generation = original.generation;
        let original_last = admissions.records[original.record].last;

        // The first MDX probe is in flight when metadata learns an alternate.
        tokio::time::advance(observation_delay).await;
        let alternate = "192.168.1.4:12345".parse().unwrap();
        let refresh = registry
            .lock()
            .unwrap()
            .devices
            .get_mut(&uuid)
            .unwrap()
            .set_host_source(
                "192.168.1.4:8009".parse().unwrap(),
                Source {
                    name: "Metadata group".into(),
                    addresses: vec![alternate],
                },
            );
        assert!(refresh);
        assert!(admit(&tx, &registry, Work::Mdx(uuid.clone()), refresh).await);
        let event = rx.recv().await.unwrap();
        assert!(matches!(&event, BackendEvent::Refresh { .. }));
        // Queue backpressure may carry an early observation past the boundary.
        tokio::time::advance(processing_delay).await;
        coordinate(event, &mut admissions, &cancel);
        assert_eq!(admissions.records.len(), 1);
        assert!(admissions.records[original.record].pending);
        if Instant::now() < end {
            assert!(original_token.is_cancelled());
            assert_eq!(admissions.queue.len(), 1);
            let replacement = admissions.queue.front().unwrap();
            assert_eq!(replacement.record, original.record);
            assert_eq!(replacement.generation, original_generation + 1);
            assert_eq!(
                replacement.deadline,
                start + observation_delay + super::super::PROBE_WINDOW
            );
            assert!(replacement.deadline < end + super::super::PROBE_WINDOW);
            assert!(
                replacement
                    .candidate
                    .keys()
                    .contains(&format!("cast-endpoint:{alternate}"))
            );
            assert!(
                admissions
                    .aliases
                    .contains_key(&format!("cast-endpoint:{alternate}"))
            );
        } else {
            assert!(!original_token.is_cancelled());
            assert!(admissions.queue.is_empty());
            assert_eq!(
                admissions.records[original.record].generation,
                original_generation
            );
            assert_eq!(admissions.records[original.record].last, original_last);
            assert!(
                !admissions
                    .aliases
                    .contains_key(&format!("cast-endpoint:{alternate}"))
            );
        }

        // A genuine removal still cancels current work even after admission ends.
        let current_token = admissions.records[original.record].token.clone();
        tokio::time::advance(super::super::ADMISSION_WINDOW).await;
        remove(&tx, vec![uuid]).await;
        coordinate(rx.recv().await.unwrap(), &mut admissions, &cancel);
        assert!(current_token.is_cancelled());
        assert!(!admissions.records[original.record].pending);
        assert!(admissions.queue.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn late_metadata_refresh_preserves_in_flight_probe() {
        metadata_refresh_admission(super::super::ADMISSION_WINDOW, Duration::ZERO).await;
    }

    #[tokio::test(start_paused = true)]
    async fn queued_early_refresh_processed_late_preserves_in_flight_probe() {
        metadata_refresh_admission(Duration::from_secs(4), Duration::from_secs(1)).await;
    }

    #[tokio::test(start_paused = true)]
    async fn early_metadata_refresh_replaces_probe_atomically() {
        metadata_refresh_admission(Duration::from_secs(4), Duration::ZERO).await;
    }
    #[test]
    fn learned_host_bound_and_lan_scope_are_enforced() {
        let mut registry = registry();
        for i in 0..LIMIT {
            assert!(registry.learn_host(
                SocketAddr::new(
                    IpAddr::V4(std::net::Ipv4Addr::new(192, 168, 1, i as u8)),
                    8009
                ),
                ""
            ));
        }
        assert!(!registry.learn_host("192.168.2.1:8009".parse().unwrap(), ""));
        assert!(registry.overflow);
        let mut registry = self::registry();
        assert!(
            resolved(&mut registry, &announcement("127.0.0.1", 8009))
                .0
                .is_empty()
        );
        assert!(
            resolved(&mut registry, &announcement("192.168.2.1", 8009))
                .0
                .is_empty()
        );
    }
}
