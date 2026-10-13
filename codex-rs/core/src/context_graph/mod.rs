//! The context graph of a session: the shadow agent's models, recall of the history the model
//! sees, and the tools that let the agent read history that recall left out.

mod model;
mod recall;

use std::sync::Arc;

use codex_config::types::ContextGraphConfig;
use codex_config::types::ContextMode;
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
use tracing::warn;

use crate::config::Config;
use crate::session::session::Session;

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
/// `context_graph_indexing`, blocks get model cards; with a context mode other than `full`,
/// recall rebuilds the history from the graph.
pub(crate) async fn start(
    config: &Config,
    provider_index_model: &str,
    thread_id: ThreadId,
    forked_from: Option<ThreadId>,
    live_thread: Option<&LiveThread>,
    initial_items: &[RolloutItem],
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
    let index_model = config
        .features
        .enabled(Feature::ContextGraphIndexing)
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
    if settings.mode == ContextMode::Full {
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
    start.service = Some(Arc::new(ContextGraphService::new(
        live,
        settings.clone(),
        screen_model,
        read_model,
    )));
    start
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
    config: ContextGraphConfig,
    screen_model: Arc<SessionShadowModel>,
    read_model: Option<Arc<SessionShadowModel>>,
}

impl ContextGraphService {
    pub(crate) fn new(
        live: Arc<LiveContextGraph>,
        config: ContextGraphConfig,
        screen_model: Arc<SessionShadowModel>,
        read_model: Option<Arc<SessionShadowModel>>,
    ) -> Self {
        Self {
            live,
            config,
            screen_model,
            read_model,
        }
    }

    pub(crate) fn live(&self) -> &Arc<LiveContextGraph> {
        &self.live
    }

    /// Whether recall replaces summary compaction in this session.
    pub(crate) fn recalls(&self) -> bool {
        self.config.mode != ContextMode::Full
    }
}
