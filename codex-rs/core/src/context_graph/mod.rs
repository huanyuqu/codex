//! The context graph of a session: the shadow agent's models, recall of the history the model
//! sees, and the tools that let the agent read history that recall left out.

mod lease;
mod mentions;
mod merge;
mod model;
mod recall;

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;

use anyhow::Context as _;
use codex_config::types::ContextGraphConfig;
use codex_config::types::ContextMode;
use codex_context_graph::BlockId;
use codex_context_graph_store::BranchSnapshot;
use codex_context_graph_store::BranchTree;
use codex_context_graph_store::GraphMembership;
use codex_context_graph_store::LiveContextGraph;
use codex_context_graph_store::LiveIndexing;
use codex_context_graph_store::LocalContextGraphStore;
use codex_context_graph_store::ShadowModel;
use codex_context_graph_store::install_live_recorder;
use codex_features::Feature;
use codex_protocol::ThreadId;
use codex_protocol::user_input::UserInput;
use codex_rollout::RolloutItem;
use codex_thread_store::LiveThread;
use codex_thread_store::ThreadStore;
use tracing::warn;

use crate::config::Config;
use crate::session::session::Session;

pub(crate) use lease::acquire_workspace_lease;
pub(crate) use mentions::resolve_branch_mentions;
pub(crate) use merge::PreparedMerge;
pub(crate) use model::SessionShadowModel;
pub(crate) use recall::RecallTrigger;
pub(crate) use recall::recall_into_history;

/// What a session gets from [`start`]: the services recall needs, and the models to connect to
/// the session once it exists.
pub(crate) struct ContextGraphStart {
    pub(crate) service: Option<Arc<ContextGraphService>>,
    models: Vec<Arc<SessionShadowModel>>,
}

impl ContextGraphStart {
    /// Connects the shadow agent's models to `sess`.
    pub(crate) fn attach(&self, sess: &Arc<Session>) {
        for model in &self.models {
            model.attach(sess);
        }
    }
}

/// Starts recording the thread into its context graph when the `context_graph` feature is on: a
/// fork of a recorded thread joins that thread's graph as a new branch. With
/// `context_graph_indexing`, blocks get model cards. Root sessions also get the context graph
/// services: branch mentions, and with a context mode other than `full`, recall from the graph
/// and from the rollouts in `threads` of the branches they reference. Subagents record but get
/// no services, since their parents manage what they see.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn start(
    config: &Config,
    provider_index_model: &str,
    thread_id: ThreadId,
    forked_from: Option<ThreadId>,
    live_thread: Option<&LiveThread>,
    initial_items: &[RolloutItem],
    root_session: bool,
    threads: Arc<dyn ThreadStore>,
) -> ContextGraphStart {
    let mut start = ContextGraphStart {
        service: None,
        models: Vec::new(),
    };
    let Some(live_thread) = live_thread.filter(|_| config.features.enabled(Feature::ContextGraph))
    else {
        return start;
    };
    let settings = &config.context_graph;
    let index_model_name = settings
        .index_model
        .clone()
        .unwrap_or_else(|| provider_index_model.to_string());
    // Model cards are derived data, like memories: a user who turned memories off keeps blocks
    // to extracted cards, which never leave the machine.
    let index_model = (config.features.enabled(Feature::ContextGraphIndexing)
        && config.memories.generate_memories)
        .then(|| SessionShadowModel::new(index_model_name.clone()));
    start.models.extend(index_model.clone());

    let thread = thread_id.to_string();
    let parent = forked_from.map(|parent| parent.to_string());
    let membership = match LocalContextGraphStore::open(&config.sqlite).await {
        Ok(store) => store
            .membership_for_thread(&thread, parent.as_deref())
            .await
            .unwrap_or_else(|err| {
                warn!("context graph membership lookup failed: {err:#}");
                GraphMembership::root(&thread)
            }),
        Err(err) => {
            warn!("context graph store unavailable: {err:#}");
            GraphMembership::root(&thread)
        }
    };
    let indexing = LiveIndexing {
        model: index_model
            .clone()
            .map(|model| -> Arc<dyn ShadowModel> { model }),
        config: Default::default(),
    };
    let Some(live) = install_live_recorder(
        live_thread,
        membership,
        initial_items,
        config.sqlite.clone(),
        Some(indexing),
    ) else {
        return start;
    };
    if !root_session {
        return start;
    }

    let screen_name = settings
        .screen_model
        .clone()
        .unwrap_or_else(|| index_model_name.clone());
    let mut model = |name: &str| match &index_model {
        Some(model) if model.name() == name => Arc::clone(model),
        _ => {
            let model = SessionShadowModel::new(name);
            start.models.push(Arc::clone(&model));
            model
        }
    };
    let screen_model = model(&screen_name);
    let read_model = match settings.read_model.as_deref() {
        Some("none") => None,
        Some(name) if name != screen_name => Some(model(name)),
        _ => Some(Arc::clone(&screen_model)),
    };
    start.service = Some(Arc::new(ContextGraphService {
        live,
        threads,
        mode: StdMutex::new(settings.mode),
        config: settings.clone(),
        screen_model,
        read_model,
        recalled_references: StdMutex::new(None),
        window: StdMutex::new(None),
    }));
    start
}

