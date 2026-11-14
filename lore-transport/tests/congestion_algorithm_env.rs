// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Connections opened by a process that names its congestion controller in
//! `LORE_QUIC_CONGESTION_ALGORITHM`, as the CLI and the storage API open them: with no controller
//! from settings. A process reads the variable once, so these run in a process of their own that
//! sets it before anything reads it: to `cubic`, or, in the child process that
//! [`an_unknown_algorithm_fails_a_connection_without_settings`] starts, to the value the parent
//! passes.

use std::process::Command;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::Weak;

use lore_base::error::Disconnected;
use lore_base::types::Partition;
use lore_base::types::RepositoryId;
use lore_transport::EnvironmentConfig;
use lore_transport::auth::AuthEndpoint;
use lore_transport::auth::AuthMode;
use lore_transport::connection::CONGESTION_ALGORITHM_ENV;
use lore_transport::connection::Connection;
use lore_transport::connection::SuppliedCredentials;
use lore_transport::connection::add;
use lore_transport::connection::connect;
use lore_transport::error::ProtocolError;
use lore_transport::quic::client::CongestionAlgorithm;
use lore_transport::traits::Admin;
use lore_transport::traits::Environment;
use lore_transport::traits::Lock;
use lore_transport::traits::Protocol;
use lore_transport::traits::Repository;
use lore_transport::traits::Revision;
use lore_transport::traits::Storage;

/// The value a child process names, as its parent passes it.
const CHILD_VALUE_VAR: &str = "LORE_TEST_CHILD_CONGESTION_ALGORITHM";

/// The test a child process runs.
const CHILD_TEST: &str = "child_connects_naming_the_value_its_parent_passed";

/// A constructor because the environment is process-wide: this runs before the harness starts the
/// threads that would make writing to it a data race.
#[ctor::ctor]
fn name_the_algorithm() {
    let value = std::env::var_os(CHILD_VALUE_VAR).unwrap_or_else(|| "cubic".into());
    // Safety: constructors run before `main`, so this is the single-threaded window where writing
    // to the environment has no reader to race.
    unsafe { std::env::set_var(CONGESTION_ALGORITHM_ENV, value) };
}

/// A protocol whose storage records the controller each storage connection is opened with, and
/// connects none.
#[derive(Default)]
struct RecordingProtocol(parking_lot::Mutex<Vec<CongestionAlgorithm>>);

struct NoEndpoints;

#[async_trait::async_trait]
impl Environment for NoEndpoints {
    async fn get(&self) -> Result<EnvironmentConfig, ProtocolError> {
        Ok(EnvironmentConfig::default())
    }
}

#[async_trait::async_trait]
impl Protocol for RecordingProtocol {
    async fn storage(
        &self,
        _: Weak<Connection>,
        _: &str,
        _: &AuthEndpoint,
        _: &str,
        _: Partition,
        _: usize,
        _: &Arc<SuppliedCredentials>,
        congestion_algorithm: CongestionAlgorithm,
    ) -> Result<Arc<dyn Storage>, ProtocolError> {
        self.0.lock().push(congestion_algorithm);
        Err(ProtocolError::from(Disconnected))
    }

    async fn revision(
        &self,
        _: Weak<Connection>,
        _: &str,
        _: &AuthEndpoint,
        _: &str,
        _: RepositoryId,
        _: &Arc<SuppliedCredentials>,
    ) -> Result<Arc<dyn Revision>, ProtocolError> {
        Err(ProtocolError::from(Disconnected))
    }

    async fn repository(
        &self,
        _: Weak<Connection>,
        _: &str,
        _: &AuthEndpoint,
        _: &str,
        _: &Arc<SuppliedCredentials>,
    ) -> Result<Arc<dyn Repository>, ProtocolError> {
        Err(ProtocolError::from(Disconnected))
    }

