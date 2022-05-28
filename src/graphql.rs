use std::collections::BTreeMap;

use async_graphql::{Context, Object, SimpleObject};

use crate::RepoIndexes;

pub struct Query;

#[derive(Debug, Clone, SimpleObject)]
struct ServerStatus {
    index_ref: BTreeMap<String, String>,
}

#[Object]
impl Query {
    async fn status(&self, ctx: &Context<'_>) -> ServerStatus {
        let repo_indexes = ctx.data_unchecked::<RepoIndexes>();
        ServerStatus {
            index_ref: crate::server_status(&repo_indexes),
        }
    }
}
