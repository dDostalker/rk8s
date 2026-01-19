use std::{
    collections::{HashMap, VecDeque},
    env::{self},
    fs::{self, File},
    net::{IpAddr, Ipv4Addr},
    path::{Path, PathBuf},
    vec,
};

use anyhow::{Ok, Result, anyhow};
use clap::Subcommand;
use libcontainer::container::State;
use libcontainer::syscall::syscall::create_syscall;
use liboci_cli::{Delete, List};
use netavark::network::types::{NetworkOptions, PerNetworkOptions};

use serde::{Deserialize, Serialize};
use tokio::runtime::Runtime;
use tracing::debug;

use libruntime::dns::{LOCAL_NAMESERVER, run_local_dns};

use crate::commands::{
    compose::{
        config::ConfigManager,
        network::NetworkManager,
        spec::{ComposeSpec, ServiceSpec},
    },
    container::{ContainerRunner, remove_container},
    delete, list,
};
use common::{ContainerSpec, Port};
use libruntime::rootpath;
use libruntime::volume::{
    PatternType, VolumeManager, VolumeMetadata, VolumePattern, string_to_pattern,
};
type ComposeAction = Box<dyn FnOnce(&mut ComposeManager) -> Result<()>>;

// pub mod config;
pub mod config;
pub mod network;
pub mod spec;

use clap::Args;

// Common Args shared by commands
#[derive(Args)]
pub struct PsArgs {
    #[arg(long = "project-name", short, value_name = "PROJECT_NAME")]
    pub project_name: Option<String>,

    #[arg(short = 'f', value_name = "COMPOSE_YAML")]
    pub compose_yaml: Option<String>,
}

#[derive(Args)]
pub struct DownArgs {
    #[arg(long = "project-name", short, value_name = "PROJECT_NAME")]
    pub project_name: Option<String>,

    #[arg(short = 'f', value_name = "COMPOSE_YAML")]
    pub compose_yaml: Option<String>,
}

#[derive(Args)]
pub struct UpArgs {
    #[arg(value_name = "COMPOSE_YAML")]
    pub compose_yaml: Option<String>,

    #[arg(long = "project-name", value_name = "PROJECT_NAME")]
    pub project_name: Option<String>,
}

#[derive(Subcommand)]
pub enum ComposeCommand {
    #[command(about = "Start a compose application from a compose yaml")]
    Up(UpArgs),

    #[command(about = "stop and delete all the containers in the compose application")]
    Down(DownArgs),

    #[command(about = "List all the containers' state in compose application")]
    Ps(PsArgs),

    #[command(about = "Start up DNS Server", hide = true)]
    Server,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ComposeMetadata {
    pub containers: Vec<State>,
    pub volumes: Vec<String>,
    pub project_name: String,
}

pub struct ComposeManager {
    /// the path to store the basic info of compose application
    root_path: PathBuf,
    project_name: String,
    containers: Vec<State>,
    volumes: Vec<String>,
    network_manager: NetworkManager,
    config_manager: ConfigManager,
    startup_order: HashMap<String, usize>,
}

impl ComposeManager {
    fn new(project_name: String) -> Result<Self> {
        let root_path = rootpath::determine(None, &*create_syscall())?;

        // /root_path/compose/compose_id to store the state of current compose application
        let root_path = Path::new(&root_path).join("compose").join(&project_name);

        Ok(Self {
            root_path,
            network_manager: NetworkManager::new(project_name.clone()),
            config_manager: ConfigManager::new(),
            project_name,
            containers: vec![],
            volumes: vec![],
            startup_order: HashMap::new(),
        })
    }

    fn down(&self, _: DownArgs) -> Result<()> {
        // delete all the containers in the compose application
        if !self.root_path.exists() {
            return Err(anyhow!("The project {} does not exist", self.project_name));
        }

        self.clean_up()
    }

