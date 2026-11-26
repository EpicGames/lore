// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Re-export of the shared test helpers for these tests to use.

#[allow(unused_imports)]
pub use lore_base::test_util::TempDir;

/// A listener on a service socket that stands in for a service, or for a socket a move of the
/// external root holds, and counts the calls and stops that reached it. Stops listening when
/// dropped.
#[allow(dead_code)]
pub struct Replier {
    name: String,
    stopping: std::sync::Arc<std::sync::atomic::AtomicBool>,
    answered: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    thread: Option<std::thread::JoinHandle<()>>,
}

/// How a [`Replier`] answers a call.
enum Calls {
    /// The hello with this build's, then call `i` with `replies[i]`, or the last of them past its
    /// end: the payload of one message, or the bytes of whole messages where `framed` is set.
    Answered { replies: Vec<Vec<u8>>, framed: bool },
    /// The hello with a refusal, as a service whose schemas differ from the client's answers.
    Refused,
    /// With nothing: the connection is closed on the hello, as a service of an older protocol
    /// closes it.
    Unanswered,
}

/// How a [`Replier`] answers a stop.
enum Stops {
    /// With status 0, after which it stops listening, as a service does.
    Honoured,
    /// With `ServiceUnavailable` and the move's reason, as a socket a move holds does, as no
    /// service runs to stop.
    Refused,
    /// With nothing: the connection is closed on the stop.
    Unanswered,
}

#[allow(dead_code)]
impl Replier {
    /// Listens on `name` and answers every request on it with the move reply, and refuses every
    /// stop, as a socket a move of the external root holds does.
    pub fn move_reply(name: &str) -> Replier {
        Replier::with(
            name,
            lore::remote::message::encode_message(&lore::remote::message::MessageToClient::<
                lore_revision::event::LoreEvent,
            >::ExternalRootMove),
        )
    }

    /// Listens on `name` and answers the hello of every request on it with this build's, then the
    /// request with a message of the bytes `payload`. Refuses every stop as a socket a move holds
    /// does.
    pub fn with(name: &str, payload: Vec<u8>) -> Replier {
        let replies = vec![payload];
        Replier::answering(
            name,
            Calls::Answered {
                replies,
                framed: false,
            },
            Stops::Refused,
        )
    }

    /// Listens on `name` as a service of this build: answers its calls in turn with each of
    /// `replies`, the bytes of whole messages as `lore::remote::message::write_payload` writes
    /// them, and honours a stop, after which it stops listening.
    pub fn service(name: &str, replies: Vec<Vec<u8>>) -> Replier {
        Replier::answering(
            name,
            Calls::Answered {
                replies,
                framed: true,
            },
            Stops::Honoured,
        )
    }

    /// Listens on `name` as a service of another build: refuses every call's hello, and honours a
    /// stop, after which it stops listening.
    pub fn other_build(name: &str) -> Replier {
        Replier::answering(name, Calls::Refused, Stops::Honoured)
    }

    /// Listens on `name` as a socket a move by another build holds: refuses every call's hello and
    /// every stop.
    pub fn held_by_another_build(name: &str) -> Replier {
        Replier::answering(name, Calls::Refused, Stops::Refused)
    }

    /// Listens on `name` as a service of an older protocol: closes the connection on every hello
    /// and every stop.
    pub fn older_protocol(name: &str) -> Replier {
        Replier::answering(name, Calls::Unanswered, Stops::Unanswered)
    }

