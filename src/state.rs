use std::{
    collections::{BTreeMap, HashMap},
    sync::Arc,
};

use arc_swap::ArcSwap;
use once_cell::sync::{Lazy, OnceCell};
use parking_lot::RwLock;

use crate::{generate_repo_index, git::GitRepo, Config, RepoIndexData, RepoIndexes};

pub(crate) static REPO_INDEXES: OnceCell<RepoIndexes> = OnceCell::new();
pub(crate) static GIT_REPO: OnceCell<RwLock<GitRepo>> = OnceCell::new();
pub(crate) static SERVER_STATUS: Lazy<ArcSwap<ServerStatus>> = Lazy::new(|| {
    ArcSwap::from_pointee(ServerStatus {
        index_ref: Default::default(),
    })
});

#[derive(Debug, Clone, poem_openapi::Object, async_graphql::SimpleObject)]
pub struct ServerStatus {
    index_ref: BTreeMap<String, String>,
}

pub(crate) fn server_status() -> ServerStatus {
    let index_ref = REPO_INDEXES
        .get()
        .unwrap()
        .iter()
        .map(|(k, v)| (k.clone(), v.load().head_ref.to_string()))
        .collect::<BTreeMap<_, _>>();
    ServerStatus { index_ref }
}

pub(crate) fn set_repo_indexes(state: &ArcSwap<RepoIndexData>, repo_index_data: RepoIndexData) {
    state.swap(Arc::new(repo_index_data));
    SERVER_STATUS.store(Arc::new(server_status()));
}

pub(crate) fn init_repo_indexes(config: &Config) -> Result<(), std::io::Error> {
    let git_repo = GitRepo::new(config.git_path.clone());
    if config.skip_repo_cleanup {
        tracing::warn!("Skipping repo cleanup (due to configuration option)");
    } else {
        tracing::info!("Cleaning up repo state...");
        git_repo.cleanup(&config)?;
    }

    let tmpdir = git_repo.shallow_clone_to_tempdir()?;
    let head_ref = Arc::from(git_repo.head_ref.clone());

    let mut repo_indexes = HashMap::new();
    for repo_id in &config.repos {
        tracing::info!("Updating index for {}...", repo_id);
        let repo_index_data =
            generate_repo_index(Arc::clone(&head_ref), &tmpdir.path().join(repo_id)).unwrap();
        // set_repo_indexes(state, repo_index_data);
        repo_indexes.insert(repo_id.to_string(), ArcSwap::from_pointee(repo_index_data));
    }

    tracing::info!("Finished updating indexes");

    REPO_INDEXES
        .set(Arc::new(repo_indexes))
        .expect("Could not set repo indexes");

    GIT_REPO
        .set(RwLock::new(git_repo))
        .expect("Could not set git repo");

    SERVER_STATUS.store(Arc::new(server_status()));

    Ok(())
}
