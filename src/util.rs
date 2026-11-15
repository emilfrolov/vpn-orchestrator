//! Общие хелперы: проверка интерфейсов, ping, IP forwarding.

use anyhow::{Context, Result};
use tokio::process::Command;

pub async fn interface_is_up(name: &str) -> Result<()> {
    let output = crate::util::command_output(Command::new("ip").args(["link", "show", name]), None)
        .await
        .with_context(|| format!("ip link show {name}"))?;
    if !output.status.success() {
        anyhow::bail!("интерфейс {name} не найден");
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    if link_flags_are_up(&stdout) {
        Ok(())
    } else {
        anyhow::bail!("интерфейс {name} не в состоянии UP")
    }
}

/// Restore an existing configured interface without recreating it or taking ownership.
pub async fn restore_existing_interface(name: &str) -> Result<bool> {
    let output =
        crate::util::command_output(Command::new("ip").args(["link", "show", name]), None).await?;
    if !output.status.success() {
        return Ok(false);
    }
    let output = crate::util::command_output(
        Command::new("ip").args(["link", "set", "dev", name, "up"]),
        None,
    )
    .await?;
    anyhow::ensure!(
        output.status.success(),
        "ip link set {name} up: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    interface_is_up(name).await?;
    Ok(true)
}

pub async fn ping_check(interface: &str, target: &str, timeout_secs: u64) -> Result<f64> {
    let timeout_str = timeout_secs.to_string();
    let output = crate::util::command_output(
        Command::new("ping")
            .kill_on_drop(true)
            .env("LC_ALL", "C")
            .args(["-I", interface, "-c", "1", "-W", &timeout_str, target]),
        None,
    )
    .await
    .with_context(|| format!("ping -I {interface} -c 1 -W {timeout_str} {target}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        anyhow::bail!(
            "ping {target} via {interface}: status={}, stderr={:?}, stdout={:?}",
            output.status,
            stderr.trim(),
            stdout.trim()
        );
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    for part in stdout.split_whitespace() {
        if let Some(rest) = part
            .strip_prefix("time=")
            .or_else(|| part.strip_prefix("time<"))
        {
            if let Ok(ms) = rest.trim_end_matches("ms").parse::<f64>() {
                return Ok(ms);
            }
        }
    }
    Ok(0.0)
}

#[cfg(target_os = "linux")]
pub fn enable_ip_forward() -> Result<()> {
    std::fs::write("/proc/sys/net/ipv4/ip_forward", "1")
        .context("включение net.ipv4.ip_forward")?;
    tracing::info!("net.ipv4.ip_forward = 1");
    Ok(())
}

#[cfg(not(target_os = "linux"))]
pub fn enable_ip_forward() -> Result<()> {
    anyhow::bail!("IP forwarding поддерживается только на Linux");
}

fn link_flags_are_up(output: &str) -> bool {
    output
        .split_once('<')
        .and_then(|(_, rest)| rest.split_once('>'))
        .is_some_and(|(flags, _)| flags.split(',').any(|flag| flag == "UP"))
}

#[cfg(test)]
mod tests {
    #[test]
    fn recognizes_administratively_up_tunnels() {
        assert!(super::link_flags_are_up(
            "5: wg0: <POINTOPOINT,NOARP,UP> mtu 1420 state UNKNOWN"
        ));
        assert!(!super::link_flags_are_up(
            "5: wg0: <POINTOPOINT,NOARP,LOWER_UP> state UNKNOWN"
        ));
    }
}

/// Network mutations must not outlive cancellation or hang shutdown indefinitely.
pub async fn command_output(
    command: &mut Command,
    input: Option<&[u8]>,
) -> Result<std::process::Output> {
    command_output_with_timeout(command, input, std::time::Duration::from_secs(30)).await
}

async fn command_output_with_timeout(
    command: &mut Command,
    input: Option<&[u8]>,
    timeout: std::time::Duration,
) -> Result<std::process::Output> {
    use std::process::Stdio;
    use tokio::io::AsyncWriteExt;
    command
        .env("LC_ALL", "C")
        .kill_on_drop(true)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        });
    #[cfg(unix)]
    command.process_group(0);
    let mut child = command.spawn().context("запуск сетевой команды")?;
    #[cfg(unix)]
    let mut group = ProcessGroup(child.id().context("missing child PID")? as i32);
    let operation = async move {
        if let Some(input) = input {
            let mut stdin = child.stdin.take().context("missing stdin")?;
            stdin.write_all(input).await?;
        }
        child
            .wait_with_output()
            .await
            .context("ожидание сетевой команды")
    };
    tokio::pin!(operation);
    match tokio::time::timeout(timeout, &mut operation).await {
        Ok(result) => {
            #[cfg(unix)]
            if result.is_ok() {
                group.0 = 0;
            }
            result
        }
        Err(_) => {
            #[cfg(unix)]
            group.kill();
            #[cfg(unix)]
            let _ = operation.await;
            anyhow::bail!("сетевая команда превысила таймаут {timeout:?}")
        }
    }
}

#[cfg(unix)]
struct ProcessGroup(i32);
#[cfg(unix)]
impl ProcessGroup {
    fn kill(&self) {
        // The child was spawned in its own process group, never the service's group.
        if self.0 > 0 {
            unsafe {
                libc::kill(-self.0, libc::SIGKILL);
            }
        }
    }
}
#[cfg(unix)]
impl Drop for ProcessGroup {
    fn drop(&mut self) {
        self.kill();
    }
}

pub fn cleanup_result(results: Vec<Result<()>>) -> Result<()> {
    let errors: Vec<_> = results
        .into_iter()
        .filter_map(Result::err)
        .map(|e| format!("{e:#}"))
        .collect();
    anyhow::ensure!(errors.is_empty(), "{}", errors.join("; "));
    Ok(())
}

#[cfg(all(test, unix))]
mod process_tests {
    use super::*;
    use std::time::Duration;
    #[tokio::test]
    async fn timeout_and_cancellation_stop_descendants() {
        for cancel in [false, true] {
            let path =
                std::env::temp_dir().join(format!("vpn-child-{}-{cancel}", std::process::id()));
            let _ = std::fs::remove_file(&path);
            let child_path = path.clone();
            let task = tokio::spawn(async move {
                command_output_with_timeout(
                    Command::new("sh")
                        .args(["-c", "(sleep 1; printf leaked > \"$1\") & wait", "sh"])
                        .arg(child_path),
                    None,
                    if cancel {
                        Duration::from_secs(30)
                    } else {
                        Duration::from_millis(100)
                    },
                )
                .await
            });
            if cancel {
                tokio::time::sleep(Duration::from_millis(100)).await;
                task.abort();
                assert!(task.await.unwrap_err().is_cancelled());
            } else {
                assert!(task
                    .await
                    .unwrap()
                    .unwrap_err()
                    .to_string()
                    .contains("таймаут"));
            }
            tokio::time::sleep(Duration::from_millis(1100)).await;
            assert!(!path.exists(), "descendant survived: {}", path.display());
        }
    }
}
