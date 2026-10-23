//! BuildProvider -- SPEC §35.3, §35.4
//! Independent build workspace, cargo/git operations, artifact identity.
use std::path::PathBuf;
use anyhow::{anyhow, Result};
use tracing::{info, warn};
use serde::{Deserialize, Serialize};
use chrono::{DateTime, Utc};
use tokio::process::Command;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuildWorkspace {
    pub workspace_id: String,
    pub base_repo: PathBuf,
    pub worktree_path: PathBuf,
    pub branch: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuildResult {
    pub workspace_id: String,
    pub branch: String,
    pub artifact_paths: Vec<PathBuf>,
    pub cargo_check_passed: bool,
    pub test_passed: bool,
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
    pub built_at: DateTime<Utc>,
    pub source_sha: Option<String>,
}

impl BuildResult {
    pub fn success(&self) -> bool { self.cargo_check_passed && self.exit_code == 0 }
}

pub struct BuildProvider {
    pub repo_root: PathBuf,
}

impl BuildProvider {
    pub fn new(repo_root: PathBuf) -> Self { Self { repo_root } }

    pub async fn current_sha(&self) -> Result<String> {
        let out = Command::new("git")
            .args(["-C", self.repo_root.to_str().unwrap_or("."), "rev-parse", "HEAD"])
            .output().await?;
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    }

    pub async fn create_worktree(&self, branch: &str, base: &str) -> Result<BuildWorkspace> {
        let workspace_id = format!("wt-{}", chrono::Utc::now().timestamp());
        let worktree_path = std::env::temp_dir().join("shenko-build").join(&workspace_id);

        let out = Command::new("git")
            .args(["-C", self.repo_root.to_str().unwrap_or("."),
                "worktree", "add", "-b", branch,
                worktree_path.to_str().unwrap_or("/tmp/shenko-wt"), base])
            .output().await?;

        if !out.status.success() {
            return Err(anyhow!("git worktree add failed: {}", String::from_utf8_lossy(&out.stderr)));
        }
        info!(workspace_id=%workspace_id, branch=%branch, "worktree created");
        Ok(BuildWorkspace { workspace_id, base_repo: self.repo_root.clone(), worktree_path, branch: branch.to_string(), created_at: Utc::now() })
    }

    pub async fn cargo_check(&self, workspace: &BuildWorkspace) -> Result<BuildResult> {
        let out = Command::new("cargo").args(["check", "--workspace"])
            .current_dir(&workspace.worktree_path).output().await?;
        let sha = self.current_sha().await.ok();
        Ok(BuildResult {
            workspace_id: workspace.workspace_id.clone(), branch: workspace.branch.clone(),
            artifact_paths: vec![], cargo_check_passed: out.status.success(), test_passed: false,
            stdout: String::from_utf8_lossy(&out.stdout).to_string(),
            stderr: String::from_utf8_lossy(&out.stderr).to_string(),
            exit_code: out.status.code().unwrap_or(-1), built_at: Utc::now(), source_sha: sha,
        })
    }

    pub async fn cargo_build(&self, workspace: &BuildWorkspace) -> Result<BuildResult> {
        let out = Command::new("cargo").args(["build", "--release", "--bin", "shenko"])
            .current_dir(&workspace.worktree_path).output().await?;
        let sha = self.current_sha().await.ok();
        let artifact = workspace.worktree_path.join("target/release/shenko");
        Ok(BuildResult {
            workspace_id: workspace.workspace_id.clone(), branch: workspace.branch.clone(),
            artifact_paths: if artifact.exists() { vec![artifact] } else { vec![] },
            cargo_check_passed: out.status.success(), test_passed: false,
            stdout: String::from_utf8_lossy(&out.stdout).to_string(),
            stderr: String::from_utf8_lossy(&out.stderr).to_string(),
            exit_code: out.status.code().unwrap_or(-1), built_at: Utc::now(), source_sha: sha,
        })
    }

    pub async fn cargo_test(&self, workspace: &BuildWorkspace, package: Option<&str>) -> Result<BuildResult> {
        let mut args = vec!["test"];
        if let Some(pkg) = package { args.extend(["-p", pkg]); }
        let out = Command::new("cargo").args(&args)
            .current_dir(&workspace.worktree_path).output().await?;
        let sha = self.current_sha().await.ok();
        Ok(BuildResult {
            workspace_id: workspace.workspace_id.clone(), branch: workspace.branch.clone(),
            artifact_paths: vec![], cargo_check_passed: out.status.success(),
            test_passed: out.status.success(),
            stdout: String::from_utf8_lossy(&out.stdout).to_string(),
            stderr: String::from_utf8_lossy(&out.stderr).to_string(),
            exit_code: out.status.code().unwrap_or(-1), built_at: Utc::now(), source_sha: sha,
        })
    }

    pub async fn remove_worktree(&self, workspace: &BuildWorkspace) -> Result<()> {
        let out = Command::new("git")
            .args(["-C", self.repo_root.to_str().unwrap_or("."),
                "worktree", "remove", "--force",
                workspace.worktree_path.to_str().unwrap_or("")])
            .output().await?;
        if !out.status.success() { warn!("worktree remove failed: {}", String::from_utf8_lossy(&out.stderr)); }
        info!(workspace_id=%workspace.workspace_id, "worktree removed");
        Ok(())
    }

    pub async fn apply_patch(&self, workspace: &BuildWorkspace, patch: &str) -> Result<bool> {
        use std::io::Write;
        let tmp = std::env::temp_dir().join(format!("{}.patch", workspace.workspace_id));
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(patch.as_bytes())?;
        drop(f);
        let out = Command::new("git").args(["apply", tmp.to_str().unwrap_or("")])
            .current_dir(&workspace.worktree_path).output().await?;
        let _ = std::fs::remove_file(&tmp);
        Ok(out.status.success())
    }
}
