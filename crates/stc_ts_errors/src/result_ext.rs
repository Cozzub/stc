use std::panic::Location;

use crate::{Error, ErrorKind};

pub trait DebugExt<T>: Into<Result<T, Error>> {
    fn convert_err<F>(self, op: F) -> Result<T, Error>
    where
        F: FnOnce(ErrorKind) -> ErrorKind,
    {
        self.into().map_err(|err: Error| err.convert(op))
    }

    #[inline]
    #[track_caller]
    fn context(self, msg: &str) -> Result<T, Error> {
        if !cfg!(debug_assertions) {
            return self.into();
        }
        let loc = Location::caller();

        self.into().map_err(|err: Error| err.context_impl(loc, msg))
    }

    #[inline]
    #[track_caller]
    fn with_context<F>(self, msg: F) -> Result<T, Error>
    where
        F: FnOnce() -> String,
    {
        if !cfg!(debug_assertions) {
            return self.into();
        }
        let loc = Location::caller();

        self.into().map_err(|err: Error| err.context_impl(loc, msg()))
    }
}

impl<T> DebugExt<T> for Result<T, Error> {}
