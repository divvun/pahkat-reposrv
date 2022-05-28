use std::collections::BTreeMap;

use async_graphql::{Context, Object, SimpleObject};

use crate::{Config, RepoIndexes};

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

    async fn repos(&self, ctx: &Context<'_>) -> Vec<Repo> {
        let repo_indexes = ctx.data_unchecked::<RepoIndexes>();
        repo_indexes
            .keys()
            .map(|x| Repo { key: x.to_string() })
            .collect()
    }
}

struct Repo {
    key: String,
}

#[Object]
impl Repo {
    #[graphql(flatten)]
    async fn _index(&self, ctx: &Context<'_>) -> pahkat_types::repo::Index {
        let config = ctx.data_unchecked::<Config>();
        let index_path = config.git_path.join(&self.key).join("index.toml");

        let output = std::fs::read_to_string(index_path).unwrap();
        ::toml::from_str(&output).unwrap()
    }

    async fn packages(&self, ctx: &Context<'_>) -> Vec<pahkat_types::package::Package> {
        let repo_indexes = ctx.data_unchecked::<RepoIndexes>();
        let (ref packages, _) = repo_indexes[&self.key].load().1;
        packages.iter().cloned().collect()
    }
}
