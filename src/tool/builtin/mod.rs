//! Built-in tools.
//!
//! These live in-process (vs MCP tools which live in subprocesses).
//!
//! One file per tool keeps the descriptions, args structs, and `invoke`
//! bodies grep-friendly. `SendRateLimit` is shared by `send_message` and
//! `send_file` and lives in its own file so it can be Arc-injected into
//! both without a cyclic dep.

mod cross_user_transcript;
mod fact_forget;
mod fact_remember;
mod identify_user;
mod rate_limit;
mod send_file;
mod send_message;
mod soul_patch;

pub use cross_user_transcript::CrossUserTranscript;
pub use fact_forget::FactForget;
pub use fact_remember::FactRemember;
pub use identify_user::IdentifyUser;
pub use rate_limit::SendRateLimit;
pub use send_file::SendFile;
pub use send_message::SendMessage;
pub use soul_patch::SoulPatch;