    fn clean_up(&self) -> Result<()> {
        // delete container
        for container in &self.containers {
            remove_container(&self.root_path, container)?;
        }
        self.clean_up_network()?;

        fs::remove_dir_all(&self.root_path)
            .map_err(|e| anyhow!("failed to delete the whole project: {}", e))
    }

    fn get_root_path_by_name(&self, project_name: String) -> Result<PathBuf> {
        let root_path = rootpath::determine(None, &*create_syscall())?;
        let new_path = Path::new(&root_path).join("compose").join(project_name);
        Ok(new_path)
    }

    fn up(&mut self, args: UpArgs) -> Result<()> {
        /*
         *
         * 来源问题，如何知道创建了什么网络，第二个问题ns的问题
         *
         1 │services:
         2 │  web:
         3 │    image: nginx
         4 │    networks:
         5 │      - app-network
         6 │  db:
         7 │    image: postgres
         8 │    networks:
         9 │      - app-network
        10 │
        11 │networks:
        12 │  app-network:
        13 │    driver: bridge
        `networks` 和 `network_info` 的作用


         一、`networks`（容器级别的网络选项）

         作用：描述容器在每个网络上的配置选项。
         包含内容：
         • interface_name：容器内的接口名（如 eth0）
         • static_ips：静态 IP 地址（如果指定）
         • aliases：DNS 别名列表
         • static_mac：静态 MAC 地址（如果指定）
         • options：驱动特定的选项

         特点：
         • 容器级别：每个容器在每个网络上可能有不同的配置
         • 动态：可以在运行时修改（如添加别名）


         二、`network_info`（网络级别的配置信息）

         作用：描述网络本身的定义和配置。
         包含内容：
         • name：网络名称
         • id：网络 ID
         • driver：网络驱动（如 bridge）
         • subnets：子网配置（包含网关）
         • dns_enabled：是否启用 DNS
         • network_dns_servers：网络级别的 DNS 服务器
         • internal：是否为内部网络
         • ipv6_enabled：是否启用 IPv6

         特点：
         • 网络级别：同一网络的所有容器共享相同的网络配置
         • 静态：网络配置在创建网络时确定，通常不会改变


         Podman 如何获取它们


         一、获取 `networks`（容器级别的选项）

         流程：

         1. 从容器状态数据库读取

         代码位置：podman/libpod/networking_linux.go:38-48

            1 │// 1. 获取容器连接的网络
            2 │networks, err := ctr.networks()  // ← 从这里获取
            3 │if err != nil {
            4 │    return nil, err
            5 │}
            6 │
            7 │// 2. 构建 NetworkOptions
            8 │netOpts := ctr.getNetworkOptions(networks)  // ← 使用 networks


         2. `ctr.networks()` 的实现

         代码位置：podman/libpod/container.go:1471-1473

            1 │func (c *Container) networks() (map[string]types.PerNetworkOptions, error) {
            2 │    return c.runtime.state.GetNetworks(c)  // ← 从状态数据库获取
            3 │}


         3. 从 SQLite 数据库读取

         代码位置：podman/libpod/sqlite_state.go:908-931

            1 │func (s *SQLiteState) GetNetworks(ctr *Container) (map[string]types.PerNetworkOptions, error) {
            2 │    // 从容器配置中读取
            3 │    cfg, err := s.getCtrConfig(ctr.ID())
            4 │    if err != nil {
            5 │        return nil, err
            6 │    }
            7 │
            8 │    return cfg.Networks, nil  // ← 返回容器在每个网络上的选项
            9 │    // cfg.Networks 的结构：
           10 │    // map[string]PerNetworkOptions {
           11 │    //   "app-network": {
           12 │    //     InterfaceName: "eth0",
           13 │    //     Aliases: ["web"],
           14 │    //     StaticIPs: nil,
           15 │    //     ...
           16 │    //   }
           17 │    // }
           18 │}


         4. 数据来源

         • 容器创建时：从 c.config.Networks 获取（用户指定或默认）
         • 容器运行时：从状态数据库读取（可能已被修改）

         代码位置：podman/libpod/runtime_ctr.go:260-306

            1 │// 容器创建时，处理网络配置
            2 │if len(ctr.config.Networks) > 0 {
            3 │    for nameOrID, opts := range ctr.config.Networks {
            4 │        // 规范化网络名称
            5 │        netName, nicName, err := r.normalizeNetworkName(nameOrID)
            6 │
            7 │        // 设置接口名（如果未指定）
            8 │        if opts.InterfaceName == "" {
            9 │            opts.InterfaceName = nicName  // 或自动分配 eth0, eth1...
           10 │        }
           11 │
           12 │        // 自动添加 DNS 别名
           13 │        opts.Aliases = append(opts.Aliases, getExtraNetworkAliases(ctr)...)
           14 │        // getExtraNetworkAliases 添加：
           15 │        // - 容器 ID 前 12 位
           16 │        // - 容器主机名（如果设置了）
           17 │
           18 │        normalizeNetworks[netName] = opts
           19 │    }
           20 │    ctr.config.Networks = normalizeNetworks
           21 │    // 然后保存到状态数据库
           22 │}


         二、获取 `network_info`（网络级别的配置）

         流程：

         1. Netavark 加载网络配置

         代码位置：podman/vendor/go.podman.io/common/libnetwork/netavark/run.go:164-183

            1 │func (n *netavarkNetwork) convertNetOpts(opts types.NetworkOptions) (*netavarkOptions, bool, error) {
            2 │    netavarkOptions := netavarkOptions{
            3 │        NetworkOptions: opts,  // 包含 networks（容器级别）
            4 │        Networks:       make(map[string]*types.Network, len(opts.Networks)),
            5 │    }
            6 │
            7 │    // 为每个网络加载网络配置
            8 │    for network := range opts.Networks {
            9 │        net, err := n.getNetwork(network)  // ← 从配置文件加载
           10 │        if err != nil {
           11 │            return nil, false, err
           12 │        }
           13 │        netavarkOptions.Networks[network] = net  // ← 添加到 network_info
           14 │    }
           15 │    return &netavarkOptions, needsPlugin, nil
           16 │}


         2. `getNetwork()` 从内存映射查找

         代码位置：podman/vendor/go.podman.io/common/libnetwork/netavark/network.go:317-341

            1 │func (n *netavarkNetwork) getNetwork(nameOrID string) (*types.Network, error) {
            2 │    // 从内存中的 networks 映射查找
            3 │    if val, ok := n.networks[nameOrID]; ok {
            4 │        return val, nil  // ← 返回网络配置
            5 │    }
            6 │
            7 │    // 如果没找到，尝试通过 ID 查找
            8 │    for _, val := range n.networks {
            9 │        if strings.HasPrefix(val.ID, nameOrID) {
           10 │            return val, nil
           11 │        }
           12 │    }
           13 │
           14 │    return nil, fmt.Errorf("unable to find network")
           15 │}


         3. `loadNetworks()` 从文件系统加载

         代码位置：podman/vendor/go.podman.io/common/libnetwork/netavark/network.go:182-261

            1 │func (n *netavarkNetwork) loadNetworks() error {
            2 │    // 网络配置目录
            3 │    // 默认：/etc/containers/networks
            4 │    // 或：/run/containers/networks（运行时）
            5 │
            6 │    files, err := os.ReadDir(n.networkConfigDir)
            7 │    // 读取所有 .json 文件
            8 │
            9 │    networks := make(map[string]*types.Network, len(files))
           10 │    for _, f := range files {
           11 │        if filepath.Ext(f.Name()) != ".json" {
           12 │            continue
           13 │        }
           14 │
           15 │        // 读取网络配置文件
           16 │        path := filepath.Join(n.networkConfigDir, f.Name())
           17 │        // 例如：/etc/containers/networks/app-network.json
           18 │
           19 │        file, err := os.Open(path)
           20 │        network := new(types.Network)
           21 │        err = json.NewDecoder(file).Decode(network)  // ← 解析 JSON
           22 │
           23 │        // 验证网络名称
           24 │        if network.Name+".json" != f.Name() {
           25 │            continue
           26 │        }
           27 │
           28 │        // 添加到内存映射
           29 │        networks[network.Name] = network
           30 │    }
           31 │
           32 │    n.networks = networks  // ← 保存到内存
           33 │    return nil
           34 │}


         4. 网络配置文件的位置和格式

         配置文件位置：
         • 默认：/etc/containers/networks/{network-name}.json
         • 运行时：/run/containers/networks/{network-name}.json

         配置文件示例：/etc/containers/networks/app-network.json

            1 │{
            2 │  "name": "app-network",
            3 │  "id": "app-network-id-123...",
            4 │  "driver": "bridge",
            5 │  "network_interface": "app-network",
            6 │  "subnets": [{
            7 │    "subnet": "10.88.0.0/24",
            8 │    "gateway": "10.88.0.1"
            9 │  }],
           10 │  "ipv6_enabled": false,
           11 │  "internal": false,
           12 │  "dns_enabled": true,
           13 │  "network_dns_servers": [],
           14 │  "ipam_options": {
           15 │    "driver": "host-local"
           16 │  }
           17 │}


         完整数据流
         */
        let compose_yaml = args.compose_yaml;
        // check the project_id exists?
        if self.root_path.exists() {
            return Err(anyhow!("The project {} already exists", self.project_name));
        }

        let target_path = get_yml_path(compose_yaml)?;

        // read the yaml
        let spec = parse_spec(target_path)?;

        self.handle_depends_on(&spec)?;

        // top-field manager handle those field
        /// 明天把ips信息和获取以和创建初始ns优先完成，现在的netavark没法通过库来传递
        let _ = &mut self.network_manager.handle(&spec)?;

        self.handle_volumes(&spec)?;

        let _ = &mut self.config_manager.handle(&spec);

        // start the whole containers
        if let Err(err) = self.run(&spec) {
            self.clean_up().ok();
            return Err(anyhow!("failed to up: {}", err));
        }

        // store the spec info into a .json file
        self.persist_compose_state()?;

        println!("Project {} starts successfully", self.project_name);
        Ok(())
    }

