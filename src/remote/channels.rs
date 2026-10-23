//! The control channels the workstation daemon holds, one per remote.
//!
//! A channel opens when something needs its remote, stays open while it is
//! in use, and reconnects with backoff after a drop. A remote whose
//! transport reports it stopped is never started to fetch status.

use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use super::channel::Handler;
use super::connection::{Connection, HELLO_TIMEOUT};
use super::transport::{RemoteState, Transport};
use crate::config::{RemoteConfig, TransportKind};

/// Builds the transport for a configured remote.
pub type TransportFactory =
    Arc<dyn Fn(&str, &RemoteConfig) -> Result<Arc<dyn Transport>> + Send + Sync>;

#[derive(Debug, Clone)]
pub struct ChannelPolicy {
    pub hello_timeout: Duration,
    /// First reconnect delay after a drop; doubles per failed attempt.
    pub backoff_initial: Duration,
    pub backoff_max: Duration,
    /// How long an unused channel stays open (and is reconnected).
    pub linger: Duration,
}

impl Default for ChannelPolicy {
    fn default() -> Self {
        Self {
            hello_timeout: HELLO_TIMEOUT,
            backoff_initial: Duration::from_secs(1),
            backoff_max: Duration::from_secs(30),
            linger: Duration::from_secs(60),
        }
    }
}

/// A remote's reachability as `workmux remote status` shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteStatus {
    pub name: String,
    pub transport: TransportKind,
    pub target: String,
    #[serde(flatten)]
    pub reach: Reach,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "lowercase")]
pub enum Reach {
    Reachable {
        workmux: String,
        protocol: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        warning: Option<String>,
    },
    /// The transport reports the remote stopped or missing; not started.
    Stopped,
    Unreachable {
        error: String,
    },
}

/// Answers requests remote workmux sends over its control channel.
struct WorkstationHandler;

impl Handler for WorkstationHandler {
    fn request(&self, method: &str, _params: Value) -> Result<Value, String> {
        Err(format!("unknown method: {method}"))
    }
}

struct Slot {
    config: RemoteConfig,
    connection: Option<Arc<Connection>>,
    last_used: Instant,
    /// Set while a background reconnect loop runs for this remote.
    reconnecting: bool,
}

struct Shared {
    factory: TransportFactory,
    policy: ChannelPolicy,
    slots: Mutex<HashMap<String, Slot>>,
    /// Serializes connection attempts, so a burst of requests opens one
    /// channel per remote.
    connecting: Mutex<()>,
}

/// The workstation daemon's control channels.
#[derive(Clone)]
pub struct Channels {
    shared: Arc<Shared>,
}

impl Channels {
    pub fn new(factory: TransportFactory, policy: ChannelPolicy) -> Self {
        Self {
            shared: Arc::new(Shared {
                factory,
                policy,
                slots: Mutex::new(HashMap::new()),
                connecting: Mutex::new(()),
            }),
        }
    }

    /// Report `name`'s reachability, opening its channel if needed.
    pub fn status(&self, name: &str, config: &RemoteConfig) -> RemoteStatus {
        RemoteStatus {
            name: name.to_string(),
            transport: config.transport,
            target: config.target.clone(),
            reach: self.reach(name, config),
        }
    }

    fn reach(&self, name: &str, config: &RemoteConfig) -> Reach {
        let unreachable = |error: anyhow::Error| Reach::Unreachable {
            error: format!("{error:#}"),
        };
        let transport = match (self.shared.factory)(name, config) {
            Ok(transport) => transport,
            Err(error) => return unreachable(error),
        };
        if let Some(provisioning) = transport.provisioning() {
            match provisioning.status() {
                Ok(RemoteState::Running) => {}
                Ok(RemoteState::Stopped | RemoteState::Missing) => return Reach::Stopped,
                Err(error) => return unreachable(error),
            }
        }
        let connection = match self.channel_with(name, config, transport.as_ref(), true) {
            Ok(connection) => connection,
            Err(error) => return unreachable(error),
        };
        match connection.ping() {
            Ok(ping) => Reach::Reachable {
                workmux: ping.workmux,
                protocol: ping.protocol,
                warning: connection.warning.clone(),
            },
            Err(error) => unreachable(error),
        }
    }

    /// The open channel to `name`, opened on demand.
    #[allow(dead_code)] // Used by commands routed to a remote
    pub fn channel(&self, name: &str, config: &RemoteConfig) -> Result<Arc<Connection>> {
        self.channel_for(name, config, true)
    }

    /// `use_now` marks the channel used; a background reconnect does not, so
    /// it never extends the linger period by itself.
    fn channel_for(
        &self,
        name: &str,
        config: &RemoteConfig,
        use_now: bool,
    ) -> Result<Arc<Connection>> {
        let transport = (self.shared.factory)(name, config)?;
        if let Some(provisioning) = transport.provisioning()
            && provisioning.status()? != RemoteState::Running
        {
            return Err(anyhow!("remote {name} is stopped"));
        }
        self.channel_with(name, config, transport.as_ref(), use_now)
    }

