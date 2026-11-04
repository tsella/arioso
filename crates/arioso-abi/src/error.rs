//! The error string, and the guard every entry point goes through.
//!
//! Two rules from the header are implemented here rather than at each call site: a failure
//! always leaves a sentence for the caller to read, and a panic becomes a code instead of
//! unwinding into C.

use std::any::Any;
use std::cell::RefCell;
use std::ffi::{CString, c_char};
use std::panic::{AssertUnwindSafe, catch_unwind};

use crate::ARIOSO_ERR_PANIC;

thread_local! {
    /// The last failure on this thread.
    ///
    /// Thread-local because two threads calling the engine at once must not overwrite each
    /// other's sentence, and because the caller is allowed to keep the pointer only until
    /// its next call.
    static LAST_ERROR: RefCell<Option<CString>> = const { RefCell::new(None) };
}

/// What `arioso_last_error` answers before anything has failed on this thread.
pub(crate) static EMPTY: [u8; 1] = [0];

/// Forget the previous failure.
///
/// Called at the top of every entry point, so a successful call always answers an empty
/// sentence: a caller that reads the error after `ARIOSO_OK` is not reading history.
pub(crate) fn clear() {
    LAST_ERROR.with(|slot| *slot.borrow_mut() = None);
}

/// Record a sentence for the caller.
///
/// An interior NUL would truncate the string at the first one, so it is replaced: the
/// sentence a caller reads should be the sentence the engine wrote.
pub(crate) fn set(message: &str) {
    let text = message.replace('\0', " ");
    let value = CString::new(text).unwrap_or_default();
    LAST_ERROR.with(|slot| *slot.borrow_mut() = Some(value));
}

/// Record `message` and answer `code`, for the `return Err(fail(..))` shape.
pub(crate) fn fail(code: i32, message: impl Into<String>) -> i32 {
    set(&message.into());
    code
}

/// The current sentence, as a pointer the caller must not free.
///
/// Never null: an empty string is the answer when nothing has failed, so `strlen` on the
/// result is always safe.
pub(crate) fn pointer() -> *const c_char {
    LAST_ERROR.with(|slot| match slot.borrow().as_ref() {
        Some(value) => value.as_ptr(),
        None => EMPTY.as_ptr().cast(),
    })
}

/// The message inside a caught panic.
pub(crate) fn panic_message(payload: &(dyn Any + Send)) -> String {
    if let Some(text) = payload.downcast_ref::<&str>() {
        return format!("a panic crossed the boundary: {text}");
    }
    if let Some(text) = payload.downcast_ref::<String>() {
        return format!("a panic crossed the boundary: {text}");
    }
    "a panic crossed the boundary with no message".to_owned()
}

/// Run one entry point, turning a panic into a code and a sentence.
///
/// `body` answers `Ok(())` for success, or the code to return. `ARIOSO_CANCELLED` arrives
/// the same way and leaves the error string empty, because a cancellation the caller asked
/// for is not a failure.
///
/// The closure is asserted unwind safe: it only touches caller pointers and the engine's
/// own locks, and the locks the engine holds are poisoned-and-recovered rather than left in
/// a state a panic could make worse.
pub(crate) fn guard(body: impl FnOnce() -> Result<(), i32>) -> i32 {
    clear();
    match catch_unwind(AssertUnwindSafe(body)) {
        Ok(Ok(())) => crate::ARIOSO_OK,
        Ok(Err(code)) => code,
        Err(payload) => {
            set(&panic_message(payload.as_ref()));
            ARIOSO_ERR_PANIC
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_panic_becomes_a_code_and_a_sentence() {
        let code = guard(|| panic!("the voice exploded"));
        assert_eq!(code, ARIOSO_ERR_PANIC);
        // Safety: `pointer` is never null and points at a NUL-terminated string that lives
        // in this thread until the next call.
        let text = unsafe { std::ffi::CStr::from_ptr(pointer()) }
            .to_str()
            .unwrap()
            .to_owned();
        assert!(text.contains("the voice exploded"), "got {text:?}");
        assert!(text.contains("panic"), "got {text:?}");
    }

    #[test]
    fn a_successful_call_clears_the_sentence() {
        let _ = fail(crate::ARIOSO_ERR_ENGINE, "the voice failed to load");
        assert_ne!(pointer(), EMPTY.as_ptr().cast());
        assert_eq!(guard(|| Ok(())), crate::ARIOSO_OK);
        // Safety: as above, and the pointer is the empty string after a success.
        let text = unsafe { std::ffi::CStr::from_ptr(pointer()) };
        assert_eq!(text.to_bytes(), b"");
    }

    #[test]
    fn a_cancellation_is_not_an_error_and_leaves_no_sentence() {
        assert_eq!(
            guard(|| Err(crate::ARIOSO_CANCELLED)),
            crate::ARIOSO_CANCELLED
        );
        // Safety: as above.
        let text = unsafe { std::ffi::CStr::from_ptr(pointer()) };
        assert_eq!(text.to_bytes(), b"");
    }

    #[test]
    fn a_sentence_with_a_nul_in_it_is_not_truncated_into_garbage() {
        set("the file \0 name is wrong");
        // Safety: as above; the stored CString has no interior NUL.
        let text = unsafe { std::ffi::CStr::from_ptr(pointer()) }
            .to_str()
            .unwrap()
            .to_owned();
        assert_eq!(text, "the file   name is wrong");
    }
}