    // persist the compose application's status to a json file
    ///{
    /// "project_name": "",
    /// "containers": [ {} {},],
    /// "volumes":[]
    ///}
    fn persist_compose_state(&self) -> Result<()> {
        let metadata = ComposeMetadata {
            containers: self.containers.clone(),
            volumes: self.volumes.clone(),
            project_name: self.project_name.clone(),
        };
        let json_str = serde_json::to_string_pretty(&metadata)?;

        let file_path = self.root_path.join("metadata.json");
        fs::create_dir_all(&self.root_path)?;
        fs::write(file_path, json_str)?;
        Ok(())
    }

    pub fn read_spec(&self, path: PathBuf) -> Result<ComposeSpec> {
        let path = path
            .to_str()
            .ok_or_else(|| anyhow!("compose.yml file is None"))?;
        let reader = File::open(path)?;
        let spec: ComposeSpec = serde_yaml::from_reader(reader).map_err(|_| {
            anyhow!("Read the compose specification failed, make sure the file is valid")
        })?;
        Ok(spec)
    }
    // todo
    fn run(&mut self, spec: &ComposeSpec) -> Result<()> {
        let network_mapping = self.network_manager.network_service_mapping();

        for (network_name, services) in network_mapping {
            println!("Creating network: {network_name}");
            let mut ordered: Vec<(String, ServiceSpec)> = services.clone();
            ordered.sort_by_key(|(name, _)| self.startup_order.get(name).unwrap());

            for (srv_name, srv) in ordered.into_iter() {
                let container_ports = map_port_style(srv.ports.clone())?;
                let container_spec = ContainerSpec {
                    name: srv
                        .container_name
                        .clone()
                        // .map(|str| format!("compose_{}", str))
                        .unwrap_or(self.generate_container_name(&srv_name)),
                    image: srv.image.clone(),
                    ports: container_ports,
                    args: srv.command.clone(),
                    resources: None,
                    liveness_probe: None,
                    readiness_probe: None,
                    startup_probe: None,
                    security_context: None,
                    env: None,
                    volume_mounts: None,
                    command: None,
                    working_dir: None,
                };

                // handle the services volume name
                let mut patterns: Result<Vec<VolumePattern>> = srv
                    .volumes
                    .iter()
                    .map(|v| v.as_str())
                    .map(string_to_pattern)
                    .collect();

                patterns = patterns.map(|mut vec| {
                    vec.iter_mut().for_each(|pattern| {
                        if let PatternType::Named = pattern.pattern_type {
                            pattern.host_path = format!(
                                "{}_{}",
                                spec.name.clone().unwrap_or("compose_default".to_string()),
                                pattern.host_path
                            );
                        }
                    });
                    vec
                });

                // println!("compose get volume patterns: {patterns:?}");
                // generate the volumes Mount
                let (mut volumes, mounts) =
                    VolumeManager::new()?.handle_container_volume(patterns?, true)?;

                self.volumes.append(&mut volumes);

                debug!("get mount: {:#?}", mounts);

                //  setup the network_conf file
                self.network_manager
                    .setup_network_conf(&network_name)
                    .map_err(|e| {
                        anyhow!(
                            "Service [{}] create network Config file failed: {}",
                            srv_name,
                            e
                        )
                    })?;
                let configs_mounts = self.config_manager.get_mounts_by_service(&srv_name);

                let mut runner =
                    ContainerRunner::from_spec(container_spec, Some(self.root_path.clone()))?;

                runner.add_mounts(mounts);
                runner.add_mounts(configs_mounts);

                match runner.run() {
                    std::result::Result::Ok(_) => {
                        self.containers.push(runner.get_container_state()?);
                    }
                    Err(err) => {
                        // create one container failed delete others
                        println!(
                            "container {} created failed: {}",
                            runner.get_container_id()?,
                            err
                        );
                        for state in &self.containers {
                            if let Err(err) = delete(
                                Delete {
                                    container_id: state.id.clone(),
                                    force: true,
                                },
                                self.root_path.clone(),
                            ) {
                                println!("container {} deleted failed: {}", state.id, err)
                            } else {
                                println!("container {} deleted during the rollback", state.id)
                            }
                        }
                        return Err(err);
                    }
                };
                self.network_manager
                    .after_container_started(&srv_name, runner)
                    .map_err(|e| anyhow!("network setup failed: {e}"))?;
            }
        }
        // return the compose application's state
        //
        Ok(())
    }

