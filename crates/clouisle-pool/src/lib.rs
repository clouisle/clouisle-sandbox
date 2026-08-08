//! clouisle-pool: 快照预热池（FR-08 / ADR-008）。

pub mod pool;

pub use pool::{Pool, PoolError, PoolSlot, SlotState};