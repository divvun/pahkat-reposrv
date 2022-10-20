use std::sync::Arc;

use arc_ext::{ArcExt, ArcProjectOption};
use arc_swap::Guard;
use async_graphql::Object;
use pahkat_types::{package::Package, repo::Index};

use crate::{
    state::{ServerStatus, REPO_INDEXES, SERVER_STATUS},
    RepoIndexData,
};

pub struct Query;

#[Object]
impl Query {
    async fn status(&self) -> Arc<ServerStatus> {
        SERVER_STATUS.load_full()
    }

    async fn repos<'a>(&self) -> Vec<Repo> {
        REPO_INDEXES
            .get()
            .unwrap()
            .values()
            .map(|value| Repo {
                model: value.load(),
            })
            .collect()
    }

    async fn repo(&self, id: String) -> Option<Repo> {
        REPO_INDEXES.get().unwrap().get(&id).map(|value| Repo {
            model: value.load(),
        })
    }
}

struct Repo {
    model: Guard<Arc<RepoIndexData>>,
}

#[Object]
impl Repo {
    #[graphql(flatten)]
    async fn _index(&self) -> Arc<Index> {
        self.model.repo_index.clone()
    }

    async fn packages(&self) -> Arc<[Package]> {
        self.model.packages.clone()
    }

    async fn package(&self, id: String) -> ArcProjectOption<[Package], Package> {
        self.model
            .packages
            .clone()
            .project_option(|packages| packages.iter().find(|p| id == p.id()))
    }
}
