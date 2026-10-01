//! WFSearch core engine: compact in-memory filename index plus the query
//! matcher. Pure logic — all Win32/IO lives in `wfs-fs`.

pub mod engine;
pub mod index;
pub mod matcher;

pub use engine::{
    Engine, EngineStatus, FileEntry, IndexEvent, JournalPos, SearchHit, SearchOptions,
    SearchOutput, VolumePhase, VolumeStatusInfo,
};
pub use index::{
    InsertOutcome, NamePool, Node, RenameOutcome, VolumeIndex, FLAG_DELETED, FLAG_DIR, ROOT_FRN,
};
pub use matcher::{Query, SortKind, Substr, Term};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
