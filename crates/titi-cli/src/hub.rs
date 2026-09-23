//! Surface-side hub membership.
//!
//! [`titi_core::hub::HubClient`] owns the socket and its reader thread; this
//! is the little bit of bookkeeping both surfaces need on top of it: who is
//! on the roster, and what a polled event means for the screen.
//!
//! Polling is always `try_recv`: the hub is a convenience, and a broker that
//! is slow, gone, or never started must cost the chat loop nothing.

use std::path::Path;

use titi_core::hub::{HubBroker, HubClient, HubEvent};

/// Why a `/join` did not happen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JoinError {
    /// This surface is already on the roster under that id.
    AlreadyJoined(String),
    /// Nothing is listening on the socket. Not an error worth a red screen:
    /// running without a broker is the normal case.
    NoBroker,
    /// The broker answered, and said no (duplicate id, bad id, ...).
    Refused(String),
}

impl std::fmt::Display for JoinError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AlreadyJoined(id) => write!(f, "already joined as {id} · /leave first"),
            Self::NoBroker => f.write_str("no hub broker running"),
            Self::Refused(reason) => write!(f, "hub refused the join: {reason}"),
        }
    }
}

/// What one polled hub event means to a surface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HubUpdate {
    /// The roster changed; read it back from [`HubSession::peers`].
    Roster,
    /// A peer said something. `to` is `None` for a broadcast.
    Message {
        from: String,
        to: Option<String>,
        message: String,
    },
    /// The broker rejected something this surface asked for.
    Refused(String),
    /// The socket closed under us; the session is no longer joined.
    Disconnected,
}

/// This surface's membership in the local hub.
#[derive(Debug, Default)]
pub struct HubSession {
    client: Option<HubClient>,
    /// The broker this surface started, when it was the first to join. Held
    /// for the session's lifetime: dropping it removes the socket and
    /// disconnects the peers, so it outlives every `client` that shares it.
    broker: Option<HubBroker>,
    peers: Vec<String>,
}

impl HubSession {
    /// Joins `<agent_dir>/hub.sock` as `agent_id`.
    pub fn join(&mut self, agent_dir: &Path, agent_id: &str) -> Result<(), JoinError> {
        if let Some(client) = &self.client {
            return Err(JoinError::AlreadyJoined(client.agent_id().to_owned()));
        }
        // First-joiner-hosts: a missing socket is not a refusal, it is this
        // session's cue to start the broker and join it. `NoBroker` therefore
        // no longer reaches a healthy `/join`; it stays only for a store that
        // cannot be reached at all, surfaced through `Refused`.
        match titi_core::hub::join_or_host(agent_dir, agent_id) {
            Ok((client, broker)) => {
                self.peers = client.peers().to_vec();
                self.peers.sort();
                self.client = Some(client);
                self.broker = broker;
                Ok(())
            }
            Err(other) => Err(JoinError::Refused(other.to_string())),
        }
    }

    /// Leaves the hub, reporting whether there was anything to leave.
    ///
    /// The client goes first: its `Drop` sends `Leave` and shuts the stream,
    /// so a hosting session announces its own exit before the broker tears
    /// the socket down. A peer that never hosted holds `broker == None`, so
    /// clearing it is a no-op for them.
    pub fn leave(&mut self) -> Option<String> {
        let client = self.client.take()?;
        let id = client.agent_id().to_owned();
        drop(client);
        self.broker = None;
        self.peers.clear();
        Some(id)
    }

    pub fn joined(&self) -> bool {
        self.client.is_some()
    }

    /// The id this surface joined under, if it did.
    pub fn agent_id(&self) -> Option<&str> {
        self.client.as_ref().map(HubClient::agent_id)
    }

    /// Everyone on the roster, this surface included, sorted by id.
    pub fn peers(&self) -> &[String] {
        &self.peers
    }

    /// Drains what the broker has pushed so far. Never blocks, and never
    /// waits on the socket: an empty result is the normal result.
    pub fn poll(&mut self) -> Vec<HubUpdate> {
        let mut updates = Vec::new();
        loop {
            let Some(client) = &self.client else {
                return updates;
            };
            match client.try_recv() {
                Ok(None) => return updates,
                Ok(Some(event)) => updates.push(self.apply(event)),
                Err(_) => {
                    // The reader thread is gone: the membership is over,
                    // whatever the surface still had on screen.
                    self.client = None;
                    self.peers.clear();
                    updates.push(HubUpdate::Disconnected);
                    return updates;
                }
            }
        }
    }

