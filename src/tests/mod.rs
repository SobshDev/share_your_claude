//! In-crate integration tests. They live inside the crate because the upstream and token
//! endpoint overrides on `AppState` are `pub(crate)`.
//!
//! `mock_upstream` is a fake Anthropic API on a loopback port, and `harness` builds the app
//! against it and provides request, login, and database fixture helpers. Both are re-exported
//! here, so every topic module only needs `use super::*;`. Topic modules whose names match a
//! crate module (`auth`, `oauth`, `policy`, `usage`, ...) shadow it inside `tests`, so they
//! import the crate module explicitly, for example `use crate::oauth;`.
use super::*;
use axum::{
    Json,
    body::Body,
    extract::State,
    http::{HeaderMap, Request, StatusCode},
    response::{IntoResponse, Response},
};
use futures_util::StreamExt;
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sqlx::Row;
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};
use tower::ServiceExt;

mod harness;
mod mock_upstream;

mod admin;
mod analytics;
mod auth;
mod dashboard;
mod oauth;
mod policy;
mod proxy;
mod usage;

use harness::*;
use mock_upstream::*;

/// The one model the harness enables and grants to its key.
const MODEL: &str = "claude-sonnet-4-6";
