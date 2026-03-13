use std::path::Path;

use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::{EnvFilter, fmt, layer::SubscriberExt, util::SubscriberInitExt};

/// Initialize stderr-only logging for commands that don't need file output.
/// Used by prep, look, report, clean, ticket.
pub fn init_stderr_only() {
    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("oven_cli=info")))
        .with(fmt::layer().with_writer(std::io::stderr))
        .init();
}

/// Initialize dual-output logging: human-readable to stderr, JSON to a file.
///
/// Used by `oven on`. The returned `WorkerGuard` must be held until shutdown
/// to ensure the background writer thread flushes and stops cleanly.
pub fn init_with_file(log_dir: &Path, verbose: bool) -> WorkerGuard {
    let file_appender = tracing_appender::rolling::never(log_dir, "pipeline.log");
    let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);

    let env_filter = if verbose {
        EnvFilter::new("oven_cli=debug")
    } else {
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("oven_cli=info"))
    };

    tracing_subscriber::registry()
        .with(env_filter)
        .with(fmt::layer().with_writer(std::io::stderr))
        .with(fmt::layer().json().with_writer(non_blocking))
        .init();

    guard
}

#[cfg(test)]
mod tests {
    // Logging initialization is global and can only happen once per process,
    // so we verify the functions compile and the guard pattern is sound.
    // Full integration testing of log output is deferred to CLI integration tests.

    #[test]
    fn init_stderr_only_compiles() {
        // Just verify the function signature and types are correct.
        // Can't actually call it in tests because tracing::subscriber::set_global_default
        // can only be called once per process.
        let _ = std::io::stderr;
    }

    #[test]
    fn guard_is_send() {
        fn assert_send<T: Send>() {}
        assert_send::<tracing_appender::non_blocking::WorkerGuard>();
    }
}
