//! Unified resource inventory for server, client, and NIC monitor lifecycles.
//!
//! The production adapter delegates to the existing managers. The in-memory
//! implementation is a deterministic lifecycle model used by fault and
//! concurrency tests.

use crate::cmd::iperf::{IperfClientJobMgr, IperfServerMgr};
use crate::nic::monitor::MonitorMgr;
use crate::protocol::ResourceCleanupOut;
#[cfg(test)]
use std::collections::{HashMap, HashSet};
#[cfg(test)]
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

#[cfg(test)]
fn lock_recover<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ResourceKind {
    Server,
    Client,
    Monitor,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ResourceRef {
    pub kind: ResourceKind,
    pub id: String,
    pub owner_id: String,
}

/// Snapshot and owner-scoped cleanup boundary shared by production and tests.
pub trait ResourceInventory: Send + Sync {
    fn list_owner(&self, owner_id: &str) -> Vec<ResourceRef>;

    fn cleanup_owner(&self, owner_id: &str, wait: Duration) -> ResourceCleanupOut;

    /// 停止全部资源（Agent 退出清理用）。
    fn cleanup_all(&self, wait: Duration) -> ResourceCleanupOut;
}

/// Production adapter over the three independent agent managers.
pub struct AgentResourceInventory<'a> {
    clients: &'a IperfClientJobMgr,
    servers: &'a IperfServerMgr,
    monitors: &'a MonitorMgr,
}

impl<'a> AgentResourceInventory<'a> {
    pub fn new(
        clients: &'a IperfClientJobMgr,
        servers: &'a IperfServerMgr,
        monitors: &'a MonitorMgr,
    ) -> Self {
        Self {
            clients,
            servers,
            monitors,
        }
    }
}

impl ResourceInventory for AgentResourceInventory<'_> {
    fn list_owner(&self, owner_id: &str) -> Vec<ResourceRef> {
        let mut resources = Vec::new();
        resources.extend(
            self.clients
                .resource_ids_for_owner(owner_id)
                .into_iter()
                .map(|id| ResourceRef {
                    kind: ResourceKind::Client,
                    id,
                    owner_id: owner_id.to_string(),
                }),
        );
        resources.extend(
            self.servers
                .resource_ids_for_owner(owner_id)
                .into_iter()
                .map(|id| ResourceRef {
                    kind: ResourceKind::Server,
                    id,
                    owner_id: owner_id.to_string(),
                }),
        );
        resources.extend(
            self.monitors
                .resource_ids_for_owner(owner_id)
                .into_iter()
                .map(|id| ResourceRef {
                    kind: ResourceKind::Monitor,
                    id,
                    owner_id: owner_id.to_string(),
                }),
        );
        resources.sort();
        resources
    }

    fn cleanup_owner(&self, owner_id: &str, wait: Duration) -> ResourceCleanupOut {
        // Taking a snapshot through the common boundary makes cleanup
        // observability identical for production and the deterministic model.
        let _before_cleanup = self.list_owner(owner_id);
        // Preserve the established cleanup order: stop senders first, then
        // listeners, and settle monitors last so their final sample is kept.
        let clients = self.clients.stop_owner(owner_id, wait);
        let servers = self.servers.stop_owner(owner_id, Duration::ZERO);
        let monitors = self.monitors.stop_owner(owner_id);
        let mut errors = clients.errors;
        errors.extend(servers.errors);
        let mut stopped_monitors = 0usize;
        for (id, result) in monitors {
            match result {
                Ok(_) => stopped_monitors += 1,
                Err(error) => errors.push(format!("monitor {id} cleanup failed: {error}")),
            }
        }
        ResourceCleanupOut {
            servers: servers.stopped,
            clients: clients.stopped,
            monitors: stopped_monitors,
            errors,
        }
    }

    fn cleanup_all(&self, wait: Duration) -> ResourceCleanupOut {
        let clients = self.clients.stop_all(wait);
        let servers = self.servers.stop_all();
        let monitors = self.monitors.stop_all();
        let mut errors = clients.errors;
        errors.extend(servers.errors);
        let mut stopped_monitors = 0usize;
        for (id, result) in monitors {
            match result {
                Ok(_) => stopped_monitors += 1,
                Err(error) => errors.push(format!("monitor {id} cleanup failed: {error}")),
            }
        }
        ResourceCleanupOut {
            servers: servers.stopped,
            clients: clients.stopped,
            monitors: stopped_monitors,
            errors,
        }
    }
}

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegisterOutcome {
    Created,
    Replayed,
}

