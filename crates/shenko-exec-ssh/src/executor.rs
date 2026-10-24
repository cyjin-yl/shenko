//! 宿主 OpenSSH 执行器；不是 WASM SSH，也没有使用 russh。
//! 外部效果不提供自动重放或远端已停止的虚假保证。SPEC §11、§36、§41。
use anyhow::{Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::process::Stdio;
use tokio::io::AsyncReadExt;

/// 目标与显式凭据选择；主机身份始终由 OpenSSH 严格校验。
#[derive(Clone, Serialize, Deserialize)]
pub struct SshTarget {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub key_path: Option<PathBuf>,
    /// 仅为旧配置兼容保留；不支持密码认证，执行前明确拒绝。
    #[serde(default, skip_serializing)]
    pub password: Option<String>,
    /// 不指定时使用 OpenSSH 默认 known_hosts；指定时只使用此信任文件。
    #[serde(default)]
    pub known_hosts_path: Option<PathBuf>,
    /// 只有明确启用时，才允许使用当前环境已授权的 SSH agent。
    #[serde(default)]
    pub use_agent: bool,
}

impl std::fmt::Debug for SshTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SshTarget")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("user", &self.user)
            .field("key_path", &self.key_path)
            .field("password", &self.password.as_ref().map(|_| "[REDACTED]"))
            .field("known_hosts_path", &self.known_hosts_path)
            .field("use_agent", &self.use_agent)
            .finish()
    }
}

impl Default for SshTarget {
    fn default() -> Self {
        Self {
            host: "localhost".into(),
            port: 22,
            user: "root".into(),
            key_path: None,
            password: None,
            known_hosts_path: None,
            use_agent: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecOutcome {
    /// 观察到命令退出；不等于应用后置条件通过或没有副作用。
    Exited,
    /// 仅用于本机路径；已请求终止并等待直接子进程回收。
    TimedOut,
    /// 远端结果或输出证据不明，不得据此自动重发。
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecUncertainty {
    Timeout,
    /// OpenSSH 的 255 同时可能是传输错误和远端 exit 255，不能区分。
    SshExit255,
    TransportTerminated,
    OutputUnavailable,
}

/// 调用方应按 execution_id 保存、检索和追加核验证据；本 crate 不冒充持久账本。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecDiagnostic {
    pub reason: ExecUncertainty,
    pub local_process_reaped: bool,
    pub reconciliation: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecResult {
    pub stdout: String,
    pub stderr: String,
    /// SSH 进程状态；unknown 时不应解释成远端命令的确定退出码。
    pub exit_code: i32,
    pub timed_out: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_id: Option<uuid::Uuid>,
    /// 缺省兼容旧的已记录结果，不把旧数据伪造成新证据。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<ExecOutcome>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diagnostic: Option<ExecDiagnostic>,
}

impl ExecResult {
    pub fn success(&self) -> bool {
        self.exit_code == 0
            && !self.timed_out
            && matches!(self.outcome, None | Some(ExecOutcome::Exited))
    }

    pub(crate) fn require_success(&self) -> Result<()> {
        if !self.success() {
            return Err(ExecFailure {
                result: self.clone(),
            }
            .into());
        }
        Ok(())
    }
}

/// 读取型 seat 接口的可下转错误，保留完整 unknown 证据而非普通失败文本。
#[derive(Debug)]
pub struct ExecFailure {
    pub result: ExecResult,
}

impl std::fmt::Display for ExecFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "执行未确认成功：execution_id={:?}, outcome={:?}, exit_code={}",
            self.result.execution_id, self.result.outcome, self.result.exit_code
        )
    }
}
impl std::error::Error for ExecFailure {}

/// 每次启动独立 OpenSSH，不读取用户 ssh_config，不复用外部 ControlMaster。
/// 调用方负责先提交 intent/armed attempt，并保存结果或取消后的 unknown。
pub struct SshExecutor {
    pub target: SshTarget,
    force_ssh: bool,
    ssh_program: PathBuf,
}

impl SshExecutor {
    /// 兼容便捷路径：localhost/127.0.0.1 用本机 sh，不进行 SSH 身份认证。
    pub fn new(target: SshTarget) -> Self {
        Self {
            target,
            force_ssh: false,
            ssh_program: "ssh".into(),
        }
    }

