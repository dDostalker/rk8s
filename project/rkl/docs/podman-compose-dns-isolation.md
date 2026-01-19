# Podman Compose 项目之间的 DNS 隔离机制

## 核心问题

**不同的 compose 项目之间能否通过 DNS 直接访问对方？**

**答案：取决于网络配置，默认情况下是隔离的，但可以通过共享网络实现互通。**

## DNS 配置的组织方式

### 1. 按网络名称组织

Aardvark-dns 的配置是按**网络名称**（network name）组织的：

```
/run/containers/networks/aardvark-dns/
├── {network-name-1}      # 网络 1 的 DNS 配置
├── {network-name-2}      # 网络 2 的 DNS 配置
└── {network-name-3}      # 网络 3 的 DNS 配置
```

每个网络配置文件包含：
- 第一行：网关 IP 和网络 DNS 服务器
- 后续行：该网络中所有容器的 DNS 条目（容器 ID、IP、名称等）

### 2. 配置文件格式

```rust
// netavark/src/dns/aardvark.rs
pub fn commit_entries(&self, entries: &[AardvarkEntry]) -> NetavarkResult<()> {
    for entry in entries {
        // 每个网络有自己的配置文件
        let mut path = Path::new(&self.config).join(entry.network_name);
        
        // 写入配置：网关、DNS 服务器、容器条目
        // 格式：ID ipv4s ipv6s names [dns-servers]
    }
}
```

**配置文件示例**：
```
10.88.0.1 8.8.8.8
abc123def456 10.88.0.2  web,nginx
def789ghi012 10.88.0.3  db,postgres
```

## Compose 项目的网络命名

### 1. Docker Compose 的默认行为

Docker Compose 会为每个项目创建默认网络：

```yaml
# 项目 A: docker-compose.yml
services:
  web:
    image: nginx

# 默认网络名称：{project-name}_default
# 例如：myproject_default
```

**网络名称生成规则**：
- 默认网络：`{project-name}_default`
- 自定义网络：`{project-name}_{network-name}`
- `project-name` 通常是目录名或通过 `-p` 参数指定

### 2. 示例场景

**场景 1：两个独立的 compose 项目**

```bash
# 项目 A
cd /path/to/project-a
docker-compose up -d
# 网络名称：project-a_default

# 项目 B
cd /path/to/project-b
docker-compose up -d
# 网络名称：project-b_default
```

**结果**：
- ✅ **DNS 隔离**：两个项目使用不同的网络名称
- ✅ **网络隔离**：不同的 bridge 网络，IP 段不同
- ❌ **无法通过 DNS 访问**：项目 A 的容器无法通过 DNS 解析项目 B 的服务名

**场景 2：使用相同的项目名称**

```bash
# 项目 A
cd /path/to/project-a
docker-compose -p myproject up -d
# 网络名称：myproject_default

# 项目 B
cd /path/to/project-b
docker-compose -p myproject up -d
# 网络名称：myproject_default（相同！）
```

**结果**：
- ⚠️ **网络名称冲突**：两个项目使用相同的网络名称
- ⚠️ **可能共享网络**：取决于 Podman 的网络管理方式
- ⚠️ **DNS 可能共享**：如果共享网络，DNS 也会共享

**场景 3：显式共享网络**

```yaml
# 项目 A: docker-compose.yml
services:
  web:
    image: nginx
    networks:
      - shared-network

networks:
  shared-network:
    external: true
    name: shared-network

# 项目 B: docker-compose.yml
services:
  api:
    image: node
    networks:
      - shared-network

networks:
  shared-network:
    external: true
    name: shared-network
```

**结果**：
- ✅ **共享网络**：两个项目使用同一个网络
- ✅ **DNS 共享**：可以互相通过服务名访问
- ✅ **网络互通**：在同一个 bridge 网络中

## Aardvark-DNS 的解析机制

### 1. 单例进程

Aardvark-dns 是一个**单例进程**，管理所有网络的 DNS：

```rust
// netavark/src/dns/aardvark.rs
pub fn start_aardvark_server(&self) -> NetavarkResult<()> {
    // 启动单个 aardvark-dns 进程
    // 监听所有网络的 DNS 查询
}
```

### 2. 配置文件读取

Aardvark-dns 会读取**所有网络**的配置文件：

```
/run/containers/networks/aardvark-dns/
├── project-a_default      # 项目 A 的网络
├── project-b_default      # 项目 B 的网络
└── shared-network         # 共享网络
```

### 3. DNS 解析范围

**关键问题**：Aardvark-dns 在解析 DNS 查询时，是否会跨网络解析？

**答案**：取决于 aardvark-dns 的实现，但通常：

1. **同一网络内**：可以解析同一网络中所有容器的服务名
2. **跨网络**：默认情况下，**不能**跨网络解析（网络隔离）
3. **共享网络**：如果两个项目共享网络，可以互相解析

### 4. 网络隔离机制

即使 aardvark-dns 读取了所有网络的配置，**网络层面的隔离**仍然存在：

- **不同的 bridge 网络**：不同的 IP 段，路由隔离
- **防火墙规则**：nftables/iptables 规则限制跨网络通信
- **DNS 解析**：即使能解析到 IP，网络层面也可能无法通信

## 实际测试场景

### 测试 1：两个独立的 compose 项目

