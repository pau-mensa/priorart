use std::process::ExitCode;
use std::sync::Arc;

fn main() -> ExitCode {
    priorart::cli::run(|_| Ok(Arc::new(priorart::gather::Bm25Recipe)))
}