    /// 强制真正的 SSH，含回环地址；TmuxSeat::remote 使用此路径。
    pub fn over_ssh(target: SshTarget) -> Self {
        Self {
            force_ssh: true,
            ..Self::new(target)
        }
    }

    pub async fn run_command(&self, cmd: &str) -> Result<ExecResult> {
        self.run(cmd, None).await
    }

    pub async fn run_with_timeout(&self, cmd: &str, timeout_secs: u64) -> Result<ExecResult> {
        self.run(cmd, Some(std::time::Duration::from_secs(timeout_secs)))
            .await
    }

    fn command(&self, cmd: &str, remote: bool) -> Result<tokio::process::Command> {
        if self.target.password.is_some() {
            bail!("不支持 SSH 密码认证；请显式选择私钥或已授权 agent");
        }
        if !remote {
            let mut command = tokio::process::Command::new("sh");
            command.arg("-c").arg(cmd);
            return Ok(command);
        }
        // 禁止选项/目标混淆，不让目标或用户名成为本机 shell 文本。
        if self.target.host.is_empty()
            || self.target.host.starts_with('-')
            || !self
                .target
                .host
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b".-_:[]%".contains(&c))
            || self.target.user.is_empty()
            || self.target.user.starts_with('-')
            || !self
                .target
                .user
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c))
            || self.target.port == 0
        {
            bail!("SSH 目标、用户名或端口无效");
        }
        if self.target.key_path.is_none() && !self.target.use_agent {
            bail!("SSH 需要显式私钥路径或 use_agent=true");
        }
        if self.target.use_agent && std::env::var_os("SSH_AUTH_SOCK").is_none() {
            bail!("已选择 SSH agent，但当前环境未提供 SSH_AUTH_SOCK");
        }
        let mut command = tokio::process::Command::new(&self.ssh_program);
        // -F none 禁用用户和系统配置，避免隐式 ProxyCommand/LocalCommand 等本机执行。
        command.args(["-F", "none", "-T"]);
        for option in [
            "BatchMode=yes",
            "StrictHostKeyChecking=yes",
            "UpdateHostKeys=no",
            "PasswordAuthentication=no",
            "KbdInteractiveAuthentication=no",
            "PreferredAuthentications=publickey",
            "ForwardAgent=no",
            "ForwardX11=no",
            "ClearAllForwardings=yes",
            "ControlMaster=no",
            "ControlPath=none",
            "PermitLocalCommand=no",
            "ProxyCommand=none",
            "ProxyJump=none",
        ] {
            command.arg("-o").arg(option);
        }
        if !self.target.use_agent {
            command.args(["-o", "IdentityAgent=none", "-o", "IdentitiesOnly=yes"]);
            command.env_remove("SSH_AUTH_SOCK");
        }
        // 即使 -i 文件不存在，也禁止 OpenSSH 回退读取默认私钥。
        command.args(["-o", "IdentityFile=none"]);
        if let Some(key) = &self.target.key_path {
            command.arg("-i").arg(key);
        }
        if let Some(path) = &self.target.known_hosts_path {
            // ssh_config 的值需要引号；拒绝会被其二次展开的路径，不改变信任范围。
            let path = path
                .to_str()
                .ok_or_else(|| anyhow!("known_hosts 路径必须为 UTF-8"))?;
            if path.contains(['\n', '\r', '"', '\\', '%', '$'])
                || !std::path::Path::new(path).is_absolute()
            {
                bail!("known_hosts 需要不含展开符的绝对路径");
            }
            command
                .arg("-o")
                .arg(format!("UserKnownHostsFile=\"{path}\""));
            command.args(["-o", "GlobalKnownHostsFile=none"]);
        }
        // 单独 argv 发送的命令由远端 shell 解释，绝不经本机 sh 拼装 ssh。
        command
            .arg("-p")
            .arg(self.target.port.to_string())
            .arg("-l")
            .arg(&self.target.user)
            .arg("--")
            .arg(&self.target.host)
            .arg(cmd);
        Ok(command)
    }

    async fn run(&self, cmd: &str, timeout: Option<std::time::Duration>) -> Result<ExecResult> {
        let remote =
            self.force_ssh || !matches!(self.target.host.as_str(), "localhost" | "127.0.0.1");
        let mut command = self.command(cmd, remote)?;
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.as_std_mut().process_group(0);
        }
        let execution_id = uuid::Uuid::new_v4();
        // spawn 错误发生在派发之前；派发后的不确定性必须返回结构化结果。
        let mut child = command.spawn()?;
        let mut group = ProcessGroupGuard {
            pid: child.id(),
            armed: true,
        };
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut stdout_pipe = child.stdout.take().expect("已配置 stdout pipe");
        let mut stderr_pipe = child.stderr.take().expect("已配置 stderr pipe");
        let collect = async {
            tokio::join!(
                child.wait(),
                stdout_pipe.read_to_end(&mut stdout),
                stderr_pipe.read_to_end(&mut stderr)
            )
        };
        let completion = match timeout {
            Some(duration) => tokio::time::timeout(duration, collect).await.ok(),
            None => Some(collect.await),
        };
        let (exit_code, timed_out, outcome, reason, reaped) = match completion {
            None => {
                group.terminate();
                let _ = child.start_kill();
                let reaped = child.wait().await.is_ok();
                (
                    -1,
                    true,
                    if remote || !reaped {
                        ExecOutcome::Unknown
                    } else {
                        ExecOutcome::TimedOut
                    },
                    Some(ExecUncertainty::Timeout),
                    reaped,
                )
            }
            Some((status, out_read, err_read)) => match status {
                Ok(status) => {
                    let code = status.code().unwrap_or(-1);
                    let reason = if out_read.is_err() || err_read.is_err() {
                        Some(ExecUncertainty::OutputUnavailable)
                    } else if remote && code == 255 {
                        Some(ExecUncertainty::SshExit255)
                    } else if remote && status.code().is_none() {
                        Some(ExecUncertainty::TransportTerminated)
                    } else {
                        None
                    };
                    (
                        code,
                        false,
                        if reason.is_some() {
                            ExecOutcome::Unknown
                        } else {
                            ExecOutcome::Exited
                        },
                        reason,
                        true,
                    )
                }
                Err(_) => {
                    group.terminate();
                    let _ = child.start_kill();
                    let reaped = child.wait().await.is_ok();
                    (
                        -1,
                        false,
                        ExecOutcome::Unknown,
                        Some(ExecUncertainty::TransportTerminated),
                        reaped,
                    )
                }
            },
        };
        group.armed = !reaped;
        Ok(ExecResult {
            stdout: String::from_utf8_lossy(&stdout).into_owned(),
            stderr: String::from_utf8_lossy(&stderr).into_owned(),
            exit_code, timed_out, execution_id: Some(execution_id), outcome: Some(outcome),
            diagnostic: reason.map(|reason| ExecDiagnostic {
                reason, local_process_reaped: reaped,
                reconciliation: if remote {
                    "仅停止/回收本机 SSH 不证明远端进程已停；按原任务/seat/目标核验外部状态，不自动重发。此 execution_id 不是远端 receipt handle。".into()
                } else {
                    "直接子进程回收状态见 local_process_reaped；既有文件/网络效果不回滚，逃逸进程需另行核验。".into()
                },
            }),
        })
    }
}

