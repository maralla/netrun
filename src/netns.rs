use std::fs::{File};
use std::net::{Ipv4Addr, IpAddr};
use std::os::fd::{AsFd, AsRawFd};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use futures::TryStreamExt;
use nix::sched::{setns, CloneFlags};
use nix::unistd::{fork, ForkResult};
use nix::sys::wait::{waitpid, WaitStatus};
use rtnetlink::{new_connection, Handle, LinkHandle, NetworkNamespace, NETNS_PATH};
use tokio::runtime::Runtime;

pub struct VethConfig {
    pub host_ip: Ipv4Addr,
    pub host_prefix: u8,
    pub ns_ip: Ipv4Addr,
    pub ns_prefix: u8,
}

fn veth_names(ns_name: &str) -> (String, String) {
    // Max interface name is 15 chars
    let max_len = 13;
    let name = if ns_name.len() > max_len { &ns_name[..max_len] } else { ns_name };
    (format!("h-{}", name), format!("n-{}", name))
}

#[derive(Clone)]
pub struct NsPath {
    name: String,
    path: PathBuf,

    pub veth: (String, String),
}

impl NsPath {
    pub fn new(name: String) -> NsPath {
        let ns_path = Path::new(NETNS_PATH).join(&name);

        NsPath{
            path: ns_path,
            veth: veth_names(&name),
            name: name,
        }
    }

    fn file(&self) -> Result<File> {
        Ok(File::open(&self.path).context("failed to open namespace")?)
    }

    pub fn set(&self) -> Result<()> {
        setns(self.file()?.as_fd(), CloneFlags::CLONE_NEWNET)
            .context("failed to enter network namespace")?;
        Ok(())
    }
}


pub struct NetNS<'a> {
    handle: Handle,
    pub path: &'a NsPath,
}

impl<'a> NetNS<'a> {
    pub fn new(rt: &Runtime, ns: &'a NsPath) -> Result<Self> {
        let handle: Result<Handle> = rt.block_on(async {
            let (connection, handle, _) = new_connection()?;
            tokio::spawn(connection);

            Ok(handle)
        });

        Ok(Self {
            handle: handle?,
            path: ns,
        })
    }

    pub fn create_namespace(&self, rt: &Runtime) -> Result<()> {
        let name = self.path.name.to_string();

        rt.block_on(async {
            NetworkNamespace::add(name).await.context("Failed to create network namespace")
        })
    }

    fn link(&self) -> LinkHandle {
        self.handle.link()
    }

    pub async fn delete_host_veth(&self) -> Result<()> {
        let (name, _) = &self.path.veth;

        if let Ok(idx) = self.link_index(name).await {
            self.link()
                .del(idx)
                .execute()
                .await
                .context(format!("failed to delete link: {}", name))?;
        }
        Ok(())
    }

    pub async fn delete_namespace(&self) -> Result<()> {
        let name = &self.path.name;

        NetworkNamespace::del(name.clone())
            .await
            .context(format!("failed to delete network namespace: {}", name))?;

        Ok(())
    }

    pub async fn create_veth_pair(&self, config: &VethConfig) -> Result<()> {
        let (veth_host, veth_ns) = &self.path.veth;

        self.link()
            .add()
            .veth(veth_host.clone(), veth_ns.clone())
            .execute()
            .await
            .context("Failed to create veth pair")?;

        let host_idx = self.link_index(veth_host).await?;
        let ns_idx = self.link_index(veth_ns).await?;

        self.link()
            .set(ns_idx)
            .setns_by_fd(self.path.file()?.as_raw_fd())
            .execute()
            .await
            .context("Failed to move veth to namespace")?;

        self.handle
            .address()
            .add(host_idx, IpAddr::V4(config.host_ip), config.host_prefix)
            .execute()
            .await
            .context("Failed to add IP to host veth")?;

        self.link()
            .set(host_idx)
            .up()
            .execute()
            .await
            .context("Failed to bring up host veth")?;

        Ok(())
    }

    async fn set_interface_address(&self, name: &str, ip: Ipv4Addr, prefix: u8) -> Result<()> {
        let index = self.link_index(name).await?;

        self.handle
            .address()
            .add(index, IpAddr::V4(ip), prefix)
            .execute()
            .await
            .context("failed to add IP")?;

        self.handle
            .link()
            .set(index)
            .up()
            .execute()
            .await
            .context("failed to bring up veth")?;

        let lo_index = self.link_index("lo").await?;
        self.handle.link().set(lo_index).up().execute().await.context("failed to setup lo")?;

        Ok(())
    }

    async fn link_index(&self, name: &str) -> Result<u32> {
        let mut links = self.handle.link().get().match_name(name.to_string()).execute();

        if let Some(link) = links.try_next().await? {
            Ok(link.header.index)
        } else {
            Err(anyhow!("interface {} not found", name))
        }
    }

    fn setup_ns_network(&self, veth_name: String, ip: Ipv4Addr, prefix: u8) -> Result<()> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("failed to create runtime")?;

        let ctx = NetNS::new(&rt, &self.path)?;

        return rt.block_on(async {
            ctx.set_interface_address(&veth_name, ip, prefix).await
        });
    }

    pub fn setup_networking(&self, config: &VethConfig) -> Result<()> {
        let (_, veth_ns) = &self.path.veth;

        let veth_name = veth_ns.clone();
        let ip = config.ns_ip;
        let prefix = config.ns_prefix;

        match unsafe { fork() }? {
            ForkResult::Parent { child } => {
                if let WaitStatus::Exited(_pid, status) = waitpid(child, None)? {
                    if status != 0 {
                        return Err(anyhow!("failed to setup namespace network"));
                    }
                }

                Ok(())
            }
            ForkResult::Child => {
                self.path.set()?;

                let mut code = 0;

                if let Err(e) = self.setup_ns_network(veth_name, ip, prefix) {
                    eprintln!("Error: setup namespace network: {}", e);
                    code = 1;
                }

                std::process::exit(code);
            }
        }
    }
}
