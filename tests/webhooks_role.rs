#![cfg(feature = "webhooks")]

use mcpmem::runtime::WebhookService;
use std::collections::{BTreeMap, BTreeSet};

const fn empty_config() -> mcpmem_webhook::WebhookConfigFile {
    mcpmem_webhook::WebhookConfigFile {
        allowlist: BTreeSet::new(),
        secrets: BTreeMap::new(),
    }
}

/// An empty configuration is the fail-closed default: the section is
/// optional, so this is the configuration most installs start with. The
/// service must still construct a working worker from it.
#[test]
fn empty_config_builds_a_service_with_a_worker() {
    let dir = tempfile::tempdir().unwrap();
    let service = WebhookService::with_config(dir.path().join("memory.mcpmem"), empty_config())
        .expect("an empty webhook config must build a service");
    service
        .run_once(mcpmem_core::events::now_us())
        .expect("the configured worker must poll without error");
}

/// A fresh database is an empty outbox: no subscription, no due delivery.
/// Claiming or completing anything here would lose a delivery, so the report
/// must be all zeros.
#[test]
fn run_against_an_empty_outbox_does_not_error() {
    let dir = tempfile::tempdir().unwrap();
    let service = WebhookService::with_config(dir.path().join("memory.mcpmem"), empty_config())
        .expect("an empty webhook config must build a service");
    let report = service
        .run_once(mcpmem_core::events::now_us())
        .expect("an empty outbox must poll cleanly");
    assert_eq!(report, mcpmem_webhook::DeliveryReport::default());
}
