// SPDX-FileCopyrightText: Copyright Logically Magic Inc. <hello@logicallymagic.com>
// SPDX-FileCopyrightText: Copyright Hamid Jahanzad <Hamid@Jahanzad.com>
// SPDX-FileCopyrightText: Copyright Contributors to the Sentin8:Web
// SPDX-License-Identifier: Apache-2.0

//! Sentin8:Web self-registration.
//!
//! The RFW renderer host describes itself to `vertical enroll` via
//! `--print-registration`. Its M2M grants declare:
//! - `sentin8:web-policy` — the Shield hot-reload consumer
//!   (`policy_bus_client("sentin8:web")` → `{service}-policy`).

use sentin8::registration::{AdminScope, M2mGrant, PgRole, Registration};

const M2M_GRANTS: &[M2mGrant] = &[
    // The web host's own Valkey-plane identity (group DESIGN.md §3.1):
    // the app-dir discovery client's cert-CN workload. No scopes — it
    // exists for the enroll-time ACL provisioning (`~captain:app_dir:*`
    // reads) and the token-free registry map.
    M2mGrant {
        workload: "sentin8:web",
        scopes: &[],
    },
    M2mGrant {
        workload: "sentin8:web-policy",
        scopes: &["sentin8:bus:consume"],
    },
    // TLS-RLS-4 (design/sentin8-vertical-data-plane-contract.md §VDC.7): the database-free HostContextResolver’s resolve workload — cache miss calls Sys `GET /v1/resolve/hostname`.
    M2mGrant {
        workload: "sentin8-web-tls",
        scopes: &["sentin8:sys:instances:read"],
    },
];

// Web's bootstrap-tier DB identity (VDC §VDC.3b): the policy boot opens a
// read-only connection BEFORE any M2M token can exist, so web's role
// authenticates with SCRAM (platform.sql seeds/converges the password).
// No tables, no outbox — USAGE/EXECUTE on `public` (the
// `sentin8_boot_policy_load` helper) come from platform.sql's grant lists.
const PG_ROLES: &[PgRole] = &[PgRole {
    scope: "sentin8:web:admin",
    role: "sentin8_web_service",
    bypass_rls: false,
    database: "sentin8",
    cross_schema_grants: &[],
}];

/// The OpenAPI spec closure — the web host carries no utoipa spec (its
/// surface is RFW page rendering), so the closure emits `null`.
fn openapi_spec() -> serde_json::Value {
    serde_json::Value::Null
}

pub fn print(section: Option<String>) -> anyhow::Result<()> {
    let reg = Registration {
        service_type: "sentin8:web",
        subdomain: "web",
        health_path: "/v1/health",
        admin_scopes: &[AdminScope::Tuple("sentin8:web:admin", "sentin8:web:admin")],
        error_codes: &[],
        consent_providers: &[],
        consent_flows: &[],
        admin_ui: None,
        sql_migrations: vec![],
        pg_roles: PG_ROLES,
        policies: &[],
        relations: &[],
        event_triggers: &[],
        schedules: &[],
        audit_findings: None,
        service_actions: None,
        m2m_grants: Some(M2M_GRANTS),
        openapi_url: "/openapi.json",
        openapi: openapi_spec,
    };
    sentin8::registration::print_registration(section, &reg, openapi_spec)
}
