//! buddy: a guarded assistant for weekly reports (`wr`) and end-of-day `signoff`.

pub mod app;
pub mod cli;
pub mod collect;
pub mod config;
pub mod domain;
pub mod doctor;
pub mod job;
pub mod mail;
pub mod render;
pub mod report;
pub mod secrets;
pub mod signoff;
pub mod sources;
pub mod sync;
pub mod template;
