# Podman Compose 的 DNS 服务如何调用 Netavark

## 概述

Podman Compose 的 DNS 服务通过以下流程工作：
1. **Podman compose** 命令是外部工具的包装器（docker-compose/podman-compose）
2. 外部工具通过 **Podman API** 创建容器
3. Podman 在创建容器时**创建网络命名空间**并调用 **netavark**
4. Netavark 配置网络，如果网络启用了 DNS，会启动 **aardvark-dns**

## 完整调用链

```
docker-compose/podman-compose
    ↓ (通过 DOCKER_HOST 环境变量)
Podman API (unix:///run/podman/podman.sock)
    ↓
Podman libpod (创建容器)
    ↓
创建网络命名空间 (netns.NewNS())
    ↓
调用 netavark setup {namespacePath}
    ↓
Netavark 配置网络
    ↓
如果网络启用 DNS → 启动 aardvark-dns
```

## 详细流程

### 1. Podman Compose 命令执行

```go
// cmd/podman/compose.go
func composeMain(cmd *cobra.Command, args []string) error {
    // 执行外部 compose 提供者（docker-compose 或 podman-compose）
    return composeProviderExec(args, nil, nil, shouldLog)
}

func composeProviderExec(args []string, stdout io.Writer, stderr io.Writer, warn bool) error {
    provider, err := composeProvider()  // 查找 docker-compose 或 podman-compose
    // ...
    env, err := composeEnv()  // 设置 DOCKER_HOST 环境变量
    // DOCKER_HOST=unix:///run/podman/podman.sock
    
    cmd := exec.Command(provider, args...)
    cmd.Env = append(os.Environ(), env...)
    return cmd.Run()
}
```

**关键点**：
- Podman compose 设置 `DOCKER_HOST=unix:///run/podman/podman.sock`
- 外部 compose 工具通过这个 socket 连接到 Podman API

### 2. 容器创建和网络命名空间创建

当外部 compose 工具通过 Podman API 创建容器时：

```go
// libpod/container_internal_linux.go
func (c *Container) init(ctx context.Context, retainRetry bool) error {
    // ...
    if c.config.CreateNetNS && noNetNS && !c.config.PostConfigureNetNS {
        // 创建网络命名空间
        netNS, networkStatus, createNetNSErr = c.runtime.createNetNS(c)
        // ...
        c.state.NetNS = netNS  // 保存命名空间路径
        c.state.NetworkStatus = networkStatus
    }
}
```

**命名空间创建**：

```go
// libpod/networking_linux.go
func (r *Runtime) createNetNS(ctr *Container) (n string, q map[string]types.StatusBlock, retErr error) {
    // 创建新的网络命名空间
    ctrNS, err := netns.NewNS()
    if err != nil {
        return "", nil, fmt.Errorf("creating network namespace for container %s: %w", ctr.ID(), err)
    }
    
    // 配置网络命名空间（调用 netavark）
    networkStatus, err := r.configureNetNS(ctr, ctrNS.Path())
    return ctrNS.Path(), networkStatus, err
}
```

**命名空间路径格式**：
- 新创建的命名空间：`/var/run/netns/{container-id}`（bind mount）
- 运行中的容器：`/proc/{pid}/ns/net`

### 3. 调用 Netavark 配置网络

```go
// libpod/networking_linux.go
func (r *Runtime) configureNetNS(ctr *Container, ctrNS string) (status map[string]types.StatusBlock, rerr error) {
    // ...
    networks, err := ctr.networks()  // 获取容器要加入的网络
    netOpts := ctr.getNetworkOptions(networks)
    
    // 调用 netavark setup
    netStatus, err := r.setUpNetwork(ctrNS, netOpts)
    return netStatus, err
}

// libpod/networking_common.go
func (r *Runtime) setUpNetwork(ns string, opts types.NetworkOptions) (map[string]types.StatusBlock, error) {
    return r.network.Setup(ns, types.SetupOptions{NetworkOptions: opts})
}
```

### 4. Netavark Setup 执行

```go
// vendor/go.podman.io/common/libnetwork/netavark/run.go
func (n *netavarkNetwork) Setup(namespacePath string, options types.SetupOptions) (_ map[string]types.StatusBlock, retErr error) {
    // ...
    // 分配 IP 地址
    err = n.allocIPs(&options.NetworkOptions)
    
    // 转换网络选项
    netavarkOpts, needPlugin, err := n.convertNetOpts(options.NetworkOptions)
    
    // 执行 netavark 命令
    setup := func() error {
        return n.execNetavark([]string{"setup", namespacePath}, needPlugin, netavarkOpts, &result)
    }
    
    return setup()
}
```

