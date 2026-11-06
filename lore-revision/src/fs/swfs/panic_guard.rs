use std::any::Any;
use std::panic::AssertUnwindSafe;
use std::panic::catch_unwind;

use crate::fs::swfs::api_interface::SwfsInterface;
use crate::fs::swfs::api_interface::SwfsInterfaceError;

/// Runs `body`, the body of the SWFS callback `callback`, under [`panic_guard`], reporting a
/// panic as an error in the log and the service's status. A callback returns what the body
/// returns, or `T::default()`, its failure value, when the body panics.
pub fn panic_guard_swfs_callback<T: Default>(callback: &str, body: impl FnOnce() -> T) -> T {
    panic_guard(callback, body, report_callback_panic)
}

/// Reports a panic that [`panic_guard`] caught in the SWFS callback `callback`.
fn report_callback_panic(callback: &str, panic: &str) {
    SwfsInterface::handle_lore_error(SwfsInterfaceError::internal(format!(
        "SWFS callback {callback} panicked: {panic}"
    )));
}

/// Runs `body`, the body of the SWFS callback `callback`, and returns what it returns. A panic in
/// it fails the callback instead: `report` is given the callback's name and the panic's message,
/// and the callback returns `T::default()`, which is each callback's failure value. A panic must
/// not unwind into SWFS's C code, and must not end the service the mount belongs to.
///
/// Only builds that unwind reach `report`: a release build aborts on a panic (`panic = "abort"`),
/// before anything can catch it, which is why the callback paths return errors rather than panic.
#[lore_macro::test_pub]
fn panic_guard<T: Default>(
    callback: &str,
    body: impl FnOnce() -> T,
    report: impl FnOnce(&str, &str),
) -> T {
    match catch_unwind(AssertUnwindSafe(body)) {
        Ok(value) => value,
        Err(payload) => {
            report(callback, panic_message(payload.as_ref()));
            T::default()
        }
    }
}

/// The message a panic was raised with, which `panic!` gives as a `&str` or, formatted, a `String`.
fn panic_message(payload: &(dyn Any + Send)) -> &str {
    if let Some(message) = payload.downcast_ref::<&str>() {
        message
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message
    } else {
        "a panic with no message"
    }
}
