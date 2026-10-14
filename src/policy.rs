// SPDX-FileCopyrightText: Copyright Logically Magic Inc. <hello@logicallymagic.com>
// SPDX-FileCopyrightText: Copyright Hamid Jahanzad <Hamid@Jahanzad.com>
// SPDX-FileCopyrightText: Copyright Contributors to the Sentin8:Web
// SPDX-License-Identifier: Apache-2.0

//! Shield (Sentin8:Policy) integration for web.
//!
//! Thin re-export of the shared bridge (`sentin8::shield::runtime`). Web is a
//! static-file server — static assets and health stay public (the browser
//! app shell); the app-runtime context proxy is a data surface gated by the
//! enforced scope-mirror set (the step-5 gate).

use std::sync::Arc;

#[allow(unused_imports)]
pub use sentin8::shield::runtime::{
    check_policy, evaluate_full, policy_bus_client, request_ctx, require_policy, spawn_hot_reload,
};
pub use sentin8::shield::PolicyEngine as Engine;

/// Test helper: build a bare engine for web (empty until `load` runs in `main`).
pub fn engine() -> Arc<Engine> {
    sentin8::shield::runtime::engine_for("sentin8:web")
}
