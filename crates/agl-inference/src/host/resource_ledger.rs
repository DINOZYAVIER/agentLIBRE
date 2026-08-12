use std::collections::BTreeMap;

use thiserror::Error;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ResourcePools {
    pub host_bytes: u64,
    pub device_bytes: u64,
    pub shared_bytes: u64,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ResourceRequest {
    pub host_bytes: u64,
    pub device_bytes: u64,
    pub shared_bytes: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResourceReservation {
    id: u64,
    request: ResourceRequest,
}

impl ResourceReservation {
    pub fn id(&self) -> u64 {
        self.id
    }

    pub fn host_bytes(&self) -> u64 {
        self.request.host_bytes
    }

    pub fn device_bytes(&self) -> u64 {
        self.request.device_bytes
    }

    pub fn shared_bytes(&self) -> u64 {
        self.request.shared_bytes
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Error)]
pub enum LiveAdmissionRejection {
    #[error("insufficient host memory: requested {requested}, available {available}")]
    InsufficientHostMemory { requested: u64, available: u64 },
    #[error("insufficient device memory: requested {requested}, available {available}")]
    InsufficientDeviceMemory { requested: u64, available: u64 },
    #[error("insufficient shared memory: requested {requested}, available {available}")]
    InsufficientSharedMemory { requested: u64, available: u64 },
    #[error("resource arithmetic overflow")]
    ArithmeticOverflow,
}

#[derive(Debug)]
pub struct LiveResourceLedger {
    capacity: ResourcePools,
    reserved: ResourcePools,
    next_id: u64,
    reservations: BTreeMap<u64, ResourceRequest>,
}

impl LiveResourceLedger {
    pub fn new(capacity: ResourcePools) -> Self {
        Self {
            capacity,
            reserved: ResourcePools::default(),
            next_id: 1,
            reservations: BTreeMap::new(),
        }
    }

    pub fn reserve(
        &mut self,
        request: ResourceRequest,
    ) -> Result<ResourceReservation, LiveAdmissionRejection> {
        let next = ResourcePools {
            host_bytes: self
                .reserved
                .host_bytes
                .checked_add(request.host_bytes)
                .ok_or(LiveAdmissionRejection::ArithmeticOverflow)?,
            device_bytes: self
                .reserved
                .device_bytes
                .checked_add(request.device_bytes)
                .ok_or(LiveAdmissionRejection::ArithmeticOverflow)?,
            shared_bytes: self
                .reserved
                .shared_bytes
                .checked_add(request.shared_bytes)
                .ok_or(LiveAdmissionRejection::ArithmeticOverflow)?,
        };
        ensure_pool(
            next.host_bytes,
            self.capacity.host_bytes,
            |requested, available| LiveAdmissionRejection::InsufficientHostMemory {
                requested,
                available,
            },
        )?;
        ensure_pool(
            next.device_bytes,
            self.capacity.device_bytes,
            |requested, available| LiveAdmissionRejection::InsufficientDeviceMemory {
                requested,
                available,
            },
        )?;
        ensure_pool(
            next.shared_bytes,
            self.capacity.shared_bytes,
            |requested, available| LiveAdmissionRejection::InsufficientSharedMemory {
                requested,
                available,
            },
        )?;
        let id = self.next_id;
        self.next_id = self
            .next_id
            .checked_add(1)
            .ok_or(LiveAdmissionRejection::ArithmeticOverflow)?;
        self.reserved = next;
        self.reservations.insert(id, request);
        Ok(ResourceReservation { id, request })
    }

    pub fn reserve_pair(
        &mut self,
        first: ResourceRequest,
        second: ResourceRequest,
    ) -> Result<(ResourceReservation, ResourceReservation), LiveAdmissionRejection> {
        let combined = ResourceRequest {
            host_bytes: first
                .host_bytes
                .checked_add(second.host_bytes)
                .ok_or(LiveAdmissionRejection::ArithmeticOverflow)?,
            device_bytes: first
                .device_bytes
                .checked_add(second.device_bytes)
                .ok_or(LiveAdmissionRejection::ArithmeticOverflow)?,
            shared_bytes: first
                .shared_bytes
                .checked_add(second.shared_bytes)
                .ok_or(LiveAdmissionRejection::ArithmeticOverflow)?,
        };
        let next = ResourcePools {
            host_bytes: self
                .reserved
                .host_bytes
                .checked_add(combined.host_bytes)
                .ok_or(LiveAdmissionRejection::ArithmeticOverflow)?,
            device_bytes: self
                .reserved
                .device_bytes
                .checked_add(combined.device_bytes)
                .ok_or(LiveAdmissionRejection::ArithmeticOverflow)?,
            shared_bytes: self
                .reserved
                .shared_bytes
                .checked_add(combined.shared_bytes)
                .ok_or(LiveAdmissionRejection::ArithmeticOverflow)?,
        };
        ensure_pool(
            next.host_bytes,
            self.capacity.host_bytes,
            |requested, available| LiveAdmissionRejection::InsufficientHostMemory {
                requested,
                available,
            },
        )?;
        ensure_pool(
            next.device_bytes,
            self.capacity.device_bytes,
            |requested, available| LiveAdmissionRejection::InsufficientDeviceMemory {
                requested,
                available,
            },
        )?;
        ensure_pool(
            next.shared_bytes,
            self.capacity.shared_bytes,
            |requested, available| LiveAdmissionRejection::InsufficientSharedMemory {
                requested,
                available,
            },
        )?;
        let first_id = self.next_id;
        let second_id = first_id
            .checked_add(1)
            .ok_or(LiveAdmissionRejection::ArithmeticOverflow)?;
        self.next_id = second_id
            .checked_add(1)
            .ok_or(LiveAdmissionRejection::ArithmeticOverflow)?;
        self.reserved = next;
        self.reservations.insert(first_id, first);
        self.reservations.insert(second_id, second);
        Ok((
            ResourceReservation {
                id: first_id,
                request: first,
            },
            ResourceReservation {
                id: second_id,
                request: second,
            },
        ))
    }

    pub fn update_capacity(&mut self, capacity: ResourcePools) {
        self.capacity = capacity;
    }

    pub fn release(&mut self, reservation: &ResourceReservation) -> bool {
        let Some(request) = self.reservations.remove(&reservation.id) else {
            return false;
        };
        self.reserved.host_bytes -= request.host_bytes;
        self.reserved.device_bytes -= request.device_bytes;
        self.reserved.shared_bytes -= request.shared_bytes;
        true
    }

    pub fn reserved(&self) -> ResourcePools {
        self.reserved
    }
}

fn ensure_pool<E>(used: u64, capacity: u64, error: E) -> Result<(), LiveAdmissionRejection>
where
    E: FnOnce(u64, u64) -> LiveAdmissionRejection,
{
    if used > capacity {
        Err(error(used, capacity))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_pools_commit_or_reject_atomically() {
        let mut ledger = LiveResourceLedger::new(ResourcePools {
            host_bytes: 10,
            device_bytes: 20,
            shared_bytes: 30,
        });
        let first = ledger
            .reserve(ResourceRequest {
                host_bytes: 5,
                device_bytes: 6,
                shared_bytes: 7,
            })
            .unwrap();
        let before = ledger.reserved();
        assert!(
            ledger
                .reserve(ResourceRequest {
                    host_bytes: 1,
                    device_bytes: 100,
                    shared_bytes: 1,
                })
                .is_err()
        );
        assert_eq!(ledger.reserved(), before);
        assert!(ledger.release(&first));
        assert_eq!(ledger.reserved(), ResourcePools::default());
        assert!(!ledger.release(&first));
    }

    // MIW-ADM-001, MIW-ADM-002, MIW-ADM-003 and MIW-ADM-008.
    #[test]
    fn cold_model_and_volatile_request_commit_as_one_atomic_envelope() {
        let mut ledger = LiveResourceLedger::new(ResourcePools {
            host_bytes: 12,
            device_bytes: 20,
            shared_bytes: 30,
        });
        let before = ledger.reserved();
        assert!(
            ledger
                .reserve_pair(
                    ResourceRequest {
                        host_bytes: 8,
                        device_bytes: 20,
                        shared_bytes: 30,
                    },
                    ResourceRequest {
                        host_bytes: 5,
                        device_bytes: 0,
                        shared_bytes: 0,
                    },
                )
                .is_err()
        );
        assert_eq!(ledger.reserved(), before);

        let (model, media) = ledger
            .reserve_pair(
                ResourceRequest {
                    host_bytes: 7,
                    device_bytes: 20,
                    shared_bytes: 30,
                },
                ResourceRequest {
                    host_bytes: 5,
                    device_bytes: 0,
                    shared_bytes: 0,
                },
            )
            .unwrap();
        assert_eq!(
            ledger.reserved(),
            ResourcePools {
                host_bytes: 12,
                device_bytes: 20,
                shared_bytes: 30,
            }
        );
        assert!(ledger.release(&media));
        assert_eq!(ledger.reserved().host_bytes, 7);
        assert!(ledger.release(&model));
        assert_eq!(ledger.reserved(), ResourcePools::default());
    }
}
