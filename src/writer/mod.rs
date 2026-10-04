//! The write side: taking accepted events all the way from the durable journal to a published, queryable column file.
//!
//! It covers appending to the journal, each worker's independent commit discipline, safe retry of in-flight events,
//! building and publishing the file, and the policy that decides when to roll to a new file. This runs only on the
//! leader (behind the `write` feature) and is not reachable from a read-only binary.

pub mod build;
pub mod compaction;
pub mod error;
pub mod pipeline;
pub mod publish;
pub mod queue;
pub mod reserve;
pub mod retry;
pub mod rewrite;
pub mod sim;
