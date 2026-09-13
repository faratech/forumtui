//! Core library for WindowsForum Terminal (`wftui`).
//!
//! Layout: [`config`] (constants + env overrides), [`error`] (house-style
//! plain enum), [`http`]/[`ratelimit`] (client + politeness gates),
//! [`oauth`] (PKCE browser-handoff login), [`token`] (0600 token store),
//! [`site`] (which forum, how to sign in, what it is called),
//! [`models`] (lenient serde shapes), [`api`] (the `WfApi` seam),
//! [`bbcode`] (styled-chunk renderer), [`logging`] (file-only tracing).

pub mod api;
pub mod bbcode;
pub mod config;
pub mod drafts;
pub mod error;
pub mod http;
pub mod logging;
pub mod models;
pub mod oauth;
pub mod osc;
pub mod ratelimit;
pub mod site;
pub mod token;
pub mod update;
