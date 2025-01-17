use crate::ast::{Span, Spanned};

/// Helper trait to coerce errors which do not carry a span into spanned ones.
///
/// This is primarily used to convert errors into
/// [compile::Error][crate::compile::Error].
///
/// This has a blanked implementation over [`Result<T, E>`].
pub trait WithSpan<T, E> {
    /// Convert the given result into a result which produces a spanned error.
    fn with_span<S>(self, spanned: S) -> Result<T, HasSpan<E>>
    where
        S: Spanned;
}

impl<T, E> WithSpan<T, E> for Result<T, E> {
    /// Attach the span extracted from `spanned` to the error if it is present.
    fn with_span<S>(self, spanned: S) -> Result<T, HasSpan<E>>
    where
        S: Spanned,
    {
        match self {
            Ok(value) => Ok(value),
            Err(error) => Err(HasSpan {
                span: spanned.span(),
                error,
            }),
        }
    }
}

/// An error with an associated span.
#[derive(Debug)]
pub struct HasSpan<E> {
    pub(crate) span: Span,
    pub(crate) error: E,
}