    fn ps(&self, ps_args: PsArgs) -> Result<()> {
        let PsArgs {
            compose_yaml,
            project_name,
        } = ps_args;
        let list_arg = List {
            format: "".to_string(),
            quiet: false,
        };

        let target_path = if !self.root_path.exists() {
            let yml_file = get_yml_path(compose_yaml)?;
            let spec = self.read_spec(yml_file)?;
            match spec.name {
                Some(name) => self.get_root_path_by_name(name)?,
                None => return Err(anyhow!("Invalid Compose Spec (no project name is set)")),
            }
        } else if let Some(name) = project_name {
            self.get_root_path_by_name(name)?
        } else {
            self.root_path.clone()
        };

        list(list_arg, target_path).map_err(|e| {
            if let Some(io_err) = e.downcast_ref::<std::io::Error>()
                && io_err.kind() == std::io::ErrorKind::NotFound
            {
                return anyhow!("There is no running compose application");
            }
            // Fallback for other errors, ensuring all list errors are handled consistently
            anyhow!("Failed to list compose containers: {}", e)
        })
    }

    /// if the `container_name` field is not supplied then create a random container_name
    /// for the service container
    pub fn generate_container_name(&self, srv_name: &String) -> String {
        let root = self
            .root_path
            .file_name()
            .and_then(|os_str| os_str.to_str())
            .unwrap_or("unknown");
        let timestamp = chrono::Utc::now().timestamp() % 1000; // persist 4 bits
        format!("{root}_{srv_name}_{timestamp}")
    }

