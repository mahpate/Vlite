//! Gap 4: time-aware graph adapter.
//!
//! The temporal capability is a required header flag
//! ([`crate::format::FLAG_TEMPORAL`]) set atomically with the first temporal
//! write. Preactivation, like the P3a precision gate
//! (`crate::compact::precision_supported`): release builds do not support it
//! yet, so they refuse temporal files with `UnsupportedFeature(FLAG_TEMPORAL)`
//! and reject temporal graph operations the same way. Only this crate's tests
//! switch support on, per thread, with an [`Activation`], until temporal reads
//! (Gap 4 slice C) qualify every path.

/// Whether this build reads and writes temporal graph files (Gap 4).
pub(crate) fn supported() -> bool {
    #[cfg(test)]
    {
        TEMPORAL.with(std::cell::Cell::get)
    }
    #[cfg(not(test))]
    {
        false
    }
}

#[cfg(test)]
thread_local! {
    static TEMPORAL: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Test-only switch of [`supported`] on this thread, restored when dropped.
#[cfg(test)]
pub(crate) struct Activation(bool);

#[cfg(test)]
impl Activation {
    pub(crate) fn on() -> Self {
        Self::set(true)
    }

    pub(crate) fn set(supported: bool) -> Self {
        Self(TEMPORAL.with(|cell| cell.replace(supported)))
    }
}

#[cfg(test)]
impl Drop for Activation {
    fn drop(&mut self) {
        TEMPORAL.with(|cell| cell.set(self.0));
    }
}

#[cfg(test)]
mod tests;
