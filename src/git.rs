use std::{
    path::{self, PathBuf},
    process::Command,
    sync::Arc,
};

use parking_lot::RwLock;
use tempfile::TempDir;

use crate::{openapi::UpdatePackageMetadataRequest, Config};

fn git_revparse_head(path: &path::Path) -> String {
    let output = Command::new("git")
        .args(&["rev-parse", "HEAD"])
        .current_dir(path)
        .output()
        .unwrap();
    std::str::from_utf8(&output.stdout)
        .unwrap()
        .trim()
        .to_string()
}

pub struct GitRepo {
    pub(crate) path: PathBuf,
    pub(crate) head_ref: String,
}

impl GitRepo {
    pub fn new(path: PathBuf) -> Self {
        let path = dunce::canonicalize(&path)
            .expect(&format!("Git path does not exist: '{}'", path.display()));
        let head_ref = git_revparse_head(&path);
        Self { path, head_ref }
    }

    pub fn add_package_to_index_tree(
        &mut self,
        repo_id: &str,
        package_id: &str,
    ) -> Result<(), std::io::Error> {
        Command::new("git")
            .arg("add")
            .arg(format!("{}/packages/{}", repo_id, package_id))
            .current_dir(&self.path)
            .status()?;

        Ok(())
    }

    pub fn commit_create(&mut self, repo_id: &str, package_id: &str) -> Result<(), std::io::Error> {
        Command::new("git")
            .args(&["commit", "-m"])
            .arg(format!("[{}:create] `{}`", repo_id, package_id))
            .current_dir(&self.path)
            .status()?;

        self.head_ref = git_revparse_head(&self.path);

        Ok(())
    }

    pub fn commit_update(
        &mut self,
        repo_id: &str,
        package_id: &str,
        release: &UpdatePackageMetadataRequest,
    ) -> Result<(), std::io::Error> {
        Command::new("git")
            .args(&["commit", "-m"])
            .arg(format!("[{}:update] `{} {}`", repo_id, package_id, release))
            .current_dir(&self.path)
            .status()?;

        self.head_ref = git_revparse_head(&self.path);

        Ok(())
    }

    pub fn push(&self, config: &Config) -> Result<(), std::io::Error> {
        Command::new("git")
            .args(&["push", "origin", &format!("HEAD:{}", &config.branch_name)])
            .current_dir(&self.path)
            .status()?;
        Ok(())
    }

    pub fn cleanup(&self, config: &Config) -> Result<(), std::io::Error> {
        Command::new("git")
            .args(&["clean", "-dfx"])
            .current_dir(&self.path)
            .status()?;

        Command::new("git")
            .args(&["fetch", "origin", &config.branch_name])
            .current_dir(&self.path)
            .status()?;

        Command::new("git")
            .args(&["reset", "--hard", &format!("origin/{}", config.branch_name)])
            .current_dir(&self.path)
            .status()?;

        Ok(())
    }

    pub fn shallow_clone_to_tempdir(&self) -> Result<TempDir, std::io::Error> {
        let tmpdir = tempfile::tempdir()?;

        Command::new("git")
            .args(&["clone", "--depth", "1"])
            .arg(format!("file://{}", &self.path.display()))
            .arg(tmpdir.path())
            .output()?;

        Ok(tmpdir)
    }
}

pub type GitRepoMutex = Arc<RwLock<GitRepo>>;