    fn channel_with(
        &self,
        name: &str,
        config: &RemoteConfig,
        transport: &dyn Transport,
        use_now: bool,
    ) -> Result<Arc<Connection>> {
        let _connecting = self.shared.connecting.lock().unwrap();
        {
            let mut slots = self.shared.slots.lock().unwrap();
            if let Some(slot) = slots.get_mut(name) {
                if slot.config == *config
                    && let Some(connection) = &slot.connection
                    && !connection.is_closed()
                {
                    if use_now {
                        slot.last_used = Instant::now();
                    }
                    return Ok(connection.clone());
                }
                // Re-registered under different settings: drop the old channel.
                if slot.config != *config {
                    slot.connection = None;
                    slot.config = config.clone();
                }
            }
        }

        let connection = Arc::new(Connection::open(
            name,
            transport,
            WorkstationHandler,
            self.shared.policy.hello_timeout,
        )?);
        tracing::info!(remote = name, workmux = %connection.hello.workmux, "control channel open");
        let mut slots = self.shared.slots.lock().unwrap();
        let slot = slots.entry(name.to_string()).or_insert_with(|| Slot {
            config: config.clone(),
            connection: None,
            last_used: Instant::now(),
            reconnecting: false,
        });
        slot.connection = Some(connection.clone());
        if use_now {
            slot.last_used = Instant::now();
        }
        drop(slots);
        self.watch(name, &connection);
        Ok(connection)
    }

    /// Reconnect `name` in the background once its channel drops.
    fn watch(&self, name: &str, connection: &Arc<Connection>) {
        let shared = Arc::downgrade(&self.shared);
        let watched = Arc::downgrade(connection);
        let peer = connection.peer.clone();
        let name = name.to_string();
        std::thread::spawn(move || {
            while !peer.wait_closed(Duration::from_secs(3600)) {}
            let Some(shared) = shared.upgrade() else {
                return;
            };
            Channels { shared }.reconnect_after_drop(&name, &watched);
        });
    }

    fn reconnect_after_drop(&self, name: &str, watched: &Weak<Connection>) {
        let dropped = {
            let mut slots = self.shared.slots.lock().unwrap();
            let Some(slot) = slots.get_mut(name) else {
                return;
            };
            // Closed on purpose (idle, re-registered) rather than dropped.
            if !slot
                .connection
                .as_ref()
                .is_some_and(|current| Arc::as_ptr(current) == watched.as_ptr())
            {
                return;
            }
            let dropped = slot.connection.take();
            if slot.reconnecting || !self.wanted(slot) {
                None
            } else {
                slot.reconnecting = true;
                Some((dropped, slot.config.clone()))
            }
        };
        let Some((dropped, config)) = dropped else {
            tracing::warn!(remote = name, "control channel dropped");
            return;
        };
        if let Some(connection) = dropped {
            tracing::warn!(remote = name, reason = connection.failure(), "control channel dropped");
        }

        let mut backoff = self.shared.policy.backoff_initial;
        loop {
            std::thread::sleep(backoff);
            {
                let slots = self.shared.slots.lock().unwrap();
                let Some(slot) = slots.get(name) else {
                    break;
                };
                if slot.connection.is_some() || !self.wanted(slot) || slot.config != config {
                    break;
                }
            }
            match self.channel_for(name, &config, false) {
                Ok(_) => break,
                Err(error) => {
                    tracing::info!(remote = name, error = %format!("{error:#}"), "control channel reconnect failed");
                    backoff = (backoff * 2).min(self.shared.policy.backoff_max);
                }
            }
        }
        if let Some(slot) = self.shared.slots.lock().unwrap().get_mut(name) {
            slot.reconnecting = false;
        }
    }

    fn wanted(&self, slot: &Slot) -> bool {
        slot.last_used.elapsed() < self.shared.policy.linger
    }

    /// Close channels that have gone unused for the linger period.
    pub fn close_idle(&self) {
        let mut slots = self.shared.slots.lock().unwrap();
        for (name, slot) in slots.iter_mut() {
            if slot.connection.is_some() && !self.wanted(slot) {
                tracing::info!(remote = name, "closing idle control channel");
                slot.connection = None;
            }
        }
    }

    /// Number of open channels.
    pub fn open_count(&self) -> usize {
        self.shared
            .slots
            .lock()
            .unwrap()
            .values()
            .filter(|slot| slot.connection.as_ref().is_some_and(|c| !c.is_closed()))
            .count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::handshake::{PROTOCOL_VERSION, SENTINEL};
    use crate::remote::testing::{ScriptTransport, ServeTransport, StoppedTransport};
    use std::sync::atomic::Ordering;

    fn config() -> RemoteConfig {
        RemoteConfig {
            transport: TransportKind::Loopback,
            target: "/remote".to_string(),
            builtin_shims: None,
            host_commands: vec![],
        }
    }

    fn channels_over<T: Transport + 'static>(transport: Arc<T>, policy: ChannelPolicy) -> Channels {
        Channels::new(
            Arc::new(move |_, _| Ok(transport.clone() as Arc<dyn Transport>)),
            policy,
        )
    }