    async fn admin(
        &self,
        _: Weak<Connection>,
        _: &str,
        _: &AuthEndpoint,
        _: &str,
        _: RepositoryId,
        _: &Arc<SuppliedCredentials>,
    ) -> Result<Arc<dyn Admin>, ProtocolError> {
        Err(ProtocolError::from(Disconnected))
    }

    async fn lock(
        &self,
        _: Weak<Connection>,
        _: &str,
        _: &AuthEndpoint,
        _: &str,
        _: RepositoryId,
        _: &Arc<SuppliedCredentials>,
    ) -> Result<Arc<dyn Lock>, ProtocolError> {
        Err(ProtocolError::from(Disconnected))
    }

    async fn environment(
        &self,
        _: Weak<Connection>,
        _: &str,
    ) -> Result<Arc<dyn Environment>, ProtocolError> {
        Ok(Arc::new(NoEndpoints))
    }
}

/// A connection to `remote`, with `congestion_algorithm` from settings or none.
async fn connected(
    remote: &str,
    congestion_algorithm: Option<CongestionAlgorithm>,
) -> Result<Arc<Connection>, ProtocolError> {
    connect(
        remote,
        "",
        RepositoryId::default(),
        1,
        "",
        "",
        AuthMode::Auto,
        congestion_algorithm,
    )
    .await
}

/// A connection established with no controller from settings runs the one the process names, and
/// one established with a controller from settings runs that one. They reach different remotes, as
/// a call is served the connection already cached for its remote as it is.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_connection_without_settings_runs_the_algorithm_the_process_names() {
    let protocol = Arc::new(RecordingProtocol::default());
    add("ccenv", protocol.clone()).expect("the scheme is not registered yet");
    let named = connected("ccenv://named.invalid:41337", None)
        .await
        .expect("a connection");
    let _ = named.storage().await;
    let configured = connected(
        "ccenv://configured.invalid:41337",
        Some(CongestionAlgorithm::Bbr),
    )
    .await
    .expect("a connection");
    let _ = configured.storage().await;

    assert_eq!(named.congestion_algorithm(), CongestionAlgorithm::Cubic);
    assert_eq!(configured.congestion_algorithm(), CongestionAlgorithm::Bbr);
    assert_eq!(
        *protocol.0.lock(),
        [CongestionAlgorithm::Cubic, CongestionAlgorithm::Bbr]
    );
}

/// A process that names an unknown congestion controller opens no connection without a controller
/// from settings, and says which variable is wrong. A connection with one from settings is opened.
#[test]
fn an_unknown_algorithm_fails_a_connection_without_settings() {
    let output = Command::new(std::env::current_exe().expect("this test binary"))
        .args(["--exact", CHILD_TEST, "--ignored", "--nocapture"])
        .env(CHILD_VALUE_VAR, "reno")
        .stdin(Stdio::null())
        .output()
        .expect("the child process runs");

    assert!(
        output.status.success(),
        "the child process failed with {}:\n{}{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("1 passed"),
        "the child process ran no test:\n{}",
        String::from_utf8_lossy(&output.stdout)
    );
}

/// Run by [`an_unknown_algorithm_fails_a_connection_without_settings`] in a process of its own,
/// which names the value its parent passes. Without that value, it does nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "the child process an_unknown_algorithm_fails_a_connection_without_settings starts, not a test of its own"]
async fn child_connects_naming_the_value_its_parent_passed() {
    if std::env::var_os(CHILD_VALUE_VAR).is_none() {
        return;
    }
    add("ccchild", Arc::new(RecordingProtocol::default()))
        .expect("the scheme is not registered yet");
    let remote = "ccchild://remote.invalid:41337";

    let error = match connected(remote, None).await {
        Ok(_) => panic!("a connection without settings opened under an unknown algorithm"),
        Err(error) => error,
    };
    assert!(
        error.to_string().contains(CONGESTION_ALGORITHM_ENV),
        "unexpected error: {error}"
    );
    connected(remote, Some(CongestionAlgorithm::Cubic))
        .await
        .expect("a connection with a controller from settings");
}
