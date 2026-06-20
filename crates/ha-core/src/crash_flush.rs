//! Crash-time hooks for flushing in-flight `StreamPersister` placeholders
//! when the process is exiting cleanly. Signal handlers run on actual
//! shutdown (SIGINT/SIGTERM/Ctrl+C/Ctrl+Break) and call
//! `flush_all_blocking` to mark every active placeholder `orphaned` before
//! `std::process::exit`.
//!
//! Panic recovery is intentionally NOT global. Tokio tasks, Tauri commands,
//! and `catch_unwind` boundaries routinely turn local panics into recovered
//! errors while the process keeps running; flushing every active persister
//! on a panic anywhere in the process would corrupt unrelated active
//! sessions. Per-task panic safety lives in `StreamPersister::Drop`: the
//! unwinding task drops its `Arc`, `Drop` finalizes that one placeholder
//! to `orphaned`, and other concurrent sessions are untouched.
//!
//! `install_signal_handlers` requires an ambient tokio runtime; call it
//! from the Tauri `setup` async block, the HTTP server `main`, or the ACP
//! entrypoint after their runtimes are up.

use std::sync::OnceLock;

use crate::chat_engine::active_persisters;

static PANIC_HOOK_INSTALLED: OnceLock<()> = OnceLock::new();
static SIGNAL_HANDLERS_INSTALLED: OnceLock<()> = OnceLock::new();

/// Idempotent no-op kept for API stability. A process-wide panic hook
/// that SIGKILLs registered exec subprocesses was considered but
/// rejected: tokio task panics are commonly recovered via `JoinHandle`
/// boundaries without the process exiting, and a global kill on any
/// thread's panic would tear down unrelated long-running user commands.
/// Per-task cleanup runs through `tools::exec::ProcessGroupGuard::Drop`
/// (kills the offending exec's own process group) and
/// `StreamPersister::Drop` (finalizes that one placeholder row).
pub fn install_panic_hook() {
    let _ = PANIC_HOOK_INSTALLED.set(());
}

/// Finalize every active GUI/HTTP turn with `TerminationReason::Shutdown`
/// before the process exits. Called from the signal handler after the
/// shutdown sentinel is written. Synchronous so we don't depend on a
/// runtime that may already be tearing down.
fn finalize_active_turns_for_shutdown() {
    let Some(db) = crate::get_session_db() else {
        return;
    };
    let active = crate::chat_engine::active_turn::all_current();
    if active.is_empty() {
        return;
    }
    for snapshot in active {
        // Mirror app_init's startup-sweep behavior: resolve the
        // session's actual provider shape so a Shutdown finalize on an
        // OpenAI Chat / Responses / Codex session doesn't get rebuilt
        // as Anthropic `tool_use` / `tool_result` (which the original
        // provider would 4xx or silently drop on resume).
        let provider_kind =
            crate::chat_engine::finalize::rebuild::resolve_provider_kind_for_session(
                &db,
                &snapshot.session_id,
            );
        let partial = crate::chat_engine::finalize::rebuild::collect_partial_from_messages(
            &db,
            &snapshot.session_id,
            provider_kind,
        );
        let partial = crate::chat_engine::finalize::PartialMeta {
            turn_id: Some(snapshot.turn_id.clone()),
            ..partial
        };
        let _ = crate::chat_engine::finalize::finalize_turn_context_blocking(
            &db,
            &snapshot.session_id,
            crate::chat_engine::finalize::TerminationReason::Shutdown,
            partial,
            snapshot.source,
        );
    }
}

/// Install signal handlers (SIGINT/SIGTERM on Unix, ctrl_c/ctrl_break on
/// Windows) that flush active persisters and exit cleanly. Idempotent.
/// MUST be called from within a tokio runtime — uses `tokio::spawn`.
///
/// **Desktop (Tauri) mode**: SIGINT (Ctrl+C) is *not* intercepted so the
/// signal propagates to the parent process chain (`pnpm tauri dev` → vite).
/// Intercepting it would cause `std::process::exit(0)` to kill only the
/// hope-agent child while leaving vite/pnpm orphaned — the user then has
/// to `kill -9` them manually. Desktop exits through Tauri's own
/// `RunEvent::Exit` path instead. SIGTERM is still handled so `kill
/// $PID` works for clean shutdown.
///
/// **Server / ACP mode**: both SIGINT and SIGTERM are handled because
/// these modes run as foreground daemons where Ctrl+C is the expected
/// way to stop the process.
pub fn install_signal_handlers() {
    install_signal_handlers_inner(false);
}

/// Like [`install_signal_handlers`] but forces SIGINT handling even in
/// desktop mode. Used by server and ACP entrypoints that *are* the
/// foreground process and need Ctrl+C to trigger clean shutdown.
pub fn install_signal_handlers_with_sigint() {
    install_signal_handlers_inner(true);
}

