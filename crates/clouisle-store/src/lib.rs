//! clouisle-store: 状态存储抽象层（ADR-007）。
//!
//! Phase 1-2 用 SQLite（`rusqlite` bundled，WAL 模式，零外部依赖）。
//! Phase 3 切 Postgres（`sqlx`）。

pub mod store_trait;
pub mod sqlite;
pub mod memory;
pub mod postgres;

pub use store_trait::{Store, StoreError};
pub use sqlite::SqliteStore;
pub use memory::InMemoryStore;
pub use postgres::PostgresStore;