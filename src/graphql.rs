use std::sync::Arc;

use async_graphql::Object;

use crate::state::{ServerStatus, REPO_INDEXES, SERVER_STATUS};

pub struct Query;

#[Object]
impl Query {
    async fn status(&self) -> Arc<ServerStatus> {
        SERVER_STATUS.load_full()
    }

    async fn repos<'a>(&self) -> Vec<Repo<'a>> {
        REPO_INDEXES
            .get()
            .unwrap()
            .keys()
            .map(|key| Repo { key })
            .collect()
    }
}

struct Repo<'a> {
    key: &'a str,
}

#[Object]
impl Repo<'_> {
    #[graphql(flatten)]
    async fn _index(&self) -> Arc<pahkat_types::repo::Index> {
        let repo_indexes = REPO_INDEXES.get().unwrap();
        let value = repo_indexes.get(self.key).unwrap();
        let index_data = value.load();
        index_data.repo_index.clone()
    }

    async fn packages(&self) -> Arc<[pahkat_types::package::Package]> {
        let repo_indexes = REPO_INDEXES.get().unwrap();
        let value = repo_indexes.get(self.key).unwrap();
        let index_data = value.load();
        index_data.packages.clone()
    }
}
