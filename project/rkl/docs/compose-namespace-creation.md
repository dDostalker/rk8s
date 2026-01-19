# RKL Compose 显式创建网络命名空间实现

## 概述

本文档说明如何让 rkl 的 compose 功能像 Podman 一样显式创建网络命名空间，而不是依赖容器运行时自动创建。

## 容器 ID 存储位置

rkl 的容器 ID 存储在以下位置：

1. **状态文件路径**：`{root_path}/{container_id}/state.json`
   - 默认 `root_path` 是 `/run/youki`（通过 `rootpath::determine()` 确定）
   - 对于 compose 模式：`/run/youki/compose/{project_name}/{container_id}/state.json`
   - 对于单容器模式：`/run/youki/{container_id}/state.json`

2. **State 结构体**：容器状态（包括 ID）存储在 `libcontainer::container::State` 结构体中
   - `state.id`：容器 ID
   - `state.pid`：容器进程 PID
   - `state.status`：容器状态

## 实现方案

### 1. 添加命名空间路径字段

在 `ContainerRunner` 结构体中添加 `netns_path` 字段：

```rust
pub struct ContainerRunner {
    // ... 其他字段
    /// 显式创建的网络命名空间路径（类似 Podman 的方式）
    /// 如果为 Some，表示在容器创建前已创建命名空间
    netns_path: Option<String>,
}
```

### 2. 创建命名空间方法

添加 `create_network_namespace()` 方法，在容器创建前显式创建命名空间：

```rust
pub fn create_network_namespace(&mut self) -> Result<()> {
    let netns_name = self.container_id.clone();
    let _netns = Netns::new_named(&netns_name)?;
    let netns_path = format!("/var/run/netns/{}", netns_name);
    self.netns_path = Some(netns_path);
    info!("Created network namespace for container {} at {}", 
         self.container_id, self.netns_path.as_ref().unwrap());
    Ok(())
}
```

### 3. 修改 OCI Spec 生成

在 `create_oci_spec()` 中，如果已创建命名空间，则在 OCI spec 中指定使用该命名空间路径：

```rust
// 如果已创建命名空间，使用它；否则使用默认命名空间配置
let namespaces = if let Some(ref netns_path) = self.netns_path {
    // 使用已创建的命名空间路径
    let mut ns = get_default_namespaces();
    // 找到 Network 命名空间并设置路径
    for ns_item in &mut ns {
        if ns_item.typ() == oci_spec::runtime::LinuxNamespaceType::Network {
            ns_item.set_path(Some(netns_path.clone()));
            break;
        }
    }
    ns
} else {
    // 使用默认命名空间配置（让运行时自动创建）
    get_default_namespaces()
};
```

### 4. 修改网络设置流程

在 `setup_container_network()` 中，如果已创建命名空间，使用命名空间路径而不是 `/proc/{pid}/ns/net`：

```rust
// 如果已创建命名空间，使用命名空间路径；否则使用容器的 PID
let (container_id, netns_path) = if let Some(ref netns_path) = self.netns_path {
    // 使用显式创建的命名空间路径（类似 Podman）
    (self.container_id.clone(), netns_path.clone())
} else {
    // 使用容器的 PID（原有方式）
    let container_pid = self.get_container_state()?.pid
        .ok_or_else(|| anyhow!("get container {} pid failed", self.container_id))?;
    (format!("{container_pid}"), format!("/proc/{container_pid}/ns/net"))
};

cni.setup(container_id, netns_path)?;
```

### 5. 在 Compose 模式中启用

在 `run()` 方法中，检测是否为 compose 模式，如果是则在容器创建前创建命名空间：

```rust
pub fn run(&mut self) -> Result<()> {
    self.build_config()?;

    // 对于 compose 模式，在容器创建前创建网络命名空间
    let is_compose_mode = self.root_path.to_string_lossy().contains("compose");
    if is_compose_mode {
        self.create_network_namespace()?;
    }

    // ... 创建和启动容器
}
```

### 6. 清理命名空间

添加 `remove_container_network_by_id()` 函数，在删除容器时清理命名空间：

```rust
pub fn remove_container_network_by_id(container_id: &str) -> Result<()> {
    let netns_path = format!("/var/run/netns/{}", container_id);
    if Path::new(&netns_path).exists() {
        // 使用命名空间路径删除 CNI 网络
        let mut cni = get_cni()?;
        cni.load_default_conf();
        cni.remove(container_id.to_string(), netns_path.clone())?;
        
        // 删除命名空间文件
        Netns::delete_named(container_id)?;
    }
    Ok(())
}
```

## 与 Podman 的对比

### Podman 的方式

1. **创建命名空间**：在容器创建前，使用 `netns.NewNS()` 显式创建
2. **持久化**：通过 bind mount 到 `/var/run/netns/{container-id}`
3. **OCI Spec**：在 spec 中指定命名空间路径
4. **CNI 调用**：使用命名空间路径调用 CNI setup

### RKL 修改后的方式

1. **创建命名空间**：在 compose 模式下，容器创建前使用 `Netns::new_named()` 显式创建
2. **持久化**：通过 bind mount 到 `/var/run/netns/{container_id}`
3. **OCI Spec**：在 spec 中指定命名空间路径
4. **CNI 调用**：使用命名空间路径调用 CNI setup

## 关键代码位置

1. **命名空间创建**：`rk8s/project/rkl/src/commands/container/mod.rs::create_network_namespace()`
2. **OCI Spec 生成**：`rk8s/project/rkl/src/commands/container/mod.rs::create_oci_spec()`
3. **网络设置**：`rk8s/project/rkl/src/commands/container/mod.rs::setup_container_network()`
4. **命名空间清理**：`rk8s/project/rkl/src/commands/container/mod.rs::remove_container_network_by_id()`

## 使用方式

修改后，rkl compose 会自动在创建容器前创建网络命名空间，无需额外配置：

```bash
rkl compose up -f compose.yml
```

命名空间会：
- 在容器创建前创建
- 存储在 `/var/run/netns/{container_id}`
- 在容器删除时自动清理

## 优势

1. **与 Podman 一致**：使用相同的命名空间创建方式
2. **更好的控制**：在容器创建前就拥有命名空间，可以提前配置
3. **持久化**：命名空间通过 bind mount 持久化，即使容器停止也存在
4. **兼容性**：单容器模式仍使用原有方式（通过 PID），保持向后兼容
