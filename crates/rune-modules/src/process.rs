//! The native `process` module for the [Rune Language].
//!
//! [Rune Language]: https://rune-rs.github.io
//!
//! ## Usage
//!
//! Add the following to your `Cargo.toml`:
//!
//! ```toml
//! rune-modules = { version = "0.12.3", features = ["process"] }
//! ```
//!
//! Install it into your context:
//!
//! ```rust
//! let mut context = rune::Context::with_default_modules()?;
//! context.install(rune_modules::process::module(true)?)?;
//! # Ok::<_, rune::Error>(())
//! ```
//!
//! Use it in Rune:
//!
//! ```rust,ignore
//! use process::Command;
//!
//! fn main() {
//!     let command = Command::new("ls");
//!     command.run().await;
//! }
//! ```

use rune::{Any, Module, ContextError};
use rune::runtime::{Bytes, Shared, Value, Protocol, VmResult};
use std::fmt;
use std::io;
use tokio::process;

/// Construct the `process` module.
pub fn module(_stdio: bool) -> Result<Module, ContextError> {
    let mut module = Module::with_crate("process");
    module.ty::<Command>()?;
    module.ty::<Child>()?;
    module.ty::<ExitStatus>()?;
    module.ty::<Output>()?;

    module.function(["Command", "new"], Command::new)?;
    module.associated_function("spawn", Command::spawn)?;
    module.associated_function("arg", Command::arg)?;
    module.associated_function("args", Command::args)?;
    module.associated_function("wait_with_output", Child::wait_with_output)?;
    module.associated_function(Protocol::STRING_DISPLAY, ExitStatus::display)?;
    module.associated_function("code", ExitStatus::code)?;
    Ok(module)
}

#[derive(Any)]
#[rune(item = ::process)]
struct Command {
    inner: process::Command,
}

impl Command {
    /// Construct a new command.
    fn new(command: &str) -> Self {
        Self {
            inner: process::Command::new(command),
        }
    }

    /// Add arguments.
    fn args(&mut self, args: &[Value]) -> VmResult<()> {
        for arg in args {
            match arg {
                Value::String(s) => {
                    self.inner.arg(&*rune::vm_try!(s.borrow_ref()));
                }
                Value::StaticString(s) => {
                    self.inner.arg(&***s);
                }
                actual => {
                    return VmResult::expected::<String>(rune::vm_try!(actual.type_info()));
                }
            }
        }

        VmResult::Ok(())
    }

    /// Add an argument.
    fn arg(&mut self, arg: &str) {
        self.inner.arg(arg);
    }

    /// Spawn the command.
    fn spawn(mut self) -> io::Result<Child> {
        Ok(Child {
            inner: Some(self.inner.spawn()?),
        })
    }
}

#[derive(Any)]
#[rune(item = ::process)]
struct Child {
    // we use an option to avoid a panic if we try to complete the child process
    // multiple times.
    //
    // TODO: enapculate this pattern in some better way.
    inner: Option<process::Child>,
}

impl Child {
    // Returns a future that will resolve to an Output, containing the exit
    // status, stdout, and stderr of the child process.
    async fn wait_with_output(self) -> VmResult<io::Result<Output>> {
        let inner = match self.inner {
            Some(inner) => inner,
            None => {
                return VmResult::panic("already completed");
            }
        };

        let output = match inner.wait_with_output().await {
            Ok(output) => output,
            Err(error) => return VmResult::Ok(Err(error)),
        };

        VmResult::Ok(Ok(Output {
            status: ExitStatus { status: output.status },
            stdout: Shared::new(Bytes::from_vec(output.stdout)),
            stderr: Shared::new(Bytes::from_vec(output.stderr)),
        }))
    }
}

#[derive(Any)]
#[rune(item = ::process)]
struct Output {
    #[rune(get)]
    status: ExitStatus,
    #[rune(get)]
    stdout: Shared<Bytes>,
    #[rune(get)]
    stderr: Shared<Bytes>,
}

#[derive(Clone, Copy, Any)]
#[rune(item = ::process)]
struct ExitStatus {
    status: std::process::ExitStatus,
}

impl ExitStatus {
    fn display(&self, buf: &mut String) -> fmt::Result {
        use std::fmt::Write as _;
        write!(buf, "{}", self.status)
    }

    fn code(&self) -> Option<i32> {
        self.status.code()
    }
}
