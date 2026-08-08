//! clouisle-scheduler: 单机资源核算 + 准入控制（FR-03/FR-04）。

pub mod admission;
pub mod placement;

pub use admission::{Reservation, ResourcePool};
pub use placement::{PlacementStrategy, filter_nodes, score_nodes};
