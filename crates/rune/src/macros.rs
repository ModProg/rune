//! The macro system of Rune.
//!
//! Macros are registered with [Module::macro_][crate::Module::macro_] and are
//! function-like items that are expanded at compile time.
//!
//! Macros take a token stream as an argument and is responsible for translating
//! it into another token stream that will be embedded into the source location
//! where the macro was invoked.
//!
//! ```
//! use rune::{T, Context, Module, Vm};
//! use rune::ast;
//! use rune::compile;
//! use rune::macros::{quote, MacroContext, TokenStream};
//! use rune::parse::Parser;
//! use std::sync::Arc;
//!
//! #[rune::macro_]
//! fn concat_idents(ctx: &mut MacroContext<'_>, stream: &TokenStream) -> compile::Result<TokenStream> {
//!     let mut output = String::new();
//!
//!     let mut p = Parser::from_token_stream(stream, ctx.stream_span());
//!
//!     let ident = p.parse::<ast::Ident>()?;
//!     output.push_str(ctx.resolve(ident)?);
//!
//!     while p.parse::<Option<T![,]>>()?.is_some() {
//!         if p.is_eof()? {
//!             break;
//!         }
//!
//!         let ident = p.parse::<ast::Ident>()?;
//!         output.push_str(ctx.resolve(ident)?);
//!     }
//!
//!     p.eof()?;
//!
//!     let output = ctx.ident(&output);
//!     Ok(quote!(#output).into_token_stream(ctx))
//! }
//!
//! # fn main() -> rune::Result<()> {
//! let mut m = Module::new();
//! m.macro_meta(concat_idents)?;
//!
//! let mut context = Context::new();
//! context.install(m)?;
//!
//! let runtime = Arc::new(context.runtime());
//!
//! let mut sources = rune::sources! {
//!     entry => {
//!         pub fn main() {
//!             let foobar = 42;
//!             concat_idents!(foo, bar)
//!         }
//!     }
//! };
//!
//! let unit = rune::prepare(&mut sources)
//!     .with_context(&context)
//!     .build()?;
//!
//! let unit = Arc::new(unit);
//!
//! let mut vm = Vm::new(runtime, unit);
//! let value = vm.call(["main"], ())?;
//! let value: u32 = rune::from_value(value)?;
//!
//! assert_eq!(value, 42);
//! # Ok(())
//! # }
//! ```

mod format_args;
mod into_lit;
mod macro_compiler;
mod macro_context;
mod quote_fn;
mod storage;
mod token_stream;

pub use self::format_args::FormatArgs;
pub use self::into_lit::IntoLit;
pub(crate) use self::macro_compiler::MacroCompiler;
pub use self::macro_context::MacroContext;
pub use self::quote_fn::{quote_fn, Quote};
pub(crate) use self::storage::Storage;
pub use self::storage::{SyntheticId, SyntheticKind};
pub use self::token_stream::{ToTokens, TokenStream, TokenStreamIter};

/// Macro helper function for quoting the token stream as macro output.
///
/// Is capable of quoting everything in Rune, except for the following:
/// * Labels, which must be created using `Label::new`.
/// * Dynamic quoted strings and other literals, which must be created using
///   `Lit::new`.
///
/// ```
/// use rune::macros::quote;
///
/// quote!(hello self);
/// ```
///
/// # Interpolating values
///
/// Values are interpolated with `#value`, or `#(value + 1)` for expressions.
///
/// # Iterators
///
/// Anything that can be used as an iterator can be iterated over with
/// `#(iter)*`. A token can also be used to join inbetween each iteration, like
/// `#(iter),*`.
pub use rune_macros::quote;

/// Helper derive to implement [`ToTokens`].
pub use rune_macros::ToTokens;