    /// This function interate the named volumes in compose spec
    /// and create it if it is has not be created
    pub fn handle_volumes(&mut self, compose_spec: &ComposeSpec) -> Result<()> {
        // create volumes that are pre-defined in compose specification
        if let Some(volumes) = &compose_spec.volumes {
            let mut global_manager = VolumeManager::new()?;
            for (key, spec) in volumes {
                println!("compose get volume: {}", key);
                // use existing volume
                let volume_name = if spec.external.unwrap_or(false) {
                    spec.name.clone().unwrap_or_else(|| key.to_string())
                } else {
                    format!(
                        "{}_{}",
                        &compose_spec
                            .name
                            .clone()
                            .unwrap_or(String::from("compose_default")),
                        key
                    )
                };
                // ignore this volume is already exists
                global_manager
                    .create_(
                        volume_name.clone(),
                        spec.driver.clone(),
                        spec.opts.clone().unwrap_or_default(),
                    )
                    .or_else(|e| {
                        if !e.to_string().contains("already exists") {
                            Err(e)
                        } else {
                            std::result::Result::Ok(VolumeMetadata::default())
                        }
                    })?;
            }
        }
        Ok(())
    }

    /// TODO: clean the bridge that is generated by compose up
    fn clean_up_network(&self) -> Result<()> {
        NetworkManager::clean_up()?;
        Ok(())
    }

