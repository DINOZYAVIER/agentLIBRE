//! What a client has because it runs on the human's own computer, and what
//! every surface shows the same way (AGL-126, D1 and D8; AGL-127): the
//! microphone, the dictation and its draft, the files and the clipboard of
//! attachments, the table of the slash commands, the texts of `/help` and
//! `/status`, the fold of a session into records. It links no core.

pub mod attach;
pub mod clipboard;
pub mod commands;
pub mod daemon;
mod error;
pub mod record;
pub mod records;
pub mod words;

pub use error::Error;