    /// Folds one event into the roster and says what the surface should do.
    fn apply(&mut self, event: HubEvent) -> HubUpdate {
        match event {
            HubEvent::Presence { agents } => {
                self.peers = agents;
                self.peers.sort();
                HubUpdate::Roster
            }
            HubEvent::Joined { agent_id } => {
                if !self.peers.contains(&agent_id) {
                    self.peers.push(agent_id);
                    self.peers.sort();
                }
                HubUpdate::Roster
            }
            HubEvent::Left { agent_id } => {
                self.peers.retain(|peer| peer != &agent_id);
                HubUpdate::Roster
            }
            HubEvent::Message { from, to, message } => HubUpdate::Message { from, to, message },
            HubEvent::Error { reason } => HubUpdate::Refused(reason),
        }
    }

    /// Folds an event as if the broker had pushed it, for surfaces' tests.
    pub fn ingest(&mut self, event: HubEvent) -> HubUpdate {
        self.apply(event)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// First-joiner-hosts: with no broker up, `/join` starts one and joins
    /// it, so the session lands on a roster with itself rather than bouncing
    /// off a missing socket.
    #[test]
    fn joining_without_a_broker_hosts_one() {
        let dir = tempfile::tempdir().expect("temp");
        let mut hub = HubSession::default();
        hub.join(dir.path(), "titi").expect("the first join hosts");
        assert!(hub.joined());
        assert_eq!(hub.peers(), ["titi"]);
        // Leaving drops the client and then the broker, so the socket goes.
        assert_eq!(hub.leave(), Some("titi".to_owned()));
        assert!(!hub.joined());
        assert!(hub.peers().is_empty());
        assert!(!dir.path().join(titi_core::hub::SOCKET_NAME).exists());
    }

    #[test]
    fn presence_joined_and_left_keep_the_roster_sorted() {
        let mut hub = HubSession::default();
        assert_eq!(
            hub.ingest(HubEvent::Presence {
                agents: vec!["zoe".into(), "ann".into()],
            }),
            HubUpdate::Roster
        );
        assert_eq!(hub.peers(), ["ann".to_owned(), "zoe".to_owned()]);

        hub.ingest(HubEvent::Joined {
            agent_id: "bob".into(),
        });
        assert_eq!(
            hub.peers(),
            ["ann".to_owned(), "bob".to_owned(), "zoe".to_owned()]
        );
        // The broker announces a peer once; a repeat must not double it.
        hub.ingest(HubEvent::Joined {
            agent_id: "bob".into(),
        });
        assert_eq!(hub.peers().len(), 3);

        hub.ingest(HubEvent::Left {
            agent_id: "bob".into(),
        });
        assert_eq!(hub.peers(), ["ann".to_owned(), "zoe".to_owned()]);
    }

    #[test]
    fn a_message_is_handed_to_the_surface_untouched() {
        let mut hub = HubSession::default();
        assert_eq!(
            hub.ingest(HubEvent::Message {
                from: "ann".into(),
                to: None,
                message: "ci is red".into(),
            }),
            HubUpdate::Message {
                from: "ann".to_owned(),
                to: None,
                message: "ci is red".to_owned(),
            }
        );
    }

    /// A real broker: join, see the roster, hear a broadcast, leave.
    #[test]
    fn a_live_broker_drives_the_roster_and_delivers_messages() {
        let dir = tempfile::tempdir().expect("temp");
        let broker = titi_core::hub::HubBroker::bind(dir.path()).expect("broker");
        let peer = HubClient::connect(dir.path(), "peer").expect("peer joins");

        let mut hub = HubSession::default();
        hub.join(dir.path(), "titi").expect("join");
        assert!(hub.peers().contains(&"peer".to_owned()));
        assert!(hub.peers().contains(&"titi".to_owned()));

        peer.broadcast("ci is red").expect("broadcast");
        let heard = wait_for_message(&mut hub);
        assert_eq!(
            heard,
            HubUpdate::Message {
                from: "peer".to_owned(),
                to: None,
                message: "ci is red".to_owned(),
            }
        );

        assert_eq!(hub.leave(), Some("titi".to_owned()));
        assert!(!hub.joined());
        assert!(hub.peers().is_empty());
        drop(peer);
        broker.shutdown();
    }

    /// Polls until the broadcast lands; the socket is asynchronous, so an
    /// empty first poll is expected rather than a failure.
    fn wait_for_message(hub: &mut HubSession) -> HubUpdate {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            for update in hub.poll() {
                if matches!(update, HubUpdate::Message { .. }) {
                    return update;
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!("the broadcast never arrived");
    }
}
