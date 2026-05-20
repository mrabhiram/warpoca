use super::model::{GlobalRulesDelta, ProjectContextModel, ProjectContextModelEvent, ProjectRule};
use async_channel::Sender;
use repo_metadata::repository::{RepositorySubscriber, SubscriberId};
use repo_metadata::{DirectoryWatcher, Repository, RepositoryUpdate};
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use strum::IntoEnumIterator;
use strum_macros::EnumIter;
use warp_core::safe_warn;
use warp_util::standardized_path::StandardizedPath;
use warpui::{ModelContext, ModelHandle, SingletonEntity};
use watcher::{HomeDirectoryWatcher, HomeDirectoryWatcherEvent};

/// A well-known location that may contain a global rule file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, EnumIter)]
enum GlobalRuleSource {
    /// `~/.agents/AGENTS.md`.
    Agents,
    /// WarpOCA's local rules directory.
    WarpOcaRules,
    /// WarpOCA's local knowledge directory.
    WarpOcaKnowledge,
}

impl GlobalRuleSource {
    /// Display name (used in safe logs that don't expose user paths).
    fn name(self) -> &'static str {
        match self {
            Self::Agents => "agents",
            Self::WarpOcaRules => "warpoca-rules",
            Self::WarpOcaKnowledge => "warpoca-knowledge",
        }
    }

    /// Directory watched for this source.
    fn directory(self, home_dir: &Path) -> PathBuf {
        match self {
            Self::Agents => home_dir.join(".agents"),
            Self::WarpOcaRules => warpoca_rules_dir(home_dir),
            Self::WarpOcaKnowledge => warpoca_knowledge_dir(home_dir),
        }
    }

    fn should_create_dir(self) -> bool {
        matches!(self, Self::WarpOcaRules | Self::WarpOcaKnowledge)
    }

    fn matches_path(self, path: &Path, source_dir: &Path) -> bool {
        if path.parent() != Some(source_dir) {
            return false;
        }

        match self {
            Self::Agents => path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.eq_ignore_ascii_case("AGENTS.md")),
            Self::WarpOcaRules | Self::WarpOcaKnowledge => path
                .extension()
                .and_then(|ext| ext.to_str())
                .is_some_and(|ext| ext.eq_ignore_ascii_case("md")),
        }
    }

    fn initial_rule_paths(self, source_dir: &Path) -> Vec<PathBuf> {
        match self {
            Self::Agents => vec![source_dir.join("AGENTS.md")],
            Self::WarpOcaRules | Self::WarpOcaKnowledge => fs::read_dir(source_dir)
                .ok()
                .into_iter()
                .flat_map(|entries| entries.filter_map(Result::ok))
                .map(|entry| entry.path())
                .filter(|path| self.matches_path(path, source_dir))
                .collect(),
        }
    }
}

pub fn warpoca_app_support_dir(home_dir: &Path) -> PathBuf {
    #[cfg(target_os = "macos")]
    {
        return home_dir.join("Library/Application Support/WarpOCA");
    }

    #[cfg(target_os = "windows")]
    {
        return std::env::var_os("APPDATA")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| home_dir.join("AppData").join("Roaming"))
            .join("WarpOCA");
    }

    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        std::env::var_os("XDG_DATA_HOME")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| home_dir.join(".local").join("share"))
            .join("WarpOCA")
    }
}

pub fn warpoca_rules_dir(home_dir: &Path) -> PathBuf {
    warpoca_app_support_dir(home_dir).join("rules")
}

pub fn warpoca_knowledge_dir(home_dir: &Path) -> PathBuf {
    warpoca_app_support_dir(home_dir).join("knowledge")
}

#[derive(Debug)]
struct GlobalSourceWatcherState {
    repository: ModelHandle<Repository>,
    subscriber_id: SubscriberId,
}

#[derive(Debug)]
struct GlobalRulesUpdate {
    /// The [`GlobalRuleSource`] variant that produced this update. The
    /// receiver uses it to look up the matching `home_subdir`/`file_pattern`
    /// without needing a per-source channel.
    source: GlobalRuleSource,
    update: RepositoryUpdate,
}