    /// Answers each call as `calls` describes, and each stop as `stops` does.
    fn answering(name: &str, calls: Calls, stops: Stops) -> Replier {
        use std::sync::atomic::Ordering;

        use lore::remote::message::Opening;

        let listener = lore::remote::network::UdsListener::new(name).expect("listen on the socket");
        let stopping = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let answered = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let thread = std::thread::spawn({
            let stopping = stopping.clone();
            let answered = answered.clone();
            move || {
                while let Ok(mut stream) = listener.accept() {
                    if stopping.load(Ordering::SeqCst) {
                        break;
                    }
                    match lore::remote::message::blocking_read_opening(stream.reader()) {
                        Ok(Some(Opening::Hello(_))) => {}
                        Ok(Some(Opening::Stop { .. })) => {
                            answered.fetch_add(1, Ordering::SeqCst);
                            match stops {
                                Stops::Honoured => {
                                    let _ = lore::remote::message::write_stop_result(
                                        stream.writer(),
                                        0,
                                        "",
                                    );
                                    break;
                                }
                                Stops::Refused => {
                                    let _ = lore::remote::message::write_stop_result(
                                        stream.writer(),
                                        lore_error_set::FfiError::ffi_code(
                                            &lore_base::error::ServiceUnavailable {
                                                reason: String::new(),
                                            },
                                        ),
                                        &lore::remote::occupy::stop_refusal(),
                                    );
                                }
                                Stops::Unanswered => {}
                            }
                            continue;
                        }
                        _ => continue,
                    }
                    let (replies, framed) = match &calls {
                        Calls::Answered { replies, framed } => (replies, *framed),
                        Calls::Refused => {
                            answered.fetch_add(1, Ordering::SeqCst);
                            let _ = lore::remote::message::write_refusal(
                                stream.writer(),
                                "the Lore service is of another build",
                            );
                            continue;
                        }
                        Calls::Unanswered => {
                            answered.fetch_add(1, Ordering::SeqCst);
                            continue;
                        }
                    };
                    // The hello is answered with this build's before the request is read, as the
                    // service answers it.
                    if lore::remote::message::write_hello(stream.writer()).is_err() {
                        continue;
                    }
                    let request = lore::remote::message::blocking_read_message::<
                        lore::remote::message::MessageToServer,
                        _,
                    >(stream.reader());
                    if let Ok(Some(_)) = request {
                        let turn = answered.fetch_add(1, Ordering::SeqCst);
                        let reply = &replies[turn.min(replies.len() - 1)];
                        if framed {
                            let _ = std::io::Write::write_all(stream.writer(), reply);
                        } else {
                            let _ = lore::remote::message::write_payload(stream.writer(), reply);
                        }
                    }
                }
            }
        });
        Replier {
            name: name.to_string(),
            stopping,
            answered,
            thread: Some(thread),
        }
    }

    /// The calls and stops that have reached it so far.
    pub fn answered(&self) -> usize {
        self.answered.load(std::sync::atomic::Ordering::SeqCst)
    }
}

impl Drop for Replier {
    fn drop(&mut self) {
        self.stopping
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let _ = lore::remote::network::UdsStream::connect(&self.name);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Registers the shared stores in `stores`, directories that hold a `shared_store` directory, in
/// the registry of the global directory `global`.
#[allow(dead_code)]
pub fn register(global: &std::path::Path, stores: &[&std::path::Path]) {
    let mut registry = lore_revision::shared_store::registry::SharedStoreRegistry::default();
    for store in stores {
        registry
            .register("lore://127.0.0.1:1".to_owned(), store)
            .expect("register the store");
    }
    let directory = global
        .join("data")
        .join(lore_revision::shared_store::SHARED_STORES_DIR);
    std::fs::create_dir_all(&directory).expect("make the registry's directory");
    std::fs::write(
        directory.join(lore_revision::shared_store::registry::REGISTRY),
        toml::to_string(&registry).expect("serialize the registry"),
    )
    .expect("write the registry");
}

/// A hold on the lock of the shared store in `store`, a directory that holds or is to hold a
/// `shared_store` directory, as another process holds it, with the store's `immutable` directory
/// made.
#[allow(dead_code)]
pub async fn hold_store(store: &std::path::Path) -> lore_base::fs::lock::FSLock {
    let immutable = store
        .join(lore_revision::shared_store::SHARED_STORE_DIR)
        .join("immutable");
    std::fs::create_dir_all(&immutable).expect("make the store's immutable directory");
    lore_base::fs::lock::FSLock::acquire_exact_path(&immutable.join("lock"))
        .await
        .expect("hold the store's lock")
}

/// The path a listing of the shared stores gives for the store in `store`, as the registry
/// records it.
#[allow(dead_code)]
pub fn listed_path(store: &std::path::Path) -> String {
    lore_revision::util::path::clean(store.display().to_string())
}
