use ipnet::IpNet;
use netavark::commands::setup::Setup;
use netavark::network::types::Network;
use netavark::network::types::NetworkOptions;
use netavark::network::types::PerNetworkOptions;
use netavark::network::types::Subnet;
use std::collections::HashMap;
use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::net::IpAddr;
use std::net::Ipv4Addr;
use std::path::Path;
use std::path::PathBuf;
use std::str::FromStr;
use std::thread;
use std::time::Duration;
use sysinfo::Pid;
use sysinfo::System;

use crate::commands::compose::spec::NetworkDriver::Bridge;
use crate::commands::compose::spec::NetworkDriver::Host;
use crate::commands::compose::spec::NetworkDriver::Overlay;
use libruntime::dns;
use libruntime::dns::PID_FILE_PATH;

use cni_plugin::ip_range::IpRange;
use hickory_proto::rr::LowerName;
use ipnetwork::IpNetwork;
use ipnetwork::Ipv4Network;
use libipam::config::IPAMConfig;
use libipam::range_set::RangeSet;
use nix::unistd::Uid;
use tokio::io::AsyncBufReadExt;
use tokio::io::AsyncWriteExt;
use tokio::io::BufReader;
use tokio::net::UnixStream;

use crate::commands::compose::spec::ComposeSpec;
use crate::commands::compose::spec::NetworkSpec;
use crate::commands::compose::spec::ServiceSpec;
use crate::commands::container::ContainerRunner;
use anyhow::Result;
use anyhow::anyhow;
use libruntime::dns::DNS_SOCKET_PATH;
use serde::{Deserialize, Serialize};

pub const CNI_VERSION: &str = "1.0.0";
pub const STD_CONF_PATH: &str = "/etc/cni/net.d";

pub const BRIDGE_PLUGIN_NAME: &str = "libbridge";
pub const BRIDGE_CONF: &str = "rkl-standalone-bridge.conf";

fn default_netavark_config_dir(rootless: bool) -> OsString {
    if let Some(v) = std::env::var_os("NETAVARK_CONFIG") {
        return v;
    }

    if rootless {
        let uid = Uid::effective().as_raw();
        return OsString::from(format!("/run/user/{uid}/containers/networks"));
    }

    OsString::from("/run/containers/networks")
}