    /// This function handle the compose's depends_on functionality.
    /// Use Kahn Algorithm to implement Topological sorting.
    /// Return the final startup sequences of compose services.
    fn handle_depends_on(&mut self, spec: &ComposeSpec) -> Result<()> {
        // Init status
        let mut graph: HashMap<String, Vec<String>> = HashMap::new();
        let mut in_degree: HashMap<String, usize> = HashMap::new();

        for srv_name in spec.services.keys() {
            graph.insert(srv_name.clone(), vec![]);
            in_degree.insert(srv_name.clone(), 0);
        }

        for (srv_name, srv_spec) in &spec.services {
            for dep in &srv_spec.depends_on {
                if !graph.contains_key(dep) {
                    return Err(anyhow!("{srv_name} depends on {dep} is not defined"));
                }
                graph.get_mut(dep).unwrap().push(srv_name.clone());
                *in_degree.get_mut(srv_name).unwrap() += 1
            }
        }

        let mut result: Vec<String> = vec![];
        let mut queue: VecDeque<String> = VecDeque::new();

        for (srv, &degree) in &in_degree {
            if degree == 0 {
                queue.push_back(srv.clone());
            }
        }

        while let Some(srv) = queue.pop_front() {
            // recored the degree = 0's srv
            result.push(srv.clone());

            if let Some(deps) = graph.get(&srv) {
                for dep in deps {
                    let degree = in_degree.get_mut(dep).unwrap();
                    *degree -= 1;
                    if *degree == 0 {
                        queue.push_back(dep.clone());
                    }
                }
            }
        }

        self.startup_order = result
            .iter()
            .enumerate()
            .map(|(i, name)| (name.clone(), i))
            .collect();

        Ok(())
    }

