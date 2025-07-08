use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

const CONFIG_FILE_NAME: &str = "config.toml";

#[derive(Serialize, Deserialize, Debug)]
struct SshConfig {
    host: String,
    port: u16,
    user: String,
    password: Option<String>,
    private_key_path: Option<String>,
}

/// 检测 SSH 命令是否存在
fn detect_ssh_command() -> Result<String> {
    let ssh_commands = if cfg!(target_os = "windows") {
        vec!["ssh.exe", "ssh"]
    } else {
        vec!["ssh"]
    };

    for cmd in ssh_commands {
        // 尝试获取 SSH 版本来测试命令是否存在
        let result = Command::new(cmd).arg("-V").output();

        match result {
            Ok(output) => {
                if output.status.success() || !output.stderr.is_empty() {
                    // SSH -V 通常会将版本信息输出到 stderr
                    let version_info = String::from_utf8_lossy(&output.stderr);
                    if version_info.to_lowercase().contains("openssh") {
                        println!("找到 ssh 程序: {} ({})", cmd, version_info.trim());
                        return Ok(cmd.to_string());
                    }
                }
            }
            Err(_) => continue,
        }
    }

    Err(anyhow::anyhow!(
        "未找到可用的 SSH 命令。请确保已安装 OpenSSH 客户端。\n\
        Windows: 可以通过 '设置 > 应用 > 可选功能' 安装 OpenSSH 客户端\n\
        Linux: 使用包管理器安装 openssh-client"
    ))
}

fn load_or_create_config() -> Result<SshConfig> {
    let config_path = Path::new(CONFIG_FILE_NAME);

    if config_path.exists() {
        println!("找到配置文件 '{}'，正在加载...", CONFIG_FILE_NAME);
        let file_content = fs::read_to_string(config_path)
            .with_context(|| format!("无法读取配置文件 '{}'", CONFIG_FILE_NAME))?;

        let config: SshConfig = toml::from_str(&file_content).with_context(|| {
            format!(
                "解析配置文件 '{}' 失败，请检查其 TOML 格式是否正确",
                CONFIG_FILE_NAME
            )
        })?;

        Ok(config)
    } else {
        println!("未找到配置文件 '{}'。", CONFIG_FILE_NAME);

        let default_config = SshConfig {
            host: "192.168.1.100".to_string(),
            port: 22,
            user: "root".to_string(),
            password: Some("your_password".to_string()),
            private_key_path: Some("your_private_key_path".to_string()),
        };

        let toml_header = "# SSH 连接配置\n\
                           # host: 目标主机IP或域名\n\
                           # port: SSH端口\n\
                           # user: 登录用户名\n\
                           # password: 登录密码\n\
                           # private_key_path: 私钥路径（若设置则优先使用）\n";

        let toml_body =
            toml::to_string_pretty(&default_config).context("无法序列化默认配置到 TOML")?;

        let file_content = format!("{}{}", toml_header, toml_body);

        fs::write(config_path, file_content)
            .with_context(|| format!("无法创建默认配置文件 '{}'", CONFIG_FILE_NAME))?;

        println!("已创建了默认配置文件 '{}'。", CONFIG_FILE_NAME);

        Err(anyhow::anyhow!("请修改默认配置文件后重试。"))
    }
}

/// 构建 SSH 命令参数
fn build_ssh_command(ssh_cmd: &str, config: &SshConfig) -> Command {
    let mut cmd = Command::new(ssh_cmd);

    // 基本连接参数
    cmd.arg("-p").arg(config.port.to_string());

    // 指定私钥
    if let Some(key_path) = &config.private_key_path {
        if !key_path.is_empty() && Path::new(key_path).exists() {
            cmd.arg("-i").arg(key_path);
        }
    }

    // 如果有密码，使用 sshpass（如果可用）
    // 注意：这需要额外安装 sshpass，这里先不实现，建议使用密钥认证

    // 目标和命令
    let target = format!("{}@{}", config.user, config.host);
    cmd.arg(target);

    // 要执行的命令
    cmd.arg("ls -la ~");

    cmd
}

/// 执行 SSH 命令并处理密码输入
fn execute_ssh_command(mut cmd: Command, config: &SshConfig) -> Result<()> {
    println!("\n正在连接...");
    println!("执行 SSH 命令: {:?}", cmd);

    // 如果配置了密码，我们需要交互式处理
    if config.password.is_some() && !config.password.as_ref().unwrap().is_empty() {
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = cmd.spawn().context("启动 SSH 进程失败")?;

        // 处理密码输入
        if let Some(stdin) = child.stdin.as_mut() {
            if let Some(password) = &config.password {
                // 注意：这种方式可能不总是有效，因为 SSH 可能会直接从终端读取密码
                // 更好的方式是使用 sshpass 或配置密钥认证
                writeln!(stdin, "{}", password).context("写入密码失败")?;
            }
        }

        // 读取输出
        let output = child.wait_with_output().context("等待 SSH 进程结束失败")?;

        if output.status.success() {
            println!("命令执行成功！");
            println!("--- 输出 ---");
            println!("{}", String::from_utf8_lossy(&output.stdout));
            if !output.stderr.is_empty() {
                println!("--- 错误信息 ---");
                println!("{}", String::from_utf8_lossy(&output.stderr));
            }
        } else {
            println!("命令执行失败，退出码: {:?}", output.status.code());
            println!("--- 错误信息 ---");
            println!("{}", String::from_utf8_lossy(&output.stderr));
            return Err(anyhow::anyhow!("SSH 命令执行失败"));
        }
    } else {
        // 无密码模式，直接执行（继承终端的 stdin/stdout）
        let status = cmd.status().context("执行 SSH 命令失败")?;

        if status.success() {
            println!("命令执行成功！");
        } else {
            println!("命令执行失败，退出码: {:?}", status.code());
            return Err(anyhow::anyhow!("SSH 命令执行失败"));
        }
    }

    Ok(())
}

fn main() -> Result<()> {
    // 1. 检测 SSH 命令
    let ssh_cmd = detect_ssh_command()?;

    // 2. 加载配置
    let config = match load_or_create_config() {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("\n错误: {}", e);
            println!("\n按 Enter 键退出...");
            let mut buf = String::new();
            std::io::stdin().read_line(&mut buf)?;
            return Ok(());
        }
    };

    // 3. 构建 SSH 命令
    let ssh_command = build_ssh_command(&ssh_cmd, &config);

    // 4. 执行 SSH 命令
    match execute_ssh_command(ssh_command, &config) {
        Ok(()) => {
            println!("\n操作完成！");
        }
        Err(e) => {
            eprintln!("\n执行失败: {}", e);
        }
    }

    println!("\n按 Enter 键退出...");
    let mut buf = String::new();
    std::io::stdin().read_line(&mut buf)?;

    Ok(())
}
