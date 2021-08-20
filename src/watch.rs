use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use futures::prelude::*;
use notify::{recommended_watcher, Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
// use tokio::process::{Child, Command};
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;
use tokio::time::sleep;
use tokio_stream::wrappers::BroadcastStream;

use crate::build::BuildSystem;
use crate::config::{RtcBuild, RtcWatch};

/// Blacklisted path segments which are ignored by the watcher by default.
const BLACKLIST: [&str; 1] = [".git"];

/// A watch system wrapping a build system and a watcher.
pub struct WatchSystem {
    /// The build system.
    build: BuildSystem,
    /// The current vector of paths to be ignored.
    ignored_paths: Vec<PathBuf>,
    /// A channel of FS watch events.
    watch_rx: mpsc::Receiver<Event>,
    /// A channel of new paths to ignore from the build system.
    build_rx: mpsc::Receiver<PathBuf>,
    /// The watch system used for watching the filesystem.
    _watcher: RecommendedWatcher,
    /// The application shutdown channel.
    shutdown: BroadcastStream<()>,

    build_start_tx: std::sync::mpsc::Sender<()>,
}

impl WatchSystem {
    /// Create a new instance.
    pub async fn new(cfg: Arc<RtcWatch>, shutdown: broadcast::Sender<()>, build_done_tx: Option<broadcast::Sender<()>>) -> Result<Self> {
        // Create a channel for being able to listen for new paths to ignore while running.
        let (watch_tx, watch_rx) = mpsc::channel(1);
        let (build_tx, build_rx) = mpsc::channel(1);

        // Build the watcher.
        let _watcher = build_watcher(watch_tx, cfg.paths.clone())?;

        // Build dependencies.
        let build = BuildSystem::new(cfg.build.clone(), Some(build_tx)).await?;

        let (build_start_tx, build_start_rx) = std::sync::mpsc::channel();
        start_build_thread(cfg.build.clone(), build_start_rx, build_done_tx);

        Ok(Self {
            build,
            ignored_paths: cfg.ignored_paths.clone(),
            watch_rx,
            build_rx,
            _watcher,
            shutdown: BroadcastStream::new(shutdown.subscribe()),
            build_start_tx,
        })
    }

    /// Run a build.
    #[tracing::instrument(level = "trace", skip(self))]
    pub async fn build(&mut self) -> Result<()> {
        self.build.build().await
    }

    /// Run the watch system, responding to events and triggering builds.
    #[tracing::instrument(level = "trace", skip(self))]
    pub async fn run(mut self) {
        loop {
            tokio::select! {
                Some(ign) = self.build_rx.recv() => self.update_ignore_list(ign),
                Some(ev) = self.watch_rx.recv() => self.handle_watch_event(ev).await,
                _ = self.shutdown.next() => break, // Any event, even a drop, will trigger shutdown.
            }
        }

        tracing::debug!("watcher system has shut down");
    }

    #[tracing::instrument(level = "trace", skip(self, event))]
    async fn handle_watch_event(&mut self, event: Event) {
        if matches!(&event.kind, EventKind::Access(_) | EventKind::Any | EventKind::Other) {
            return; // Nothing to do with these.
        }

        for ev_path in event.paths {
            let ev_path = match tokio::fs::canonicalize(&ev_path).await {
                Ok(ev_path) => ev_path,
                // Ignore errors here, as this would only take place for a resource which has
                // been removed, which will happen for each of our dist/.stage entries.
                Err(_) => continue,
            };

            // Check ignored paths.
            if ev_path
                .ancestors()
                .any(|path| self.ignored_paths.iter().any(|ignored_path| ignored_path == path))
            {
                continue; // Don't emit a notification if path is ignored.
            }

            // Check blacklisted paths.
            if ev_path
                .components()
                .filter_map(|segment| segment.as_os_str().to_str())
                .any(|segment| BLACKLIST.contains(&segment))
            {
                continue; // Don't emit a notification as path is on the blacklist.
            }

            tracing::debug!("change detected in {:?}", ev_path);
            // let _res = self.build.build().await;
            self.build_start_tx.send(()).unwrap();

            return; // If one of the paths triggers a build, then we're done.
        }
    }

    fn update_ignore_list(&mut self, arg_path: PathBuf) {
        let path = match arg_path.canonicalize() {
            Ok(canon_path) => canon_path,
            Err(_) => arg_path,
        };

        if !self.ignored_paths.contains(&path) {
            self.ignored_paths.push(path);
        }
    }
}

/// Build a FS watcher, when the watcher is dropped, it will stop watching for events.
fn build_watcher(watch_tx: mpsc::Sender<Event>, paths: Vec<PathBuf>) -> Result<RecommendedWatcher> {
    let event_handler = move |event_res: notify::Result<Event>| match event_res {
        Ok(event) => {
            let _res = watch_tx.try_send(event);
        }
        Err(err) => {
            tracing::error!(error = ?err, "error from FS watcher");
        }
    };
    let mut watcher = recommended_watcher(event_handler).context("failed to build file system watcher")?;

    // Create a recursive watcher on each of the given paths.
    // NOTE WELL: it is expected that all given paths are canonical. The Trunk config
    // system currently ensures that this is true for all data coming from the
    // RtcBuild/RtcWatch/RtcServe/&c runtime config objects.
    for path in paths {
        watcher
            .watch(&path, RecursiveMode::Recursive)
            .context(format!("failed to watch {:?} for file system changes", path))?;
    }

    Ok(watcher)
}

pub fn start_build_thread(rtc_build: Arc<RtcBuild>, build_start_rx: std::sync::mpsc::Receiver<()>, mut build_done_tx: Option<broadcast::Sender<()>>) {
    tokio::spawn(async move {
        // let mut child: Option<Child> = None;
        // let mut old_child: Option<Child> = None;
        // let build = BuildSystem::new(rtc_build, None).await.unwrap();
        let mut join_handle: Option<JoinHandle<()>> = None;
        loop {
            match build_start_rx.try_recv() {
                Ok(_) => {
                    tracing::debug!("start build...");
                    if let Some(join_handler) = join_handle {
                        tracing::debug!("abort old build task!");
                        join_handler.abort();
                    }
                    while build_start_rx.recv_timeout(Duration::from_millis(100)).is_ok() {}

                    let cfg = rtc_build.clone();
                    join_handle = Some(tokio::spawn(async move {
                        let mut build = BuildSystem::new(cfg, None).await.unwrap();
                        let _res = build.build().await;
                    }));

                    // old_child = child;
                    // child = Some(
                    //     Command::new(std::env::args().next().unwrap())
                    //         .arg("build")
                    //         .kill_on_drop(true)
                    //         .spawn()
                    //         .unwrap(),
                    // );
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
                _ => break,
            }
            // if let Some(x) = old_child.as_mut() {
            //     tracing::debug!("drop {:?}", x.id());
            //     let _ = x.kill().await;
            //     old_child = None;
            // }
            // match child.as_mut().map(|child| child.try_wait()) {
            //     Some(Ok(Some(exit_status))) => {
            //         tracing::debug!("build done => {}", exit_status.success());
            //         child.take();
            //         if exit_status.success() {
            //             if let Some(tx) = build_done_tx.as_mut() {
            //                 tracing::debug!("reload.");
            //                 let _ = tx.send(());
            //             }
            //         }
            //     }
            //     _ => {}
            // }
            sleep(Duration::from_millis(100)).await;
        }
    });
}