#[derive(Debug, Default)]
pub(crate) struct GlobalRules {
    /// Global rule files keyed by absolute file path. Populated from
    /// [`GlobalRuleSource`]. Independent of project-level rule indexing.
    /// Stored in a `BTreeMap` so iteration order is deterministic.
    pub(super) rules: BTreeMap<PathBuf, ProjectRule>,
    /// Active home-subdir directory watchers, keyed by the absolute subdir
    /// path (e.g. `~/.agents`).
    source_watchers: HashMap<PathBuf, GlobalSourceWatcherState>,
    /// Sender used by global-rule directory subscribers to push updates back
    /// into the model's main-thread stream handler.
    updates_tx: Option<Sender<GlobalRulesUpdate>>,
}

impl GlobalRules {
    pub(crate) fn active_rules(&self) -> impl Iterator<Item = ProjectRule> + '_ {
        self.rules.values().cloned()
    }

    pub(crate) fn paths(&self) -> impl Iterator<Item = PathBuf> + '_ {
        self.rules.keys().cloned()
    }

    pub(crate) fn first_rule_parent(&self) -> Option<PathBuf> {
        self.rules
            .values()
            .next()
            .and_then(|rule| rule.path.parent().map(|p| p.to_path_buf()))
    }

    /// Index all configured global rule sources (see [`GlobalRuleSource`]).
    ///
    /// All disk I/O is dispatched through `ctx.spawn` so this method does not
    /// block startup. Subscribes to [`HomeDirectoryWatcher`] to react to
    /// creation/deletion of the home subdirs at runtime, and registers a
    /// [`DirectoryWatcher`] per existing subdir for incremental updates.
    ///
    /// Idempotent: subsequent calls are a no-op once the channel is initialized.
    pub(crate) fn index(&mut self, ctx: &mut ModelContext<ProjectContextModel>) {
        if self.updates_tx.is_some() {
            return;
        }

        let Some(home_dir) = dirs::home_dir() else {
            log::debug!("Home directory not found; skipping global rules indexing");
            return;
        };

        // Set up the channel that all per-source subscribers push into.
        let (tx, rx) = async_channel::unbounded::<GlobalRulesUpdate>();
        self.updates_tx = Some(tx);

        ctx.spawn_stream_local(
            rx,
            |me, update, ctx| {
                me.global_rules
                    .handle_global_rules_update(update.source, update.update, ctx);
            },
            |_, _| {},
        );

        // React to creation/deletion of home subdirs at runtime.
        ctx.subscribe_to_model(&HomeDirectoryWatcher::handle(ctx), |me, event, ctx| {
            me.global_rules
                .handle_home_dir_event_for_global_rules(event, ctx);
        });

        for source in GlobalRuleSource::iter() {
            let subdir_path = source.directory(&home_dir);
            if source.should_create_dir() {
                if let Err(err) = fs::create_dir_all(&subdir_path) {
                    safe_warn!(
                        safe: (
                            "Failed to create {} directory for local global rules",
                            source.name()
                        ),
                        full: (
                            "Failed to create {} directory for local global rules: {err}",
                            subdir_path.display()
                        )
                    );
                }
            }

            // Initial async reads; if files don't exist yet, the watcher will
            // pick them up on creation.
            for target_file in source.initial_rule_paths(&subdir_path) {
                Self::spawn_global_rule_read(target_file, ctx);
            }

            if subdir_path.exists() {
                self.register_global_source_watcher(source, &subdir_path, ctx);
            }
        }
    }

    /// Async read of a single global rule file. The async block runs on a
    /// background executor; the main-thread callback updates model state once
    /// the read completes.
    fn spawn_global_rule_read(file_path: PathBuf, ctx: &mut ModelContext<ProjectContextModel>) {
        ctx.spawn(
            async move {
                // `read_to_string` returning `Err` (e.g. NotFound, permission
                // denied, file replaced with a non-regular file) is converted
                // to `None`; the callback below decides whether that means
                // "insert/refresh" or "drop a previously-known entry."
                let content = async_fs::read_to_string(&file_path).await.ok();
                (file_path, content)
            },
            move |me, (file_path, content_opt), ctx| match content_opt {
                Some(content) => {
                    // Read succeeded: insert (or replace) the rule and notify
                    // subscribers.
                    me.global_rules.rules.insert(
                        file_path.clone(),
                        ProjectRule {
                            path: file_path.clone(),
                            content,
                        },
                    );
                    ctx.emit(ProjectContextModelEvent::GlobalRulesChanged(
                        GlobalRulesDelta {
                            discovered_rules: vec![file_path],
                            deleted_rules: vec![],
                        },
                    ));
                }
                None => {
                    // Drop cached content if file is now unreadable; no-op if it never existed.
                    if me.global_rules.rules.remove(&file_path).is_some() {
                        ctx.emit(ProjectContextModelEvent::GlobalRulesChanged(
                            GlobalRulesDelta {
                                discovered_rules: vec![],
                                deleted_rules: vec![file_path],
                            },
                        ));
                    }
                }
            },
        );
    }

    /// Register a `DirectoryWatcher` on the given home subdir for incremental
    /// updates. Idempotent: subsequent calls for an already-watched subdir are
    /// a no-op (the `subdir_path` key dedups by directory rather than by
    /// source, so multiple sources sharing a `home_subdir` would only register
    /// the watcher once — a future change can fan out to multiple file
    /// patterns by extending the value).
    ///
    /// The subdir must exist on disk before this is called.
    /// `DirectoryWatcher::add_directory` rejects non-existent paths, and
    /// runtime creation is handled by `handle_home_dir_event_for_global_rules`,
    /// which calls back here once the subdir appears.
    fn register_global_source_watcher(
        &mut self,
        source: GlobalRuleSource,
        subdir_path: &Path,
        ctx: &mut ModelContext<ProjectContextModel>,
    ) {
        // If the subdir is already being watched, return early.
        if self.source_watchers.contains_key(subdir_path) {
            return;
        }

        let (Some(update_tx), Ok(std_path)) = (
            self.updates_tx.clone(),
            StandardizedPath::from_local_canonicalized(subdir_path),
        ) else {
            return;
        };

        let repo_handle = match DirectoryWatcher::handle(ctx)
            .update(ctx, |watcher, ctx| watcher.add_directory(std_path, ctx))
        {
            Ok(handle) => handle,
            Err(err) => {
                // `safe_warn!` because the path contains the user's home dir,
                // which is PII; we only want the full path on dogfood builds.
                // The error itself can also embed the canonicalized path
                // (e.g. `RepoMetadataError::RepoNotFound(...)`), so we keep
                // it out of the safe branch as well — only the source name
                // is safe to send to Sentry.
                safe_warn!(
                    safe: (
                        "Failed to register {} for global rules watching",
                        source.name()
                    ),
                    full: (
                        "Failed to register {} for global rules watching: {err}",
                        subdir_path.display()
                    )
                );
                return;
            }
        };

        let subscriber = Box::new(GlobalRulesRepositorySubscriber { source, update_tx });

        let start = repo_handle.update(ctx, |repo, ctx| repo.start_watching(subscriber, ctx));
        let subscriber_id = start.subscriber_id;
        let subdir_path_owned = subdir_path.to_path_buf();

        self.source_watchers.insert(
            subdir_path_owned.clone(),
            GlobalSourceWatcherState {
                repository: repo_handle.clone(),
                subscriber_id,
            },
        );

        let cleanup_key = subdir_path_owned.clone();
        let subdir_for_log = subdir_path_owned;
        ctx.spawn(start.registration_future, move |me, res, ctx| {
            if let Err(err) = res {
                // Same PII shape as the registration error above: the path
                // and the error can both contain the user's home dir, so
                // both stay in the `full` branch only.
                safe_warn!(
                    safe: (
                        "Failed to start watching {} for global rules",
                        source.name()
                    ),
                    full: (
                        "Failed to start watching {} for global rules: {err}",
                        subdir_for_log.display()
                    )
                );
                // Remove the stored watcher since registration failed.
                if let Some(state) = me.global_rules.source_watchers.remove(&cleanup_key) {
                    state.repository.update(ctx, |repo, ctx| {
                        repo.stop_watching(state.subscriber_id, ctx);
                    });
                }
            }
        });
    }

    /// Handle an incremental update for the given global source.
    fn handle_global_rules_update(
        &mut self,
        source: GlobalRuleSource,
        update: RepositoryUpdate,
        ctx: &mut ModelContext<ProjectContextModel>,
    ) {
        if update.is_empty() {
            return;
        }
        let Some(home_dir) = dirs::home_dir() else {
            return;
        };
        let source_dir = source.directory(&home_dir);

        for deleted_file in update
            .deleted
            .iter()
            .map(|file| &file.path)
            .chain(update.moved.values().map(|file| &file.path))
            .filter(|path| source.matches_path(path, &source_dir))
            .cloned()
            .collect::<Vec<_>>()
        {
            if self.rules.remove(&deleted_file).is_some() {
                ctx.emit(ProjectContextModelEvent::GlobalRulesChanged(
                    GlobalRulesDelta {
                        discovered_rules: vec![],
                        deleted_rules: vec![deleted_file],
                    },
                ));
            }
        }

        for updated_file in update
            .added_or_modified()
            .map(|file| &file.path)
            .chain(update.moved.keys().map(|file| &file.path))
            .filter(|path| source.matches_path(path, &source_dir))
            .cloned()
            .collect::<Vec<_>>()
        {
            Self::spawn_global_rule_read(updated_file, ctx);
        }
    }

    /// React to creation/deletion of the registered home subdirs at runtime.
    fn handle_home_dir_event_for_global_rules(
        &mut self,
        event: &HomeDirectoryWatcherEvent,
        ctx: &mut ModelContext<ProjectContextModel>,
    ) {
        let HomeDirectoryWatcherEvent::HomeFilesChanged(fs_event) = event;
        let Some(home_dir) = dirs::home_dir() else {
            log::warn!("Home directory not found; skipping global rules home dir event");
            return;
        };

        for source in GlobalRuleSource::iter() {
            let subdir_path = source.directory(&home_dir);

            let subdir_deleted = fs_event.deleted.contains(&subdir_path)
                || fs_event.moved.values().any(|v| v == &subdir_path);
            if subdir_deleted {
                if let Some(state) = self.source_watchers.remove(&subdir_path) {
                    state.repository.update(ctx, |repo, ctx| {
                        repo.stop_watching(state.subscriber_id, ctx);
                    });
                }
                let deleted_rules = self
                    .rules
                    .keys()
                    .filter(|path| source.matches_path(path, &subdir_path))
                    .cloned()
                    .collect::<Vec<_>>();
                for target_file in &deleted_rules {
                    self.rules.remove(target_file);
                }
                if !deleted_rules.is_empty() {
                    ctx.emit(ProjectContextModelEvent::GlobalRulesChanged(
                        GlobalRulesDelta {
                            discovered_rules: vec![],
                            deleted_rules,
                        },
                    ));
                }
            }

            let subdir_added =
                fs_event.added.contains(&subdir_path) || fs_event.moved.contains_key(&subdir_path);
            if subdir_added {
                // Kick off the read first, then register the watcher for subsequent edits.
                for target_file in source.initial_rule_paths(&subdir_path) {
                    Self::spawn_global_rule_read(target_file, ctx);
                }
                self.register_global_source_watcher(source, &subdir_path, ctx);
            }
        }
    }
}