/// Unix 下对独立进程组兜底；future 被 drop 时先停止组，再由 kill_on_drop/Tokio 回收直接子进程。
/// 非 Unix 仅保证 kill_on_drop 和显式超时 wait，不声称能终止整个进程树。
struct ProcessGroupGuard {
    pid: Option<u32>,
    armed: bool,
}

impl ProcessGroupGuard {
    fn terminate(&self) {
        #[cfg(unix)]
        if let Some(pid) = self.pid.and_then(|pid| i32::try_from(pid).ok()) {
            unsafe extern "C" {
                fn kill(pid: i32, signal: i32) -> i32;
            }
            // SAFETY：子进程以自身 PID 建立独立组；负 PID 只定位该组，9 是 Unix SIGKILL。
            unsafe {
                kill(-pid, 9);
            }
        }
    }
}

impl Drop for ProcessGroupGuard {
    fn drop(&mut self) {
        if self.armed {
            self.terminate();
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    struct Scratch(PathBuf);
    impl Scratch {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("shenko-ssh-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    // 模拟 SSH 边界：任意命令只占一个 argv；严格信任参数和传输未知态必须保留。
    #[tokio::test]
    async fn ssh_argv_and_transport_unknown() {
        let scratch = Scratch::new();
        let script = scratch.0.join("ssh-fixture");
        let args_file = scratch.0.join("args");
        std::fs::write(&script, format!(
            "#!/bin/sh\nprintf '%s\\0' \"$@\" > '{}'\nprintf 'partial output'\nprintf 'connection lost\\n' >&2\nexit 255\n",
            args_file.display()
        )).unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut target = SshTarget {
            host: "fixture.invalid".into(),
            port: 2222,
            user: "worker".into(),
            key_path: Some(scratch.0.join("key with spaces")),
            known_hosts_path: Some(scratch.0.join("known hosts")),
            password: Some("fixture-secret-not-a-real-password".into()),
            use_agent: false,
        };
        assert!(!format!("{target:?}").contains("fixture-secret"));
        assert!(
            !serde_json::to_string(&target)
                .unwrap()
                .contains("fixture-secret")
        );
        assert!(
            SshExecutor::over_ssh(target.clone())
                .command("ignored", true)
                .is_err()
        );
        target.password = None;
        let executor = SshExecutor {
            target,
            force_ssh: true,
            ssh_program: script,
        };
        let marker = scratch.0.join("must-not-exist");
        let cmd = format!(
            "-oProxyCommand=touch {}; printf '%s' \"$HOME\"; 'quoted'",
            marker.display()
        );
        let result = executor.run_with_timeout(&cmd, 5).await.unwrap();
        let bytes = std::fs::read(&args_file).unwrap();
        let args: Vec<&str> = bytes
            .split(|b| *b == 0)
            .filter(|p| !p.is_empty())
            .map(|p| std::str::from_utf8(p).unwrap())
            .collect();
        assert_eq!(&args[..3], &["-F", "none", "-T"]);
        assert!(
            args.windows(2)
                .any(|w| w == ["-o", "StrictHostKeyChecking=yes"])
        );
        assert!(args.windows(2).any(|w| w == ["-o", "BatchMode=yes"]));
        assert!(args.windows(2).any(|w| w == ["-o", "IdentityAgent=none"]));
        assert!(args.windows(2).any(|w| w == ["-o", "ControlPath=none"]));
        let known_hosts = format!(
            "UserKnownHostsFile=\"{}\"",
            executor.target.known_hosts_path.as_ref().unwrap().display()
        );
        assert!(args.windows(2).any(|w| w == ["-o", known_hosts.as_str()]));
        assert_eq!(
            &args[args.len() - 7..],
            &["-p", "2222", "-l", "worker", "--", "fixture.invalid", &cmd]
        );
        assert!(args.windows(2).any(|w| w == ["-o", "IdentityFile=none"]));
        assert!(!marker.exists());
        assert_eq!(result.stdout, "partial output");
        assert_eq!(result.outcome, Some(ExecOutcome::Unknown));
        assert_eq!(result.exit_code, 255);
        assert!(!result.success());
        assert!(result.execution_id.is_some());
        let error = result.require_success().unwrap_err();
        assert_eq!(
            error.downcast_ref::<ExecFailure>().unwrap().result.outcome,
            Some(ExecOutcome::Unknown)
        );
        let diagnostic = result.diagnostic.unwrap();
        assert_eq!(diagnostic.reason, ExecUncertainty::SshExit255);
        assert!(diagnostic.local_process_reaped);
        assert!(diagnostic.reconciliation.contains("不自动重发"));
        let old: ExecResult =
            serde_json::from_str(r#"{"stdout":"","stderr":"","exit_code":0,"timed_out":false}"#)
                .unwrap();
        assert_eq!(old.outcome, None);
        assert!(old.success());
    }

    // /proc 的消失同时证明直接子进程已经 wait 回收，而非只发送过 kill。
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn local_timeout_reaps_child() {
        let executor = SshExecutor::new(SshTarget::default());
        let result = executor
            .run(
                "printf '%s\\n' \"$$\"; exec sleep 60",
                Some(std::time::Duration::from_millis(100)),
            )
            .await
            .unwrap();
        let pid: u32 = result.stdout.trim().parse().unwrap();
        assert!(result.timed_out);
        assert_eq!(result.outcome, Some(ExecOutcome::TimedOut));
        assert!(!result.success());
        assert!(result.diagnostic.unwrap().local_process_reaped);
        assert!(!PathBuf::from(format!("/proc/{pid}")).exists());
    }
}
