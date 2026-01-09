mod netns;
mod process;

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;
use std::time::SystemTime;
use std::net::Ipv4Addr;

use anyhow::{anyhow, Context, Result};
use clap::Parser;
use nix::unistd::Pid;
use serde::Deserialize;
use ipnet::IpNet;
use tokio::runtime::{Builder, Runtime};

const DEFAULT_ADDRESS: Ipv4Addr = Ipv4Addr::new(10, 200, 1, 1);

#[derive(Parser)]
#[command(name = "netrun")]
#[command(about = "Run commands in an isolated network namespace")]
struct Cli {
    /// Explicitly set a YAML config file for commands to run.
    #[arg(short, long, default_value = "run.yaml")]
    config: PathBuf,

    /// Explicitly set a network for the namespace, e.g: 10.200.1.1/24.
    /// The network in the isolated namespace is this address with last part increased by 1.
    #[arg(short, long, default_value = "")]
    network: String,

    /// The command to to run in the namespace.
    /// This command will replace commands in YAML config.
    #[arg(allow_hyphen_values = true)]
    args: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct Config {
    #[serde(default)]
    network: NetworkConfig,
    commands: Vec<CommandConfig>,

    #[serde(skip)]
    host_network: ParsedNetwork,

    #[serde(skip)]
    ns_network: ParsedNetwork,
}

impl Config {
    fn normalize_network(&mut self, network: &str) -> Result<()> {
        if !network.is_empty() {
            let (ip, prefix) = parse_ip_prefix(network)?;
            self.host_network = ParsedNetwork{ ip, prefix };
            self.ns_network = Self::gen_network_from(&self.host_network);

            return Ok(())
        }

        if !self.network.host_ip.is_empty() {
            let (ip, prefix) = parse_ip_prefix(&self.network.host_ip)?;
            self.host_network = ParsedNetwork{ ip, prefix };
        }

        if self.network.ns_ip.is_empty() {
            self.ns_network = Self::gen_network_from(&self.host_network);
        } else {
            let (ip, prefix) = parse_ip_prefix(&self.network.ns_ip)?;
            self.ns_network = ParsedNetwork{ ip, prefix };
        }

        Ok(())
    }

    fn gen_network_from(network: &ParsedNetwork) -> ParsedNetwork {
        let mut parts = network.ip.octets();
        parts[3] = parts[3].wrapping_add(1);

        let ip = Ipv4Addr::from(parts);

        return ParsedNetwork{ ip, prefix: network.prefix };
    }
}

#[derive(Debug)]
struct ParsedNetwork {
    ip: Ipv4Addr,
    prefix: u8,
}

impl Default for ParsedNetwork {
    fn default() -> Self {
        Self {
            ip: DEFAULT_ADDRESS,
            prefix: 24,
        }
    }
}

#[derive(Debug, Deserialize, Clone, Default)]
struct NetworkConfig {
    #[serde(default)]
    ns_ip: String,
    #[serde(default)]
    host_ip: String,
}

#[derive(Debug, Deserialize)]
struct CommandConfig {
    name: String,
    run: String,
}

fn generate_ns_name() -> String {
    let ts = SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("nr-{:x}", (ts ^ (std::process::id() as u128)) & 0xFFFFFFFF)
}

fn parse_ip_prefix(s: &str) -> Result<(Ipv4Addr, u8)> {
    match s.parse::<IpNet>().context("Invalid IP prefix")? {
        IpNet::V4(net) => Ok((net.addr(), net.prefix_len())),
        IpNet::V6(_) => Err(anyhow!("IPv6 prefixes are not supported")),
    }
}

fn run(rt: &Runtime, ctx: &netns::NetNS, config: Config) -> Result<()> {
    if !nix::unistd::Uid::effective().is_root() {
        return Err(anyhow!("Must be run as root"));
    }

    let veth_config = netns::VethConfig {
        host_ip: config.host_network.ip,
        host_prefix: config.host_network.prefix,
        ns_ip: config.ns_network.ip,
        ns_prefix: config.ns_network.prefix,
    };

    // Signal handler: count signals for graceful/force shutdown
    let signal_count = Arc::new(AtomicU8::new(0));
    let signal_count_clone = signal_count.clone();
    ctrlc::set_handler(move || {
        signal_count_clone.fetch_add(1, Ordering::Relaxed);
    })?;

    rt.block_on(async {
        ctx.create_veth_pair(&veth_config).await
    })?;

    ctx.setup_networking(&veth_config)?;

    // Spawn commands
    let mut child_pids: Vec<Pid> = Vec::new();
    for cmd in &config.commands {
        let cmd_run = cmd.run.clone();
        let ns_path = ctx.path.clone();

        let result = process::spawn(move || {
            ns_path.set()?;

            let mut cmd = std::process::Command::new("sh");
            cmd.args(["-c", &cmd_run]);
            Ok(cmd)
        });

        match result {
            Ok(pid) => child_pids.push(pid),
            Err(e) => eprintln!("Failed to spawn '{}': {}", cmd.name, e),
        }
    }

    if child_pids.is_empty() {
        eprintln!("No commands started");
    } else {
        process::supervise_children(&mut child_pids, &signal_count);
    }

    Ok(())
}

fn load_config(path: &std::path::Path) -> Result<Config> {
    let content = fs::read_to_string(path)
        .with_context(|| format!("Failed to read config: {:?}", path))?;
    let config: Config = serde_yaml::from_str(&content)
        .with_context(|| format!("Failed to parse config: {:?}", path))?;
    if config.commands.is_empty() {
        return Err(anyhow!("Config must contain at least one command"));
    }
    Ok(config)
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let mut config = load_config(&cli.config)?;

    if !cli.args.is_empty() {
        let command = cli.args.join(" ");
        config.commands = vec![CommandConfig{ name: "".to_string(), run: command }];
    }

    config.normalize_network(&cli.network)?;

    let rt = Builder::new_current_thread()
        .enable_all()
        .build()?;

    let ns = netns::NsPath::new(generate_ns_name());

    let ctx = netns::NetNS::new(&rt, &ns)?;
    ctx.create_namespace(&rt)?;

    let result = run(&rt, &ctx, config);

    rt.block_on(async {
        if let Err(e) = ctx.delete_host_veth().await {
            eprintln!("Warning: clean namespace: {}", e);
        }

        if let Err(e) = ctx.delete_namespace().await {
            eprintln!("Warning: clean namespace: {}", e);
        }
    });

    return result
}