/// Records a merge whose note entered the history of `sess`.
pub(crate) async fn commit_merge(sess: &Session, merge: &PreparedMerge) {
    if let Some(service) = &sess.services.context_graph
        && let Err(err) = service.commit_merge(merge).await
    {
        warn!("failed to record a branch merge: {err:#}");
    }
}

/// Forgets what the recalled window holds, after something else replaced the history.
pub(crate) fn forget_window(sess: &Session) {
    if let Some(service) = &sess.services.context_graph {
        service.set_window(None);
    }
}

/// Whether `sess` rebuilds its history by recall instead of summary compaction.
pub(crate) fn recalls(sess: &Session) -> bool {
    sess.services
        .context_graph
        .as_ref()
        .is_some_and(|service| service.recalls())
}

/// The text of a turn's input, as the request recall chooses context for.
pub(crate) fn question_text(input: &[UserInput]) -> String {
    input
        .iter()
        .filter_map(|input| match input {
            UserInput::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The context graph services of one session.
pub(crate) struct ContextGraphService {
    live: Arc<LiveContextGraph>,
    /// Where the rollouts of referenced branches are read from.
    threads: Arc<dyn ThreadStore>,
    /// The context mode, which starts from the configuration and can change while the session
    /// runs.
    mode: StdMutex<ContextMode>,
    config: ContextGraphConfig,
    screen_model: Arc<SessionShadowModel>,
    read_model: Option<Arc<SessionShadowModel>>,
    /// How many branches the history referenced when it was last recalled.
    recalled_references: StdMutex<Option<usize>>,
    /// What the context window holds since the last recall rebuilt it; `None` while the window
    /// holds the whole history, such as before the first recall or after summary compaction.
    window: StdMutex<Option<RecalledWindow>>,
}

/// The blocks a recalled context window holds.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct RecalledWindow {
    /// Blocks the recalled view showed, and blocks recall added to the window since.
    pub(crate) shown: HashSet<BlockId>,
    /// The branch's own blocks when the view was built; own blocks recorded after it are in the
    /// window as they were appended.
    pub(crate) known: HashSet<BlockId>,
}

impl RecalledWindow {
    pub(crate) fn holds(&self, id: &BlockId, own: bool) -> bool {
        self.shown.contains(id) || (own && !self.known.contains(id))
    }
}

impl ContextGraphService {
    /// The branch's history with the blocks of the branches it references, with the blocks
    /// whose files changed since they read them marked.
    pub(crate) async fn snapshot(&self) -> anyhow::Result<BranchSnapshot> {
        let mut snapshot = self
            .live
            .snapshot_with_references(self.threads.as_ref())
            .await?;
        snapshot.mark_changed_files();
        Ok(snapshot)
    }

    /// The branches of this conversation, with this thread's branch current.
    pub(crate) async fn branch_tree(&self) -> anyhow::Result<BranchTree> {
        self.live
            .store()
            .await?
            .branch_tree(&self.live.membership().thread_id)
            .await?
            .context("this conversation has no recorded branches yet")
    }

    /// Names the branch the thread `thread_id` records into, now or once it first records.
    pub(crate) async fn name_thread_branch(
        &self,
        thread_id: &str,
        name: &str,
    ) -> anyhow::Result<()> {
        self.live
            .store()
            .await?
            .name_thread_branch(thread_id, name)
            .await?;
        Ok(())
    }

    pub(crate) fn mode(&self) -> ContextMode {
        self.mode
            .lock()
            .map(|mode| *mode)
            .unwrap_or(self.config.mode)
    }

    pub(crate) fn set_mode(&self, mode: ContextMode) {
        if let Ok(mut current) = self.mode.lock() {
            *current = mode;
        }
    }

    /// Whether recall replaces summary compaction in this session.
    pub(crate) fn recalls(&self) -> bool {
        self.mode() != ContextMode::Full
    }

    pub(crate) fn window(&self) -> Option<RecalledWindow> {
        self.window.lock().ok().and_then(|window| window.clone())
    }

    pub(crate) fn set_window(&self, window: Option<RecalledWindow>) {
        if let Ok(mut current) = self.window.lock() {
            *current = window;
        }
    }
}
