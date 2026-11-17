// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Re-export of the shared test helpers for these tests to use.

#[allow(unused_imports)]
pub use lore_base::test_util::TempDir;

/// A listener on a service socket that answers each request with one message, as a socket a move
/// of the external root holds answers with the move reply, and counts the requests it answered.
/// Stops listening when dropped.
#[allow(dead_code)]
pub struct Replier {
    name: String,
    stopping: std::sync::Arc<std::sync::atomic::AtomicBool>,
    answered: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    thread: Option<std::thread::JoinHandle<()>>,
}

#[allow(dead_code)]
impl Replier {
    /// Listens on `name` and answers every request on it with the move reply.
    pub fn move_reply(name: &str) -> Replier {
        Replier::with(
            name,
            lore::remote::message::encode_message(&lore::remote::message::MessageToClient::<
                lore_revision::event::LoreEvent,
            >::ExternalRootMove),
        )
    }

    /// Listens on `name` and answers every request on it with a message of the bytes `payload`.
    pub fn with(name: &str, payload: Vec<u8>) -> Replier {
        use std::sync::atomic::Ordering;

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
                    let request = lore::remote::message::blocking_read_message::<
                        lore::remote::message::MessageToServer,
                        _,
                    >(stream.reader());
                    if let Ok(Some(_)) = request {
                        answered.fetch_add(1, Ordering::SeqCst);
                        let _ = lore::remote::message::write_payload(stream.writer(), &payload);
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

    /// The requests answered so far.
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