    fn start_dns_server(&self) -> Result<()> {
        let rt = Runtime::new()
            .map_err(|e| anyhow!("failed to init dns server's tokio runtime: {e}"))?;
        rt.block_on(async {
            run_local_dns(Some(53), vec![])
                .await
                .map_err(|e| anyhow!("failed to run local dns server: {e}"))
        })?;

        Ok(())
    }
}

pub fn parse_spec(path: PathBuf) -> Result<ComposeSpec> {
    let path = path
        .to_str()
        .ok_or_else(|| anyhow!("compose.yml file is None"))?;
    let reader = File::open(path)?;
    let spec: ComposeSpec = serde_yaml::from_reader(reader).map_err(|e| {
        anyhow!(
            "Read the compose specification failed, make sure the file is valid: {}",
            e
        )
    })?;
    Ok(spec)
}

// map the compose-style port to k8s-container-style ports
// compose-style: "(host-ip) 80: (container-ip) 3000"
// k8s-container-style:
// - containerPort: 80
//   protocol: ""
//   hostPort: 0
//   hostIP: "" default is ""
fn map_port_style(ports: Vec<String>) -> Result<Vec<Port>> {
    ports
        .into_iter()
        .map(|port| {
            let parts: Vec<&str> = port.split(":").collect();
            let (host_ip, host_port, container_port) = match parts.len() {
                2 => ("", parts[0], parts[1]),
                3 => (parts[0], parts[1], parts[2]),
                _ => return Err(anyhow!("Invalid port mapping syntax in compose file")),
            };

            let host_port = host_port
                .parse::<i32>()
                .map_err(|_| anyhow!("Invalid port mapping syntax in compose file"))?;

            let container_port = container_port
                .parse::<i32>()
                .map_err(|_| anyhow!("Invalid port mapping syntax in compose file"))?;

            let host_ip = host_ip.to_string();

            Ok(Port {
                container_port,
                protocol: "".to_string(),
                host_port,
                host_ip,
            })
        })
        .collect()
}

pub fn get_yml_path(compose_yaml: Option<String>) -> Result<PathBuf> {
    let target_path = if let Some(path) = compose_yaml {
        PathBuf::from(path)
    } else {
        let cwd = env::current_dir()?;
        let yml = cwd.join("compose.yml");
        let yaml = cwd.join("compose.yaml");
        if yml.exists() {
            yml
        } else if yaml.exists() {
            yaml
        } else {
            return Err(anyhow!(
                "No compose.yml or compose.yaml file in current directory: {}",
                cwd.display()
            ));
        }
    };
    Ok(target_path)
}

pub fn get_manager_from_name(project_name: Option<String>) -> Result<Box<ComposeManager>> {
    let manager = match project_name {
        Some(name) => ComposeManager::new(name),
        None => {
            let cwd = env::current_dir()?;
            let project_name = cwd
                .file_name()
                .and_then(|os_str| os_str.to_str())
                .ok_or_else(|| anyhow!("Failed to get current directory'name"))?
                .to_string();
            ComposeManager::new(project_name)
        }
    }?;
    Ok(Box::new(manager))
}

pub fn compose_execute(command: ComposeCommand) -> Result<()> {
    let (project_name, action): (Option<String>, ComposeAction) = match command {
        ComposeCommand::Up(up_args) => {
            let name = up_args.project_name.clone();
            (name, Box::new(move |manager| manager.up(up_args)))
        }
        ComposeCommand::Down(down_args) => {
            let name = down_args.project_name.clone();
            (name, Box::new(move |manager| manager.down(down_args)))
        }
        ComposeCommand::Ps(ps_args) => {
            let name = ps_args.project_name.clone();
            (name, Box::new(move |manager| manager.ps(ps_args)))
        }
        ComposeCommand::Server => (None, Box::new(move |manager| manager.start_dns_server())),
    };

    let mut manager = get_manager_from_name(project_name)?;
    action(&mut manager)
}

#[cfg(test)]
mod test {
    use super::*;
    use serial_test::serial;
    use std::fs;
    use tempfile::tempdir;