**Netavark 命令执行**：

```go
// vendor/go.podman.io/common/libnetwork/netavark/exec.go
func (n *netavarkNetwork) execNetavark(args []string, needPlugin bool, stdin, result any) error {
    // 构建命令：netavark setup {namespacePath}
    cmd := exec.Command(n.netavarkBinary, append(n.getCommonNetavarkOptions(needPlugin), args...)...)
    
    // 通过 stdin 传递 JSON 配置
    cmd.Stdin = stdinR
    cmd.Stdout = stdoutW
    
    // 执行命令
    err = json.NewEncoder(stdinW).Encode(stdin)  // 发送网络配置 JSON
    err = cmd.Wait()
    err = dec.Decode(result)  // 解析 netavark 返回的结果
}
```

**实际执行的命令**：
```bash
netavark --config /run/containers/networks \
         --rootless=false \
         --aardvark-binary=/usr/libexec/podman/aardvark-dns \
         setup /var/run/netns/{container-id} < {network-config.json}
```

### 5. Netavark 配置网络和启动 DNS

```rust
// netavark/src/commands/setup.rs
pub fn exec(&self, ...) -> NetavarkResult<()> {
    // 验证命名空间路径
    network::validation::ns_checks(&self.network_namespace_path)?;
    
    // 加载网络配置
    let network_options = network::types::NetworkOptions::load(input_file)?;
    
    // 打开命名空间文件
    let (mut hostns, mut netns) = core_utils::open_netlink_sockets(&self.network_namespace_path)?;
    
    // 为每个网络创建驱动并配置
    for named_network_opts in &network_options.networks {
        let mut driver = get_network_driver(DriverInfo {
            netns_path: &self.network_namespace_path,  // 使用传入的命名空间路径
            // ...
        })?;
        
        // 设置网络（创建接口、配置 IP、路由等）
        let (status, aardvark_entry) = driver.setup((&mut hostns.netlink, &mut netns.netlink))?;
        
        // 如果网络启用了 DNS，收集 aardvark 条目
        if let Some(a) = aardvark_entry {
            aardvark_entries.push(a);
        }
    }
    
    // 如果有 DNS 条目，启动/更新 aardvark-dns
    if !aardvark_entries.is_empty() {
        let aardvark_interface = Aardvark::new(path, rootless, aardvark_bin, dns_port);
        aardvark_interface.commit_netavark_entries(aardvark_entries)?;
    }
}
```

### 6. Aardvark-DNS 启动

```rust
// netavark/src/dns/aardvark.rs
pub fn commit_netavark_entries(&self, entries: Vec<AardvarkEntry>) -> NetavarkResult<()> {
    // 写入 DNS 配置到文件
    self.write_config_files(entries)?;
    
    // 启动或通知 aardvark-dns
    if self.get_aardvark_pid().is_err() {
        // 如果 aardvark-dns 未运行，启动它
        self.start_aardvark_server()?;
    } else {
        // 如果已运行，发送 SIGHUP 通知重新加载配置
        self.notify(true, false)?;
    }
}

pub fn start_aardvark_server(&self) -> NetavarkResult<()> {
    let mut aardvark_args = vec![];
    
    // 如果使用 systemd，通过 systemd-run 启动
    if is_using_systemd() && Aardvark::is_executable_in_path("systemd-run") {
        aardvark_args = vec![
            OsStr::new("systemd-run"),
            OsStr::new("-q"),
            OsStr::new("--scope"),
        ];
    }
    
    aardvark_args.extend(vec![
        self.aardvark_bin.as_os_str(),  // /usr/libexec/podman/aardvark-dns
        OsStr::new("--config"),
        self.config.as_os_str(),         // /run/containers/networks/aardvark-dns
        OsStr::new("-p"),
        self.port.as_os_str(),           // 53 或 NETAVARK_DNS_PORT
        OsStr::new("run"),
    ]);
    
    Command::new(aardvark_args[0])
        .args(&aardvark_args[1..])
        .output()?;
}
```

**实际执行的命令**：
```bash
systemd-run -q --scope \
    /usr/libexec/podman/aardvark-dns \
    --config /run/containers/networks/aardvark-dns \
    -p 53 \
    run
```

## 命名空间创建的依据

### 1. 每个容器都有独立的网络命名空间

**创建依据**：
- **容器 ID**：每个容器都有唯一的 ID
- **容器配置**：`c.config.CreateNetNS` 决定是否创建命名空间
- **网络模式**：bridge 模式会创建新的命名空间

