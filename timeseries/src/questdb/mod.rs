//! Everything that speaks to QuestDB, split by what it is for: `client` is the
//! transport, `schema` creates, `writer` ingests, `series` reads, `periods`
//! reads by the calendar, `rollup` is the tier table the writer and the readers
//! have to agree on, and `annotations` keeps the notes that say what the
//! readings mean.

pub mod annotations;
pub mod client;
pub mod periods;
pub mod rollup;
pub mod schema;
pub mod series;
pub mod writer;

pub use client::Client;
pub use writer::Record;
