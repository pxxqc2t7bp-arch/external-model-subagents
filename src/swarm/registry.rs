//! In-process agent discovery and message delivery.

use std::collections::BTreeMap;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::RwLock;

use super::types::{Destination, SwarmAgent, SwarmMessage};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentStatus {
    Running,
    Completed,
    Failed,
}

struct Entry {
    agent: SwarmAgent,
    status: AgentStatus,
    sender: Sender<SwarmMessage>,
}

#[derive(Default)]
pub struct AgentRegistry {
    entries: RwLock<BTreeMap<String, Entry>>,
}

impl AgentRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&self, id: String, agent: SwarmAgent) -> Receiver<SwarmMessage> {
        let (sender, receiver) = mpsc::channel();
        self.entries.write().expect("agent registry poisoned").insert(
            id,
            Entry {
                agent,
                status: AgentStatus::Running,
                sender,
            },
        );
        receiver
    }

    pub fn set_status(&self, id: &str, status: AgentStatus) {
        if let Some(entry) = self.entries.write().expect("agent registry poisoned").get_mut(id) {
            entry.status = status;
        }
    }

    pub fn unregister(&self, id: &str) {
        self.entries.write().expect("agent registry poisoned").remove(id);
    }

    /// Return matching running agents in ID order.
    pub fn find_by_capability(&self, capability: &str) -> Vec<String> {
        self.entries
            .read()
            .expect("agent registry poisoned")
            .iter()
            .filter(|(_, entry)| {
                entry.status == AgentStatus::Running
                    && std::iter::once(&entry.agent.role)
                        .chain(&entry.agent.capabilities)
                        .any(|name| {
                            name.chars()
                                .flat_map(char::to_lowercase)
                                .eq(capability.chars().flat_map(char::to_lowercase))
                        })
            })
            .map(|(id, _)| id.clone())
            .collect()
    }

    /// Count successful deliveries to running agents, including the sender on broadcast.
    pub fn send(&self, msg: SwarmMessage) -> usize {
        let entries = self.entries.read().expect("agent registry poisoned");
        match &msg.to {
            Destination::Agent(id) => usize::from(
                entries
                    .get(id)
                    .filter(|entry| entry.status == AgentStatus::Running)
                    .is_some_and(|entry| entry.sender.send(msg).is_ok()),
            ),
            Destination::Broadcast => {
                let mut running = entries
                    .values()
                    .filter(|entry| entry.status == AgentStatus::Running);
                let Some(last) = running.next_back() else {
                    return 0;
                };
                running.filter(|entry| entry.sender.send(msg.clone()).is_ok()).count()
                    + usize::from(last.sender.send(msg).is_ok())
            }
        }
    }
}