```bash
# 项目 A
mkdir project-a && cd project-a
cat > docker-compose.yml <<EOF
services:
  web:
    image: nginx
    container_name: web-a
EOF
docker-compose up -d

# 项目 B
mkdir project-b && cd project-b
cat > docker-compose.yml <<EOF
services:
  api:
    image: node
    container_name: api-b
EOF
docker-compose up -d

# 测试 DNS 解析
docker exec web-a nslookup api-b
# 结果：无法解析（不同的网络）
```

### 测试 2：共享网络

```bash
# 创建共享网络
podman network create shared-network

# 项目 A
cat > docker-compose.yml <<EOF
services:
  web:
    image: nginx
    networks:
      - shared

networks:
  shared:
    external: true
    name: shared-network
EOF

# 项目 B
cat > docker-compose.yml <<EOF
services:
  api:
    image: node
    networks:
      - shared

networks:
  shared:
    external: true
    name: shared-network
EOF

# 测试 DNS 解析
docker exec web-a nslookup api-b
# 结果：可以解析（共享网络）
```

## 总结

### 默认情况：DNS 隔离

1. **不同的 compose 项目**使用**不同的网络名称**
2. **不同的网络**有**不同的 bridge**和**IP 段**
3. **DNS 配置**按网络名称分开存储
4. **无法跨网络通过 DNS 访问**

### 实现互通的方法

1. **共享网络**：
   ```yaml
   networks:
     shared:
       external: true
       name: shared-network
   ```

2. **使用相同的项目名称**（不推荐，可能冲突）

3. **手动创建网络并指定名称**：
   ```bash
   podman network create my-shared-network
   # 然后在两个项目中都使用这个网络
   ```

### 关键代码位置

1. **网络配置存储**：
   - `netavark/src/dns/aardvark.rs:commit_entries()` - 按网络名称存储配置

2. **网络名称生成**：
   - Docker Compose 根据项目名称生成网络名称
   - 格式：`{project-name}_{network-name}`

3. **DNS 解析**：
   - Aardvark-dns 读取所有网络配置
   - 但网络层面的隔离限制跨网络访问

### 最佳实践

1. ✅ **使用不同的项目名称**：避免网络名称冲突
2. ✅ **需要互通时显式共享网络**：使用 `external: true`
3. ✅ **理解网络隔离**：默认情况下，不同项目是隔离的
4. ⚠️ **避免使用相同项目名称**：可能导致意外共享

## 结论

**默认情况下，不同的 compose 项目之间无法通过 DNS 直接访问对方**，因为：

1. 使用不同的网络名称
2. 不同的 bridge 网络
3. 网络层面的隔离

**如果需要互通，必须显式共享网络**。
Netavark 的容器信息来自 Podman，通过 JSON 配置传递。

  信息传递流程


  1. Podman 构建容器信息


     1 │// libpod/networking_common.go
     2 │func (c *Container) getNetworkOptions(networkOpts map[string]types.PerNetworkOptions) 
       │types.NetworkOptions {
     3 │    opts := types.NetworkOptions{
     4 │        ContainerID:       c.config.ID,              // 从容器配置获取
     5 │        ContainerName:     getNetworkPodName(c),     // 从容器配置获取
     6 │        ContainerHostname: c.NetworkHostname(),      // 从容器配置获取
     7 │        DNSServers:        nameservers,              // 从容器配置获取
     8 │        PortMappings:      c.convertPortMappings(),  // 从容器配置获取
     9 │        Networks:          networkOpts,              // 从容器配置获取
    10 │    }
    11 │    return opts
    12 │}


  2. Podman 通过 JSON 传递给 Netavark


     1 │// vendor/go.podman.io/common/libnetwork/netavark/exec.go
     2 │func (n *netavarkNetwork) execNetavark(args []string, needPlugin bool, stdin, result any) error {
     3 │    // stdin 包含 NetworkOptions（容器信息）
     4 │    err = json.NewEncoder(stdinW).Encode(stdin)  // 将容器信息编码为 JSON
     5 │    // ...
     6 │}


  3. Netavark 从 stdin 读取


     1 │// netavark/src/commands/setup.rs
     2 │pub fn exec(&self, input_file: Option<OsString>, ...) -> NetavarkResult<()> {
     3 │    // 从 stdin 或文件读取 JSON 配置
     4 │    let network_options = network::types::NetworkOptions::load(input_file)?;
     5 │    // network_options 包含：
     6 │    // - container_id
     7 │    // - container_name
     8 │    // - container_hostname
     9 │    // - dns_servers
    10 │    // - port_mappings
    11 │    // - networks
    12 │}


  容器信息来源

  • ContainerID: c.config.ID（容器 ID）
  • ContainerName: getNetworkPodName(c)（容器名或 Pod 名）
  • ContainerHostname: c.NetworkHostname()（容器主机名）
  • DNSServers: c.config.DNSServer（DNS 服务器列表）
  • PortMappings: c.config.PortMappings（端口映射）
  • Networks: c.networks()（容器要加入的网络）


  总结

  • Netavark 不主动收集容器信息
  • 信息由 Podman 从容器配置中提取
  • 通过 JSON 从 stdin 传递给 netavark
  • Netavark 解析 JSON 并使用这些信息配置网络和 DNS
