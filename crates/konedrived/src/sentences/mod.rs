//! Every sentence the daemon says to a person about what is wrong, in one place: each case
//! of trouble is a type of the layer that keeps it, and its sentence is here, as its
//! `Display`. Nothing else in the daemon writes such a sentence; a lower layer that logs a
//! case, or joins cases into `LastError`, says it through `Display`.
//!
//! A layer of its own, after `daemon` and before `dbus`: it sees every type of trouble,
//! and the bus takes its sentences from it.
//!
//! - `config_sentences` — `config.toml` and the accounts in it;
//! - `status_sentences` — a folder's trouble, and what holds its uploads and downloads.

mod config_sentences;
mod status_sentences;