#[cfg(test)]
#[derive(Debug, Clone)]
struct LedgerEntry {
    resource: ResourceRef,
    fingerprint: String,
}

#[cfg(test)]
#[derive(Debug, Default)]
struct LedgerState {
    live: HashMap<String, LedgerEntry>,
    tombstones: HashSet<String>,
    closed_owners: HashSet<String>,
}

/// Deterministic fake inventory used by state-machine and transport tests.
#[cfg(test)]
#[derive(Debug, Default)]
pub struct InMemoryResourceInventory {
    state: Mutex<LedgerState>,
}

#[cfg(test)]
impl InMemoryResourceInventory {
    pub fn register(
        &self,
        resource: ResourceRef,
        fingerprint: impl Into<String>,
    ) -> Result<RegisterOutcome, String> {
        if resource.id.is_empty() || resource.owner_id.is_empty() {
            return Err("resource id and owner_id must not be empty".into());
        }
        let fingerprint = fingerprint.into();
        let mut state = lock_recover(&self.state);
        if state.closed_owners.contains(&resource.owner_id) {
            return Err(format!("owner {} is already closed", resource.owner_id));
        }
        if state.tombstones.contains(&resource.id) {
            return Err(format!("resource {} was already stopped", resource.id));
        }
        if let Some(existing) = state.live.get(&resource.id) {
            if existing.resource == resource && existing.fingerprint == fingerprint {
                return Ok(RegisterOutcome::Replayed);
            }
            return Err(format!(
                "resource {} conflicts with an existing start",
                resource.id
            ));
        }
        state.live.insert(
            resource.id.clone(),
            LedgerEntry {
                resource,
                fingerprint,
            },
        );
        Ok(RegisterOutcome::Created)
    }

    /// Idempotent stop. Unknown IDs also create a tombstone, which prevents a
    /// delayed start response/request from resurrecting that lifecycle ID.
    pub fn stop(&self, id: &str) -> bool {
        let mut state = lock_recover(&self.state);
        let existed = state.live.remove(id).is_some();
        state.tombstones.insert(id.to_string());
        existed
    }

    pub fn is_owner_closed(&self, owner_id: &str) -> bool {
        lock_recover(&self.state).closed_owners.contains(owner_id)
    }
}

#[cfg(test)]
impl ResourceInventory for InMemoryResourceInventory {
    fn list_owner(&self, owner_id: &str) -> Vec<ResourceRef> {
        let state = lock_recover(&self.state);
        let mut resources: Vec<ResourceRef> = state
            .live
            .values()
            .filter(|entry| entry.resource.owner_id == owner_id)
            .map(|entry| entry.resource.clone())
            .collect();
        resources.sort();
        resources
    }

    fn cleanup_owner(&self, owner_id: &str, _wait: Duration) -> ResourceCleanupOut {
        let mut state = lock_recover(&self.state);
        state.closed_owners.insert(owner_id.to_string());
        let targets: Vec<String> = state
            .live
            .iter()
            .filter(|(_, entry)| entry.resource.owner_id == owner_id)
            .map(|(id, _)| id.clone())
            .collect();
        let mut out = ResourceCleanupOut::default();
        for id in targets {
            if let Some(entry) = state.live.remove(&id) {
                match entry.resource.kind {
                    ResourceKind::Server => out.servers += 1,
                    ResourceKind::Client => out.clients += 1,
                    ResourceKind::Monitor => out.monitors += 1,
                }
                state.tombstones.insert(id);
            }
        }
        out
    }

