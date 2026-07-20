use thiserror::Error;

/// Failures from Win32 window/monitor operations, kept separate from `domain`
/// so the domain layer never has to know about Win32 (PLAN.md §9.3).
#[derive(Debug, Error)]
pub enum WindowError {
    #[error("Win32 call failed ({context}): {source}")]
    Win32 {
        context: &'static str,
        #[source]
        source: windows::core::Error,
    },

    #[error("Win32 call failed ({context})")]
    Win32NoDetail { context: &'static str },
}

impl WindowError {
    pub fn win32(context: &'static str, source: windows::core::Error) -> Self {
        Self::Win32 { context, source }
    }

    pub fn no_detail(context: &'static str) -> Self {
        Self::Win32NoDetail { context }
    }
}