    fn get_test_yml() -> String {
        r#"
name: test_proj
services:
  web:
    image: test/bundles/busybox/
    ports: ["8080:80"]
    volumes:
      - /tmp/mount/dir:/mnt
volumes:

"#
        .to_string()
    }

    fn get_test_multiple_service() -> String {
        r#"
services:
  backend:
    container_name: back
    image: ./test/bundles/busybox
    command: ["sleep", "300"]
    ports:
      - "8080:8080"
    networks:
      - libra-net
    volumes:
      - /tmp/mount/dir:/mnt
  frontend:
    container_name: front
    image: ./test/bundles/busybox
    command: ["sleep", "300"]
    ports:
      - "80:80"
networks:
  libra-net:
    driver: bridge
"#
        .to_string()
    }

    #[test]
    fn test_new_compose_manager() {
        let mgr = ComposeManager::new("demo_proj".to_string());
        assert!(mgr.is_ok());
        let mgr = mgr.unwrap();
        assert!(mgr.root_path.ends_with("compose/demo_proj"));
        assert_eq!(mgr.project_name, "demo_proj");
    }

    #[test]
    fn test_get_root_path_by_name() {
        let mgr = ComposeManager::new("abc".to_string()).unwrap();
        let path = mgr.get_root_path_by_name("xyz".to_string()).unwrap();
        assert!(path.ends_with("compose/xyz"));
    }

    #[test]
    fn test_persist_and_read_spec() {
        let dir = tempdir().unwrap();
        let test_path = dir.path().join("compose.yml");
        let yaml = get_test_yml();

        fs::write(&test_path, yaml).unwrap();
        let mgr = ComposeManager::new("test_proj".to_string()).unwrap();
        let spec = mgr.read_spec(test_path.clone()).unwrap();
        assert_eq!(spec.name, Some("test_proj".to_string()));
        assert!(spec.services.contains_key("web"));
        assert_eq!(spec.services["web"].image, "test/bundles/busybox/");
        assert_eq!(spec.services["web"].volumes[0], "/tmp/mount/dir:/mnt");
    }

    #[test]
    fn test_map_port_style() {
        let ports = vec!["127.0.0.1:8080:80".to_string(), "8081:81".to_string()];
        let mapped = map_port_style(ports).unwrap();
        assert_eq!(mapped.len(), 2);
        assert_eq!(mapped[0].host_ip, "127.0.0.1");
        assert_eq!(mapped[0].host_port, 8080);
        assert_eq!(mapped[0].container_port, 80);
        assert_eq!(mapped[1].host_ip, "");
        assert_eq!(mapped[1].host_port, 8081);
        assert_eq!(mapped[1].container_port, 81);
    }

    #[tokio::test]
    #[serial]
    async fn test_get_yml_path_with_none() {
        let dir = tempdir().unwrap();
        let yml = dir.path().join("compose.yml");
        fs::write(&yml, "name: demo\nservices: {}\n").unwrap();
        let _cwd = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
        let path = get_yml_path(None).unwrap();
        assert!(path.ends_with("compose.yml"));
        std::env::set_current_dir(_cwd).unwrap();
    }

    #[test]
    fn test_get_manager_from_name_some() {
        let mgr = get_manager_from_name(Some("abc_proj".to_string())).unwrap();
        assert_eq!(mgr.project_name, "abc_proj");
    }

    #[tokio::test]
    #[serial]
    async fn test_up() {
        let root_dir = tempdir().unwrap();
        let root_path = root_dir.path();
        let project_name = root_dir
            .path()
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();

        fs::write(
            root_dir.path().join("compose.yml"),
            get_test_multiple_service(),
        )
        .unwrap();

        let mut manager = ComposeManager::new(project_name.clone()).unwrap();
        manager
            .up(UpArgs {
                compose_yaml: Some(root_path.join("compose.yml").to_str().unwrap().to_owned()),
                project_name: Some(project_name),
            })
            .unwrap();
    }
}