    fn fast_policy() -> ChannelPolicy {
        ChannelPolicy {
            hello_timeout: Duration::from_secs(10),
            backoff_initial: Duration::from_millis(50),
            backoff_max: Duration::from_millis(400),
            linger: Duration::from_secs(60),
        }
    }

    fn wait_until(what: &str, mut condition: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !condition() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn hello_then(after: &str) -> String {
        format!(
            "printf '\\n{SENTINEL} {{\"workmux\":\"{}\",\"protocol\":{PROTOCOL_VERSION}}}\\n'; {after}",
            env!("CARGO_PKG_VERSION")
        )
    }

    #[test]
    fn status_of_a_reachable_remote_shows_its_workmux_and_protocol() {
        let transport = Arc::new(ServeTransport::default());
        let channels = channels_over(transport.clone(), fast_policy());

        let status = channels.status("devbox", &config());

        assert_eq!(
            status,
            RemoteStatus {
                name: "devbox".to_string(),
                transport: TransportKind::Loopback,
                target: "/remote".to_string(),
                reach: Reach::Reachable {
                    workmux: env!("CARGO_PKG_VERSION").to_string(),
                    protocol: PROTOCOL_VERSION,
                    warning: None,
                },
            }
        );
        assert_eq!(channels.open_count(), 1);
    }

    #[test]
    fn one_channel_serves_every_request_for_a_remote() {
        let transport = Arc::new(ServeTransport::default());
        let channels = channels_over(transport.clone(), fast_policy());

        let threads: Vec<_> = (0..4)
            .map(|_| {
                let channels = channels.clone();
                std::thread::spawn(move || channels.status("devbox", &config()))
            })
            .collect();
        for thread in threads {
            assert!(matches!(thread.join().unwrap().reach, Reach::Reachable { .. }));
        }

        assert_eq!(transport.exec_count(), 1);
    }

    #[test]
    fn an_unreachable_remote_shows_the_transports_error() {
        let transport = Arc::new(ScriptTransport::new([
            "echo 'ssh: connect to host devbox port 22: Connection refused' >&2; exit 255",
        ]));
        let channels = channels_over(transport, fast_policy());

        let status = channels.status("devbox", &config());

        assert_eq!(
            status.reach,
            Reach::Unreachable {
                error: "ssh: connect to host devbox port 22: Connection refused".to_string()
            }
        );
        assert_eq!(channels.open_count(), 0);
    }

    #[test]
    fn a_stopped_remote_is_never_started_to_fetch_status() {
        let transport = Arc::new(StoppedTransport::default());
        let channels = channels_over(transport.clone(), fast_policy());

        let status = channels.status("sandbox", &config());

        assert_eq!(status.reach, Reach::Stopped);
        assert_eq!(transport.starts.load(Ordering::SeqCst), 0);
        assert_eq!(transport.execs.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn a_killed_remote_is_reconnected() {
        let transport = Arc::new(ServeTransport::default());
        let channels = channels_over(transport.clone(), fast_policy());
        channels.status("devbox", &config());

        transport.kill_latest();

        wait_until("a second channel", || {
            transport.exec_count() == 2 && channels.open_count() == 1
        });
        assert!(matches!(
            channels.status("devbox", &config()).reach,
            Reach::Reachable { .. }
        ));
        assert_eq!(transport.exec_count(), 2);
    }

    #[test]
    fn reconnect_attempts_back_off() {
        // The first channel drops at once; every reconnect then fails.
        let transport = Arc::new(ScriptTransport::new([
            hello_then("exec sleep 0.1"),
            "echo 'Connection refused' >&2; exit 255".to_string(),
        ]));
        let channels = channels_over(transport.clone(), fast_policy());
        channels.status("devbox", &config());

        wait_until("four attempts", || transport.exec_count() >= 4);

        let calls = transport.calls.lock().unwrap().clone();
        let gaps: Vec<Duration> = calls.windows(2).map(|w| w[1] - w[0]).collect();
        assert!(gaps[1] >= Duration::from_millis(100), "{gaps:?}");
        assert!(gaps[2] >= Duration::from_millis(200), "{gaps:?}");
        assert!(gaps[2] > gaps[1], "{gaps:?}");
        assert_eq!(channels.open_count(), 0);
    }

    #[test]
    fn an_idle_channel_is_closed_and_not_reconnected() {
        let transport = Arc::new(ServeTransport::default());
        let channels = channels_over(
            transport.clone(),
            ChannelPolicy {
                linger: Duration::from_millis(100),
                ..fast_policy()
            },
        );
        channels.status("devbox", &config());
        std::thread::sleep(Duration::from_millis(150));

        channels.close_idle();

        assert_eq!(channels.open_count(), 0);
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(transport.exec_count(), 1);
    }
}
