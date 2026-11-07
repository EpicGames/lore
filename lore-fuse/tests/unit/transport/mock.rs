// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::sync::Arc;
use std::sync::atomic::Ordering;

use lore_fuse::transport::Transport;
use lore_fuse::transport::mock::MockTransport;

#[test]
fn a_default_transport_fails_no_worker() {
    let transport = Arc::new(MockTransport::default());
    assert!(transport.worker().is_ok());
    assert!(transport.worker().is_ok());
}

#[test]
fn a_failing_worker_fails_only_itself() {
    let transport = MockTransport::new();
    transport.fail_worker.store(1 + 1, Ordering::SeqCst);
    assert!(transport.worker().is_ok());
    assert!(transport.worker().is_err());
    assert!(transport.worker().is_ok());
}
