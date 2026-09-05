//! Core library for the wftui WindowsForum terminal client.
//!
//! Layout: [`config`] (constants + env overrides), [`error`] (house-style
//! plain enum), [`http`]/[`ratelimit`] (client + politeness gates),
//! [`oauth`] (PKCE browser-handoff login), [`token`] (0600 token store),
//! [`models`] (lenient serde shapes), [`api`] (the `WfApi` seam),
//! [`bbcode`] (styled-chunk renderer), [`logging`] (file-only tracing).

pub mod api;
pub mod bbcode;
pub mod config;
pub mod error;
pub mod http;
pub mod logging;
pub mod models;
pub mod oauth;
pub mod osc;
pub mod ratelimit;
pub mod token;