fn install_signal_handlers_inner(force_sigint: bool) {
    if SIGNAL_HANDLERS_INSTALLED.set(()).is_err() {
        return;
    }

    // Desktop mode: skip SIGINT handler so Ctrl+C propagates to the
    // parent process chain (pnpm → tauri-cli → vite). SIGTERM is still
    // handled for `kill $PID` clean shutdown.
    let handle_sigint = force_sigint || !crate::app_init::is_desktop();

    #[cfg(unix)]
    {
        tokio::spawn(async move {
            use tokio::signal::unix::{signal, SignalKind};

            // SIGTERM is always handled — `kill $PID` should flush.
            let mut sigterm = match signal(SignalKind::terminate()) {
                Ok(s) => s,
                Err(e) => {
                    app_warn!(
                        "session",
                        "stream_persist",
                        "install SIGTERM handler failed: {}",
                        e
                    );
                    return;
                }
            };

            if handle_sigint {
                let mut sigint = match signal(SignalKind::interrupt()) {
                    Ok(s) => s,
                    Err(e) => {
                        app_warn!(
                            "session",
                            "stream_persist",
                            "install SIGINT handler failed: {}",
                            e
                        );
                        return;
                    }
                };
                tokio::select! {
                    _ = sigint.recv() => {
                        app_info!("session", "stream_persist", "received SIGINT, clean shutdown");
                    }
                    _ = sigterm.recv() => {
                        app_info!("session", "stream_persist", "received SIGTERM, clean shutdown");
                    }
                }
            } else {
                // Desktop: only SIGTERM. SIGINT falls through to the
                // default disposition so the parent process tree exits.
                sigterm.recv().await;
                app_info!(
                    "session",
                    "stream_persist",
                    "received SIGTERM, clean shutdown"
                );
            }

            fire_shutdown_session_end().await;
            run_clean_shutdown();
        });
    }

    #[cfg(windows)]
    {
        tokio::spawn(async move {
            // Windows: Ctrl+C / Ctrl+Break are always handled because
            // the signal model differs (no SIGINT propagation issue).
            let mut ctrl_c = match tokio::signal::windows::ctrl_c() {
                Ok(s) => s,
                Err(e) => {
                    app_warn!(
                        "session",
                        "stream_persist",
                        "install ctrl_c handler failed: {}",
                        e
                    );
                    return;
                }
            };
            let mut ctrl_break = match tokio::signal::windows::ctrl_break() {
                Ok(s) => s,
                Err(e) => {
                    app_warn!(
                        "session",
                        "stream_persist",
                        "install ctrl_break handler failed: {}",
                        e
                    );
                    return;
                }
            };
            tokio::select! {
                _ = ctrl_c.recv() => {
                    app_info!("session", "stream_persist", "received Ctrl+C, clean shutdown");
                }
                _ = ctrl_break.recv() => {
                    app_info!("session", "stream_persist", "received Ctrl+Break, clean shutdown");
                }
            }
            fire_shutdown_session_end().await;
            run_clean_shutdown();
        });
    }
}

/// Fire the `SessionEnd` shutdown hook (app-global, source `other`) on the
/// real shutdown path, bounded so a slow command hook can't wedge process
/// termination. No-op (returns immediately) when no `SessionEnd` hook is
/// configured, so the common case adds no shutdown latency. This is the single
/// place SessionEnd-on-shutdown fires for signal-driven exits (server / ACP /
/// terminal Ctrl-C); GUI window-quit fires it separately from `RunEvent::Exit`.
async fn fire_shutdown_session_end() {
    let _ = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        crate::hooks::dispatch_session_end("", "other"),
    )
    .await;
}

/// Shared cleanup sequence for SIGINT/SIGTERM/Ctrl+C/Ctrl+Break:
/// sentinel → flush placeholders → finalize active turns → exit.
///
/// **Order matters**: `flush_all_blocking` must run *before* finalize
/// so any in-memory `StreamPersister` buffer that hasn't reached its
/// 500ms / 1KB throttle is persisted as a placeholder row first. The
/// finalize pass then reverse-rebuilds from `messages` and writes the
/// `Shutdown` marker / event row / chat_turn closure including those
/// last-moment bytes. Reversing this order writes finalize from the
/// pre-flush DB state and then dangles the flushed rows as orphans
/// the next launch's restore would miss.
fn run_clean_shutdown() -> ! {
    clean_shutdown_no_exit();
    std::process::exit(0);
}

/// Same as `run_clean_shutdown` but without `std::process::exit(0)`.
/// Used by the Tauri desktop `RunEvent::Exit` handler — the process
/// is already exiting, so calling `exit` again would skip Tauri's own
/// teardown and potentially corrupt state.
pub fn clean_shutdown_no_exit() {
    crate::chat_engine::finalize::sentinel::write_clean_marker();
    active_persisters::flush_all_blocking();
    finalize_active_turns_for_shutdown();
}
