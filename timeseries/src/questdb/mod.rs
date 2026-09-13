//! Everything that speaks to QuestDB, split by what it is for: `client` is the
//! transport, `schema` creates, `writer` ingests, `series` reads, `rollup` is
//! the tier table the last two have to agree on, and `annotations` keeps the
//! notes that say what the readings mean.

pub mod annotations;
pub mod client;
pub mod rollup;
pub mod schema;
pub mod series;
pub mod writer;

pub use client::Client;
pub use writer::Record;