/// Subscriber for a single global rules home subdir (e.g. `~/.agents`).
/// Tags every update with the originating [`GlobalRuleSource`] variant so the
/// model can dispatch to the right entry without per-source channels.
struct GlobalRulesRepositorySubscriber {
    source: GlobalRuleSource,
    update_tx: Sender<GlobalRulesUpdate>,
}

impl RepositorySubscriber for GlobalRulesRepositorySubscriber {
    fn on_scan(
        &mut self,
        _repository: &Repository,
        _ctx: &mut ModelContext<Repository>,
    ) -> std::pin::Pin<Box<dyn std::prelude::rust_2024::Future<Output = ()> + Send + 'static>> {
        // Initial-state read is performed separately by `spawn_global_rule_read`,
        // so the on_scan event is intentionally a no-op.
        Box::pin(async {})
    }

    fn on_files_updated(
        &mut self,
        _repository: &Repository,
        update: &repo_metadata::RepositoryUpdate,
        _ctx: &mut ModelContext<Repository>,
    ) -> std::pin::Pin<Box<dyn std::prelude::rust_2024::Future<Output = ()> + Send + 'static>> {
        let tx = self.update_tx.clone();
        let source = self.source;
        let update = update.clone();
        Box::pin(async move {
            let _ = tx.send(GlobalRulesUpdate { source, update }).await;
        })
    }
}
