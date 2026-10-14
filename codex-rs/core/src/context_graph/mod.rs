//! The context graph of a session: the shadow agent's models, recall of the history the model
//! sees, and the tools that let the agent read history that recall left out.

mod browse;
mod code_merge;
mod lease;
mod mentions;
mod merge;
mod model;
mod recall;

use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use anyhow::Context as _;
use codex_config::types::ContextGraphConfig;
use codex_config::types::ContextMode;
use codex_config::types::RecallScreen;
use codex_context_graph::BlockId;
use codex_context_graph_store::BranchSnapshot;
use codex_context_graph_store::BranchTree;
use codex_context_graph_store::CardsListener;
use codex_context_graph_store::GraphMembership;
use codex_context_graph_store::LiveContextGraph;
use codex_context_graph_store::LiveIndexing;
use codex_context_graph_store::LocalContextGraphStore;
use codex_context_graph_store::ModelScorer;
use codex_context_graph_store::RelevanceScorer;
use codex_context_graph_store::RerankScorer;
use codex_context_graph_store::ShadowModel;
use codex_context_graph_store::install_live_recorder;
use codex_features::Feature;
use codex_protocol::ThreadId;
use codex_protocol::protocol::ContextGraphCardsIndexedEvent;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use codex_protocol::user_input::UserInput;
use codex_rollout::RolloutItem;
use codex_thread_store::LiveThread;
use codex_thread_store::ThreadStore;
use tracing::warn;

use crate::config::Config;
use crate::session::session::Session;

pub use browse::ContextBlockText;
pub(crate) use lease::acquire_workspace_lease;
pub(crate) use mentions::resolve_branch_mentions;
pub(crate) use merge::MergeOptions;
pub(crate) use merge::PreparedMerge;
pub(crate) use model::SessionShadowModel;
pub(crate) use recall::RecallTrigger;
pub(crate) use recall::add_finished_recall;
pub(crate) use recall::recall_into_history;
pub(crate) use recall::speculate_recall;

/// The agent that spawned a thread without giving it the conversation's history.
#[derive(Debug, Clone)]
pub(crate) struct SpawnedBy {
    pub(crate) parent: ThreadId,
    pub(crate) nickname: Option<String>,
}

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
    spawned_by: Option<SpawnedBy>,
    threads: Arc<dyn ThreadStore>,
    events: async_channel::Sender<Event>,
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
        Ok(store) => match (&parent, spawned_by) {
            // An agent spawned without the history joins its parent's graph as a branch of its
            // own.
            (None, Some(spawned_by)) => {
                store
                    .membership_for_spawned_thread(
                        &thread,
                        &spawned_by.parent.to_string(),
                        spawned_by.nickname.as_deref(),
                    )
                    .await
            }
            _ => {
                store
                    .membership_for_thread(&thread, parent.as_deref())
                    .await
            }
        }
        .unwrap_or_else(|err| {
            warn!("context graph membership lookup failed: {err:#}");
            GraphMembership::root(&thread)
        }),
        Err(err) => {
            warn!("context graph store unavailable: {err:#}");
            GraphMembership::root(&thread)
        }
    };
    let graph_id = membership.graph_id.clone();
    let on_cards: CardsListener = Arc::new(move |blocks| {
        // Clients refresh what they show of the graph; a closed session has no one to tell.
        let _ = events.try_send(Event {
            id: String::new(),
            msg: EventMsg::ContextGraphCardsIndexed(ContextGraphCardsIndexedEvent {
                graph_id: graph_id.clone(),
                blocks: u32::try_from(blocks).unwrap_or(u32::MAX),
            }),
        });
    });
    let indexing = LiveIndexing {
        model: index_model
            .clone()
            .map(|model| -> Arc<dyn ShadowModel> { model }),
        config: Default::default(),
        on_cards: Some(on_cards),
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
    let escalate_model = settings
        .escalate_model
        .as_deref()
        .filter(|name| *name != screen_name)
        .map(&mut model);
    let scorer: Option<Arc<dyn RelevanceScorer>> = match settings.screen {
        RecallScreen::Listwise => None,
        RecallScreen::Pointwise | RecallScreen::Cascade => Some(match &settings.rerank_url {
            Some(url) => Arc::new(RerankScorer::new(
                url.clone(),
                settings.rerank_model.clone(),
                settings
                    .rerank_api_key_env
                    .as_deref()
                    .and_then(|name| std::env::var(name).ok()),
            )),
            None => Arc::new(ModelScorer::new(screen_model.clone())),
        }),
    };
    start.service = Some(Arc::new(ContextGraphService {
        live,
        threads,
        mode: StdMutex::new(settings.mode),
        config: settings.clone(),
        screen_model,
        read_model,
        escalate_model,
        scorer,
        recalled_references: StdMutex::new(None),
        window: StdMutex::new(None),
        rebuild_requested: AtomicBool::new(false),
        job: StdMutex::new(None),
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
    /// Screens again when the first screening is unsure.
    escalate_model: Option<Arc<SessionShadowModel>>,
    /// Judges cards for pointwise and cascade screening.
    scorer: Option<Arc<dyn RelevanceScorer>>,
    /// How many branches the history referenced when it was last recalled.
    recalled_references: StdMutex<Option<usize>>,
    /// What the context window holds since the last recall rebuilt it; `None` while the window
    /// holds the whole history, such as before the first recall or after summary compaction.
    window: StdMutex<Option<RecalledWindow>>,
    /// Whether the user asked to rebuild the history by recall before the next turn.
    rebuild_requested: AtomicBool,
    /// The latest recall run in the background: of a turn's request, when the turn went on
    /// without it, or of the turn that just ended, for the next one.
    job: StdMutex<Option<recall::RecallJob>>,
}

/// The blocks a recalled context window holds.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct RecalledWindow {
    /// Blocks the recalled view showed, and blocks recall added to the window since, with how
    /// much of each, as a [`Presentation::detail`].
    ///
    /// [`Presentation::detail`]: codex_context_graph::view::Presentation::detail
    pub(crate) shown: HashMap<BlockId, u8>,
    /// The branch's own blocks when the view was built; own blocks recorded after it are in the
    /// window as they were appended.
    pub(crate) known: HashSet<BlockId>,
}

impl RecalledWindow {
    /// Whether the window shows the block `id` with at least `detail`.
    pub(crate) fn holds(&self, id: &BlockId, own: bool, detail: u8) -> bool {
        self.shown.get(id).is_some_and(|shown| *shown >= detail)
            || (own && !self.known.contains(id))
    }

    /// Records that the window now shows `id` with `detail`.
    pub(crate) fn show(&mut self, id: BlockId, detail: u8) {
        let shown = self.shown.entry(id).or_default();
        *shown = (*shown).max(detail);
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

    /// Asks for the history to be rebuilt by recall before the next turn.
    pub(crate) fn request_rebuild(&self) {
        self.rebuild_requested.store(true, Ordering::SeqCst);
    }

    /// Whether a rebuild was asked for, clearing the request.
    pub(crate) fn take_rebuild_request(&self) -> bool {
        self.rebuild_requested.swap(false, Ordering::SeqCst)
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
