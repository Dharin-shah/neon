//!
//! This module provides some helpers for spawning tokio tasks in the pageserver.
//!
//! Mostly just a wrapper around tokio::spawn, with some code to handle panics.
//!

use std::future::Future;
use std::panic::AssertUnwindSafe;

use futures::FutureExt;
use tokio::runtime::Runtime;

use tracing::{debug, error, info};

use once_cell::sync::Lazy;

use crate::context::{self, TaskKind};

//
// There are four runtimes:
//
// Compute request runtime
//  - used to handle connections from compute nodes. Any tasks related to satisfying
//    GetPage requests, base backups, import, and other such compute node operations
//    are handled by the Compute request runtime
//  - page_service.rs
//  - this includes layer downloads from remote storage, if a layer is needed to
//    satisfy a GetPage request
//
// Management request runtime
//  - used to handle HTTP API requests
//
// WAL receiver runtime:
//  - used to handle WAL receiver connections.
//  - and to receiver updates from storage_broker
//
// Background runtime
//  - layer flushing
//  - garbage collection
//  - compaction
//  - remote storage uploads
//  - initial tenant loading
//
// Everything runs in a tokio task. If you spawn new tasks, spawn it using the correct
// runtime.
//
// There might be situations when one task needs to wait for a task running in another
// Runtime to finish. For example, if a background operation needs a layer from remote
// storage, it will start to download it. If a background operation needs a remote layer,
// and the download was already initiated by a GetPage request, the background task
// will wait for the download - running in the Page server runtime - to finish.
// Another example: the initial tenant loading tasks are launched in the background ops
// runtime. If a GetPage request comes in before the load of a tenant has finished, the
// GetPage request will wait for the tenant load to finish.
//
// It's nice to have different runtimes, so that you can quickly eyeball how much CPU
// time each class of operations is taking, with 'top -H' or similar.
//
// It's also good to avoid hogging all threads that would be needed to process
// other operations, if the upload tasks e.g. get blocked on locks. It shouldn't
// happen, but still.
//
pub static COMPUTE_REQUEST_RUNTIME: Lazy<Runtime> = Lazy::new(|| {
    tokio::runtime::Builder::new_multi_thread()
        .thread_name("compute request worker")
        .enable_all()
        .build()
        .expect("Failed to create compute request runtime")
});

pub static MGMT_REQUEST_RUNTIME: Lazy<Runtime> = Lazy::new(|| {
    tokio::runtime::Builder::new_multi_thread()
        .thread_name("mgmt request worker")
        .enable_all()
        .build()
        .expect("Failed to create mgmt request runtime")
});

pub static WALRECEIVER_RUNTIME: Lazy<Runtime> = Lazy::new(|| {
    tokio::runtime::Builder::new_multi_thread()
        .thread_name("walreceiver worker")
        .enable_all()
        .build()
        .expect("Failed to create walreceiver runtime")
});

pub static BACKGROUND_RUNTIME: Lazy<Runtime> = Lazy::new(|| {
    tokio::runtime::Builder::new_multi_thread()
        .thread_name("background op worker")
        .enable_all()
        .build()
        .expect("Failed to create background op runtime")
});

/// Launch a new task
/// Note: if shutdown_process_on_error is set to true failure
///   of the task will lead to shutdown of entire process
pub fn spawn<F>(
    runtime: &tokio::runtime::Handle,
    name: &str,
    shutdown_process_on_error: bool,
    future: F,
) where
    F: Future<Output = anyhow::Result<()>> + Send + 'static,
{
    let task_name = name.to_string();
    let _join_handle = runtime.spawn(task_wrapper(task_name, shutdown_process_on_error, future));
}

/// This wrapper function runs in a newly-spawned task. It initializes the
/// task-local variables and calls the payload function.
async fn task_wrapper<F>(task_name: String, shutdown_process_on_error: bool, future: F)
where
    F: Future<Output = anyhow::Result<()>> + Send + 'static,
{
    debug!("Starting task '{}'", task_name);

    // We use AssertUnwindSafe here so that the payload function
    // doesn't need to be UnwindSafe. We don't do anything after the
    // unwinding that would expose us to unwind-unsafe behavior.
    let result = AssertUnwindSafe(future).catch_unwind().await;
    task_finish(result, task_name, shutdown_process_on_error).await;
}

async fn task_finish(
    result: std::result::Result<
        anyhow::Result<()>,
        std::boxed::Box<dyn std::any::Any + std::marker::Send>,
    >,
    task_name: String,
    shutdown_process_on_error: bool,
) {
    let mut shutdown_process = false;
    {
        match result {
            Ok(Ok(())) => {
                debug!("Task '{}' exited normally", task_name);
            }
            Ok(Err(err)) => {
                if shutdown_process_on_error {
                    error!(
                        "Shutting down: task '{}' exited with error: {:?}",
                        task_name, err
                    );
                    shutdown_process = true;
                } else {
                    error!("Task '{}'  exited with error: {:?}", task_name, err);
                }
            }
            Err(err) => {
                if shutdown_process_on_error {
                    error!("Shutting down: task '{}' panicked: {:?}", task_name, err);
                    shutdown_process = true;
                } else {
                    error!("Task '{}'  panicked: {:?}", task_name, err);
                }
            }
        }
    }

    if shutdown_process {
        shutdown_pageserver(1).await;
    }
}

///
/// Perform pageserver shutdown. This is called on receiving a signal,
/// or if one of the tasks marked as 'shutdown_process_on_error' dies.
///
/// This never returns.
pub async fn shutdown_pageserver(exit_code: i32) {
    // Shut down the libpq endpoint task. This prevents new connections from
    // being accepted.
    context::shutdown_tasks(TaskKind::LibpqEndpointListener).await;

    // Shut down all tenants gracefully
    crate::tenant::mgr::shutdown_all_tenants().await;

    // Shut down the HTTP endpoint last, so that you can still check the server's
    // status while it's shutting down.
    // FIXME: We should probably stop accepting commands like attach/detach earlier.
    context::shutdown_tasks(TaskKind::HttpEndpointListener).await;

    // There should be nothing left, but let's be sure
    context::shutdown_all_tasks().await;

    info!("Shut down successfully completed");
    std::process::exit(exit_code);
}