```go
// libpod/container_internal_linux.go
if c.config.CreateNetNS && noNetNS && !c.config.PostConfigureNetNS {
    // 基于容器 ID 创建命名空间
    netNS, networkStatus, createNetNSErr = c.runtime.createNetNS(c)
    // netNS 路径：/var/run/netns/{container-id}
}
```

### 2. 命名空间路径的生成

```go
// go.podman.io/common/pkg/netns
func NewNS() (*NS, error) {
    // 生成唯一路径
    path := filepath.Join("/var/run/netns", generateID())
    
    // 创建网络命名空间
    fd, err := unix.Open("/proc/self/ns/net", unix.O_RDONLY, 0)
    // ...
    
    // 创建 bind mount 使命名空间持久化
    err = unix.Mount(path, path, "none", unix.MS_BIND, "")
    
    return &NS{path: path}, nil
}
```

**路径格式**：
- 新创建：`/var/run/netns/{random-id}` 或 `/var/run/netns/{container-id}`
- 运行中：`/proc/{pid}/ns/net`

### 3. Compose 项目中的网络隔离

在 Compose 项目中：
- **每个服务（容器）**都有独立的网络命名空间
- **同一网络的服务**通过 bridge 网络连接
- **DNS 解析**通过 aardvark-dns 实现服务名到 IP 的映射

**示例**：
```yaml
# docker-compose.yml
services:
  web:
    image: nginx
    networks:
      - app-network
  db:
    image: postgres
    networks:
      - app-network

networks:
  app-network:
    driver: bridge
```

**流程**：
1. `web` 容器创建 → 命名空间 `/var/run/netns/{web-container-id}`
2. `db` 容器创建 → 命名空间 `/var/run/netns/{db-container-id}`
3. 两个容器都加入 `app-network` bridge 网络
4. Netavark 为每个容器配置网络，并更新 aardvark-dns 配置
5. Aardvark-dns 提供 DNS 解析：`web` → `10.88.x.x`，`db` → `10.88.y.y`

## 关键代码位置

### Podman 端

1. **Compose 命令**：
   - `cmd/podman/compose.go:composeMain()` - 执行外部 compose 工具
   - `cmd/podman/compose.go:composeDockerHost()` - 设置 DOCKER_HOST

2. **命名空间创建**：
   - `libpod/container_internal_linux.go:init()` - 容器初始化
   - `libpod/networking_linux.go:createNetNS()` - 创建网络命名空间

3. **调用 Netavark**：
   - `libpod/networking_linux.go:configureNetNS()` - 配置网络
   - `libpod/networking_common.go:setUpNetwork()` - 调用网络后端
   - `vendor/go.podman.io/common/libnetwork/netavark/run.go:Setup()` - Netavark setup

### Netavark 端

1. **接收和处理**：
   - `src/commands/setup.rs:Setup::exec()` - 处理 setup 命令
   - `src/network/core_utils.rs:open_netlink_sockets()` - 打开命名空间

2. **DNS 服务**：
   - `src/dns/aardvark.rs:Aardvark::commit_netavark_entries()` - 提交 DNS 条目
   - `src/dns/aardvark.rs:Aardvark::start_aardvark_server()` - 启动 aardvark-dns

## 总结

### 命名空间创建依据

1. **容器 ID**：每个容器都有唯一 ID，用于生成命名空间路径
2. **容器配置**：`CreateNetNS` 标志决定是否创建命名空间
3. **网络模式**：bridge 模式会创建新的命名空间
4. **路径格式**：
   - 创建时：`/var/run/netns/{container-id}`（bind mount）
   - 运行时：`/proc/{pid}/ns/net`

### DNS 服务调用流程

1. **外部工具**（docker-compose/podman-compose）通过 Podman API 创建容器
2. **Podman** 创建网络命名空间（基于容器 ID）
3. **Podman** 调用 `netavark setup {namespacePath}`，传入命名空间路径
4. **Netavark** 配置网络，如果网络启用 DNS：
   - 收集 DNS 条目（容器名、IP、网络名）
   - 写入配置到 `/run/containers/networks/aardvark-dns/{network-name}`
   - 启动或通知 aardvark-dns（通过 systemd-run）
5. **Aardvark-dns** 提供 DNS 解析服务

### 关键特点

- ✅ **每个容器独立命名空间**：基于容器 ID 创建
- ✅ **命名空间路径作为标识**：传递给 netavark 的唯一标识
- ✅ **DNS 服务共享**：同一网络的所有容器共享一个 aardvark-dns 实例
- ✅ **自动管理**：Netavark 自动启动和管理 aardvark-dns