fn default_aardvark_bin() -> OsString {
    // Allow overrides (useful in dev environments)
    if let Some(v) = std::env::var_os("AARDVARK_DNS_BIN") {
        return v;
    }
    if let Some(v) = std::env::var_os("AARDVARK_BIN") {
        return v;
    }

    // Common locations on many distros (Podman)
    let candidates = ["/usr/libexec/podman/aardvark-dns", "/usr/bin/aardvark-dns"];
    for c in candidates {
        if Path::new(c).exists() {
            return OsString::from(c);
        }
    }

    // Fall back to the most common default; netavark will disable DNS if missing.
    OsString::from("/usr/libexec/podman/aardvark-dns")
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CliNetworkConfig {
    /// default is 1.0.0
    #[serde(default)]
    pub cni_version: String,
    /// the `type` in JSON
    #[serde(rename = "type")]
    pub plugin: String,
    /// network's name
    #[serde(default)]
    pub name: String,
    /// bridge interface' s name (default cni0）
    #[serde(default)]
    pub bridge: String,
    /// whether this network should be set the container's default gateway
    #[serde(default)]
    pub is_default_gateway: Option<bool>,
    /// whether the bridge should at as a gateway
    #[serde(default)]
    pub is_gateway: Option<bool>,
    /// Maximum Transmission Unit (MTU) to set on the bridge interface
    #[serde(default)]
    pub mtu: Option<u32>,
    /// Enable Mac address spoofing check
    #[serde(default)]
    pub mac_spoof_check: Option<bool>,
    /// IPAM type（like host-local, static, etc.）
    #[serde(default)]
    pub ipam: Option<IPAMConfig>,
    /// enable hairpin mod
    #[serde(default)]
    pub hairpin_mode: Option<bool>,
    /// VLAN ID
    #[serde(default)]
    pub vlan: Option<u16>,
    /// VLAN Trunk
    #[serde(default)]
    pub vlan_trunk: Option<Vec<u16>>,
}

impl CliNetworkConfig {
    pub fn from_name_bridge(network_name: &str, bridge: &str) -> Self {
        Self {
            bridge: bridge.to_string(),
            name: network_name.to_string(),
            ..Default::default()
        }
    }

    /// Due to compose will need a unique subnet
    /// this function need two extra parameter
    /// - subnet_addr
    /// - gateway_addr
    ///
    /// by default the subnet prefix is 16
    pub fn from_subnet_gateway(
        network_name: &str,
        bridge: &str,
        subnet_addr: Ipv4Addr,
        getway_addr: Ipv4Addr,
    ) -> Self {
        let ip_range = IpRange {
            subnet: IpNetwork::V4(Ipv4Network::new(subnet_addr, 16).unwrap()),
            range_start: None,
            range_end: None,
            gateway: Some(IpAddr::V4(getway_addr)),
        };

        let set: RangeSet = vec![ip_range];

        Self {
            bridge: bridge.to_string(),
            name: network_name.to_string(),
            ipam: Some(IPAMConfig {
                type_field: "libipam".to_string(),
                name: None,
                routes: None,
                resolv_conf: None,
                data_dir: None,
                ranges: vec![set],
                ip_args: vec![],
            }),
            ..Default::default()
        }
    }
}

impl Default for CliNetworkConfig {
    fn default() -> Self {
        // Default subnet-addr for rkl container management
        // 172.17.0.0/16
        let subnet_addr = Ipv4Addr::new(172, 17, 0, 0);
        let getway_addr = Ipv4Addr::new(172, 17, 0, 1);

        let ip_range = IpRange {
            subnet: ipnetwork::IpNetwork::V4(Ipv4Network::new(subnet_addr, 16).unwrap()),
            range_start: None,
            range_end: None,
            gateway: Some(IpAddr::V4(getway_addr)),
        };

        let set: RangeSet = vec![ip_range];

        Self {
            cni_version: String::from(CNI_VERSION),
            plugin: String::from(BRIDGE_PLUGIN_NAME),
            name: Default::default(),
            bridge: Default::default(),
            is_default_gateway: Default::default(),
            is_gateway: Some(true),
            mtu: Some(1500),
            mac_spoof_check: Default::default(),
            hairpin_mode: Default::default(),
            vlan: Default::default(),
            vlan_trunk: Default::default(),
            ipam: Some(IPAMConfig {
                type_field: "libipam".to_string(),
                name: None,
                routes: None,
                resolv_conf: None,
                data_dir: None,
                ranges: vec![set],
                ip_args: vec![],
            }),
        }
    }
}

pub struct NetworkManager {
    map: HashMap<String, NetworkSpec>,
    /// key: network_name; value: bridge interface
    network_interface: HashMap<String, String>,
    /// key: service_name value: networks
    service_mapping: HashMap<String, Vec<String>>,
    /// key: network_name value: (srv_name, service_spec)
    network_service: HashMap<String, Vec<(String, ServiceSpec)>>,
    /// if there is no network definition then just create a default network
    is_default: bool,
    project_name: String,
}

impl NetworkManager {
    pub fn new(project_name: String) -> Self {
        Self {
            map: HashMap::new(),
            service_mapping: HashMap::new(),
            is_default: false,
            network_service: HashMap::new(),
            project_name,
            network_interface: HashMap::new(),
        }
    }

    pub fn network_service_mapping(&self) -> HashMap<String, Vec<(String, ServiceSpec)>> {
        self.network_service.clone()
    }

    pub fn setup_network_conf(&self, network_name: &String) -> Result<()> {
        // generate the config file
        let interface = self.network_interface.get(network_name).ok_or_else(|| {
            anyhow!(
                "Failed to find bridge interface for network {}",
                network_name
            )
        })?;

        let subnet_addr = Ipv4Addr::new(172, 17, 0, 0);
        let gateway_addr = Ipv4Addr::new(172, 17, 0, 1);

        let conf = CliNetworkConfig::from_subnet_gateway(
            network_name,
            interface,
            subnet_addr,
            gateway_addr,
        );

        let conf_value = serde_json::to_value(conf).expect("Failed to parse network config");

        let mut conf_path = PathBuf::from(STD_CONF_PATH);
        conf_path.push(BRIDGE_CONF);
        if let Some(parent) = conf_path.parent()
            && !parent.exists()
        {
            fs::create_dir_all(parent)?;
        }

        fs::write(conf_path, serde_json::to_string_pretty(&conf_value)?)?;

        Ok(())
    }

    pub fn handle(&mut self, spec: &ComposeSpec) -> Result<()> {
        // read the networks
        if let Some(networks_spec) = &spec.networks {
            self.map = networks_spec.clone()
        } else {
            // there is no definition of networks
            self.is_default = true
        }
        self.validate(spec)?;
        // allocate the bridge interface
        self.allocate_interface()?;
        Ok(())
    }

    /// validate the correctness and initialize  the service_mapping
    fn validate(&mut self, spec: &ComposeSpec) -> Result<()> {
        for (srv, srv_spec) in &spec.services {
            // if the srv does not have the network definition then add to the default network
            let network_name = format!("{}_default", self.project_name);
            if srv_spec.networks.is_empty() {
                self.network_service
                    .entry(network_name.clone())
                    .or_default()
                    .push((srv.clone(), srv_spec.clone()));

                // add to map
                self.map.insert(
                    network_name,
                    NetworkSpec {
                        external: Option::None,
                        driver: Some(Bridge),
                    },
                );
            }
            for network_name in &srv_spec.networks {
                if !self.map.contains_key(network_name) {
                    return Err(anyhow!(
                        "bad network's definition network {} is not defined",
                        network_name
                    ));
                }
                self.service_mapping
                    .entry(srv.clone())
                    .or_default()
                    .push(network_name.clone());

                self.network_service
                    .entry(network_name.clone())
                    .or_default()
                    .push((srv.clone(), srv_spec.clone()));
            }
        }
        // all the services don't have the network definition then create a default network
        if self.is_default {
            let network_name = format!("{}_default", self.project_name);

            let services: Vec<(String, ServiceSpec)> = spec
                .services
                .iter()
                .map(|(name, spec)| (name.clone(), spec.clone()))
                .collect();

            self.network_service.insert(network_name.clone(), services);
            self.map.insert(
                network_name,
                NetworkSpec {
                    external: Option::None,
                    driver: Some(Bridge),
                },
            );
        }
        Ok(())
    }

    fn allocate_interface(&mut self) -> Result<()> {
        for (i, (k, v)) in self.map.iter().enumerate() {
            if let Some(driver) = &v.driver {
                match driver {
                    // add the bridge default is rCompose0
                    Bridge => self
                        .network_interface
                        .insert(k.to_string(), format!("rCompose{}", i + 1).to_string()),
                    Overlay => todo!(),
                    Host => todo!(),
                };
            }
        }
        Ok(())
    }

    async fn add_dns_record(&self, srv_name: &str, ip: Ipv4Addr) -> Result<()> {
        let mut stream = connect_dns_socket_with_retry(5, 200).await?;

        let domain = dns::parse_service_to_domain(srv_name, None);

        let msg = dns::DNSUpdateMessage {
            action: dns::UpdateAction::Add,
            name: LowerName::from_str(&domain).unwrap(),
            ip,
        };

        let msg_byte = serde_json::to_vec(&msg).unwrap();

        stream.write_all(&msg_byte).await?;
        stream.write_all(b"\n").await?;

        let mut buf = String::new();
        let mut reader = BufReader::new(&mut stream);
        reader.read_line(&mut buf).await?;
        let buf = buf.trim(); // get rid of the "\n" in  "ok\n"

        if buf != "ok" {
            return Err(anyhow!(
                "fail to add {srv_name}'s dns record, got DNS Server response: {buf}"
            ));
        }
        Ok(())
    }

    /// This function act as a hook func, doese network-related stuff after container started
    /// Currently, it will do the following things:
    ///
    /// 1. Put the Container's IP to Local daemon dns server(use sock)
    ///
    pub(crate) fn after_container_started(
        &self,

        srv_name: &str,
        runner: ContainerRunner,
    ) -> Result<()> {
        let container_ip = runner
            .ip()
            .ok_or_else(|| anyhow!("[container {}]Empty IP address", runner.id()))?;
        // let container_mac = runner
        //     .mac()
        //     .ok_or_else(|| anyhow!("[container {}]Empty MAC address", runner.id()))?;
        if let IpAddr::V4(_ip) = container_ip {
            let alias = vec![runner.id()];
            let mut networks = HashMap::new();
            let mut network_info = HashMap::new();
            for (network_name, _) in self.map.clone() {
                let network_se = match self.network_service.get(&network_name) {
                    Some(network) => network,
                    None => continue,
                };
                for (_container, container_spec) in network_se {
                    let Some(container_name) = container_spec.container_name.as_ref() else {
                        continue;
                    };
                    if *container_name == runner.id() {
                        let network_opts = PerNetworkOptions {
                            aliases: Some(alias.clone()),
                            interface_name: self
                                .network_interface
                                .get(&network_name)
                                .unwrap_or(&"vethcni0".to_string())
                                .clone(),
                            static_ips: Some(vec![container_ip]),
                            static_mac: None,
                            options: None,
                        };
                        let network = Network {
                            dns_enabled: true,
                            driver: "bridge".to_string(),
                            id: "".to_string(), // 目前位置
                            internal: true,
                            ipv6_enabled: false,
                            name: network_name.clone(),
                            network_interface: None,
                            options: None,
                            ipam_options: None,
                            subnets: Some(vec![Subnet {
                                gateway: Some(IpAddr::V4(Ipv4Addr::new(172, 17, 0, 1))),
                                lease_range: None,
                                subnet: IpNet::new(IpAddr::V4(Ipv4Addr::new(172, 17, 0, 0)), 16)
                                    .unwrap(),
                            }]),
                            routes: None,
                            network_dns_servers: Some(vec![]),
                        };

                        networks.insert(network_name.clone(), network_opts);
                        network_info.insert(network_name, network);
                        break;
                    }
                }
            }
            let opts = NetworkOptions {
                container_id: runner.id(),
                container_name: runner.id(),
                container_hostname: None,
                networks,
                network_info,
                port_mappings: None,
                dns_servers: None,
            };

            // IMPORTANT: netavark `Setup::new()` expects a *network namespace file path* (e.g. `/proc/<pid>/ns/net`),
            // not a directory. Passing a directory will cause `setns()` to fail with EINVAL.
            let pid = runner
                .get_container_state()?
                .pid
                .ok_or_else(|| anyhow!("[container {}] PID not found", runner.id()))?;
            let netns_path = format!("/proc/{pid}/ns/net");
            if !Path::new(&netns_path).exists() {
                return Err(anyhow!(
                    "[container {}] netns path not found: {netns_path}",
                    runner.id()
                ));
            }
            let setup = Setup::new(netns_path);

            // Write netavark input JSON into a temp file.
            let mut json_path = std::env::temp_dir();
            json_path.push("rkl-netavark");
            json_path.push(format!("{}.network.json", runner.id()));
            if let Some(parent) = json_path.parent() {
                fs::create_dir_all(parent)?;
            }
            let json_str = serde_json::to_string(&opts)?;
            let mut json_file = std::fs::File::create(&json_path)?;
            json_file.write_all(json_str.as_bytes())?;

            let rootless = !Uid::effective().is_root();
            let config_dir = default_netavark_config_dir(rootless);
            fs::create_dir_all(PathBuf::from(&config_dir))?;

            setup.exec(
                Some(json_path.into_os_string()),
                Some(config_dir),
                None,
                default_aardvark_bin(),
                None,
                rootless,
            )
            .map_err(|e| anyhow!("[container {}] netavark setup failed: {e}", runner.id()))?;
        } else {
            return Err(anyhow!("Unsupported ipv6 type"));
        }

        Ok(())
    }
    // 明天写调试，以及修复尽可能正常运行
    pub fn clean_up() -> Result<()> {
        let pid_file = PID_FILE_PATH;
        if Path::new(pid_file).exists() {
            if let Ok(pid_str) = fs::read_to_string(pid_file)
                && let Ok(pid) = pid_str.trim().parse::<u32>()
            {
                let mut sys = System::new_all();
                sys.refresh_processes();
                if let Some(proc) = sys.process(Pid::from_u32(pid)) {
                    println!("KILL PID: {}", pid);
                    let _ = proc.kill();
                    thread::sleep(Duration::from_secs(3));
                }
            }
            let _ = fs::remove_file(pid_file);
        }

        Ok(())
    }
}

use lazy_static::lazy_static;

lazy_static! {
    static ref RUNTIME: tokio::runtime::Runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("Failed to create tokio runtime for network manager.");
}

fn block_on<F, T>(f: F) -> T
where
    F: Future<Output = T>,
{
    RUNTIME.block_on(f)
}

#[allow(unused)]
fn spawn<F, T>(f: F) -> tokio::task::JoinHandle<T>
where
    F: Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    RUNTIME.spawn(f)
}

pub async fn connect_dns_socket_with_retry(retries: usize, delay_ms: u64) -> Result<UnixStream> {
    for attempt in 1..=retries {
        match UnixStream::connect(DNS_SOCKET_PATH).await {
            Ok(stream) => return Ok(stream),
            Err(e) => {
                if attempt == retries {
                    return Err(anyhow!(
                        "Fatal error: failed to connect local DNS SOCKET after {} attempts: {e}",
                        retries
                    ));
                } else {
                    eprintln!(
                        "Attempt {}/{} failed to connect DNS socket: {}. Retrying in {} ms...",
                        attempt, retries, e, delay_ms
                    );
                    tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                }
            }
        }
    }

    unreachable!()
}