    fn cleanup_all(&self, _wait: Duration) -> ResourceCleanupOut {
        let mut state = lock_recover(&self.state);
        let ids: Vec<String> = state.live.keys().cloned().collect();
        let mut out = ResourceCleanupOut::default();
        for id in ids {
            if let Some(entry) = state.live.remove(&id) {
                match entry.resource.kind {
                    ResourceKind::Server => out.servers += 1,
                    ResourceKind::Client => out.clients += 1,
                    ResourceKind::Monitor => out.monitors += 1,
                }
                state.tombstones.insert(id);
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier};

    fn resource(kind: ResourceKind, id: &str, owner: &str) -> ResourceRef {
        ResourceRef {
            kind,
            id: id.into(),
            owner_id: owner.into(),
        }
    }

    #[derive(Debug, Clone, Copy)]
    enum ModelOp {
        StartA,
        ReplayA,
        ConflictA,
        StartB,
        StopA,
        CleanupA,
    }

    const OPS: [ModelOp; 6] = [
        ModelOp::StartA,
        ModelOp::ReplayA,
        ModelOp::ConflictA,
        ModelOp::StartB,
        ModelOp::StopA,
        ModelOp::CleanupA,
    ];

    fn check_sequence(sequence: &[ModelOp]) {
        let inventory = InMemoryResourceInventory::default();
        let mut cleanup_a_seen = false;
        for (step, op) in sequence.iter().enumerate() {
            match op {
                ModelOp::StartA | ModelOp::ReplayA => {
                    let result = inventory.register(
                        resource(ResourceKind::Client, "job-a", "owner-a"),
                        "fingerprint-a",
                    );
                    if cleanup_a_seen {
                        assert!(result.is_err(), "closed owner must reject late start");
                    }
                }
                ModelOp::ConflictA => {
                    let before = inventory.list_owner("owner-a");
                    let result = inventory.register(
                        resource(ResourceKind::Client, "job-a", "owner-a"),
                        format!("different-fingerprint-{step}"),
                    );
                    if !before.is_empty() || cleanup_a_seen {
                        assert!(
                            result.is_err(),
                            "conflicting replay must never replace live state"
                        );
                        assert_eq!(
                            inventory.list_owner("owner-a"),
                            before,
                            "rejected conflict must preserve the live resource"
                        );
                    }
                }
                ModelOp::StartB => {
                    let _ = inventory.register(
                        resource(ResourceKind::Monitor, "monitor-b", "owner-b"),
                        "fingerprint-b",
                    );
                }
                ModelOp::StopA => {
                    inventory.stop("job-a");
                }
                ModelOp::CleanupA => {
                    inventory.cleanup_owner("owner-a", Duration::ZERO);
                    cleanup_a_seen = true;
                }
            }

            let owner_a = inventory.list_owner("owner-a");
            let unique_a: HashSet<&str> = owner_a.iter().map(|item| item.id.as_str()).collect();
            assert_eq!(
                unique_a.len(),
                owner_a.len(),
                "resource IDs must stay unique"
            );
            if cleanup_a_seen {
                assert!(owner_a.is_empty(), "cleanup must leave no owner-a resource");
            }
            assert!(inventory.list_owner("owner-b").len() <= 1);
        }
    }

    fn enumerate(prefix: &mut Vec<ModelOp>, remaining: usize, visited: &mut usize) {
        if remaining == 0 {
            check_sequence(prefix);
            *visited += 1;
            return;
        }
        for op in OPS {
            prefix.push(op);
            enumerate(prefix, remaining - 1, visited);
            prefix.pop();
        }
    }

    #[test]
    fn bounded_lifecycle_model_exhausts_start_stop_cleanup_sequences() {
        let mut visited = 0usize;
        enumerate(&mut Vec::new(), 5, &mut visited);
        assert_eq!(visited, OPS.len().pow(5));
    }

    #[test]
    fn concurrent_identical_start_linearizes_to_create_then_replay() {
        let inventory = Arc::new(InMemoryResourceInventory::default());
        let barrier = Arc::new(Barrier::new(3));
        let mut handles = Vec::new();
        for _ in 0..2 {
            let inventory = Arc::clone(&inventory);
            let barrier = Arc::clone(&barrier);
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                inventory
                    .register(
                        resource(ResourceKind::Server, "request-1", "owner-a"),
                        "same-fingerprint",
                    )
                    .unwrap()
            }));
        }
        barrier.wait();
        let mut outcomes: Vec<RegisterOutcome> = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect();
        outcomes.sort_by_key(|outcome| match outcome {
            RegisterOutcome::Created => 0,
            RegisterOutcome::Replayed => 1,
        });
        assert_eq!(
            outcomes,
            vec![RegisterOutcome::Created, RegisterOutcome::Replayed]
        );
        assert_eq!(inventory.list_owner("owner-a").len(), 1);
    }

    #[test]
    fn cleanup_is_idempotent_and_does_not_touch_other_owner() {
        let inventory = InMemoryResourceInventory::default();
        inventory
            .register(resource(ResourceKind::Client, "client-a", "owner-a"), "a")
            .unwrap();
        inventory
            .register(resource(ResourceKind::Monitor, "monitor-b", "owner-b"), "b")
            .unwrap();
        let first = inventory.cleanup_owner("owner-a", Duration::ZERO);
        assert_eq!((first.clients, first.servers, first.monitors), (1, 0, 0));
        let second = inventory.cleanup_owner("owner-a", Duration::ZERO);
        assert_eq!((second.clients, second.servers, second.monitors), (0, 0, 0));
        assert_eq!(inventory.list_owner("owner-b").len(), 1);
        assert!(inventory.is_owner_closed("owner-a"));
    }
}
