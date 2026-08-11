//! Docker Engine API 窄适配层（仅 DockerDevVmm 使用）。
//!
//! 只暴露开发后端需要的操作：镜像拉取、网络创建/检查、容器 create/start/
//! pause/resume/stop/kill/remove、archive 上传、状态检查。测试用 fake 实现。

use async_trait::async_trait;

use clouisle_core::ClouisleError;

/// 容器状态子集。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContainerState {
    Created,
    Running,
    Paused,
    Exited(i64),
    Dead,
}

/// 容器创建参数（DockerDevVmm 映射后的窄面）。
#[derive(Debug, Clone)]
pub struct DevContainerOpts {
    pub name: String,
    pub image: String,
    pub entrypoint: Vec<String>,
    pub labels: Vec<(String, String)>,
    /// NanoCPUs
    pub nano_cpus: Option<i64>,
    /// 内存字节
    pub memory_bytes: Option<i64>,
    pub pids_limit: Option<i64>,
    /// 附加网络（管理网络必选）
    pub networks: Vec<String>,
    /// 只读挂载：{宿主路径 -> 容器路径}
    pub readonly_mounts: Vec<(String, String)>,
}

#[async_trait]
pub trait DockerEngine: Send + Sync {
    async fn pull_image(&self, reference: &str) -> Result<(), ClouisleError>;
    async fn ensure_network(&self, name: &str, internal: bool) -> Result<(), ClouisleError>;
    async fn create_container(&self, opts: &DevContainerOpts) -> Result<(), ClouisleError>;
    /// 向容器内上传 tar 归档（注入静态 agent）。
    async fn upload_archive(
        &self,
        container: &str,
        path: &str,
        tar_bytes: Vec<u8>,
    ) -> Result<(), ClouisleError>;
    async fn start(&self, container: &str) -> Result<(), ClouisleError>;
    async fn pause(&self, container: &str) -> Result<(), ClouisleError>;
    async fn unpause(&self, container: &str) -> Result<(), ClouisleError>;
    async fn stop(&self, container: &str) -> Result<(), ClouisleError>;
    async fn kill(&self, container: &str) -> Result<(), ClouisleError>;
    async fn remove(&self, container: &str) -> Result<(), ClouisleError>;
    async fn inspect_state(&self, container: &str) -> Result<ContainerState, ClouisleError>;
    /// 容器是否缺失（清理幂等判断）。
    async fn container_exists(&self, container: &str) -> Result<bool, ClouisleError>;
}

/// 生产实现：Unix socket 连接 Docker Engine。
pub struct BollardDockerEngine {
    docker: bollard::Docker,
}

impl BollardDockerEngine {
    pub async fn connect() -> Result<Self, ClouisleError> {
        let docker = bollard::Docker::connect_with_unix_defaults()
            .map_err(|e| ClouisleError::io(format!("connect docker engine: {e}")))?;
        Ok(Self { docker })
    }
}

#[async_trait]
impl DockerEngine for BollardDockerEngine {
    async fn pull_image(&self, reference: &str) -> Result<(), ClouisleError> {
        use bollard::image::CreateImageOptions;
        use futures::StreamExt;
        let options = CreateImageOptions {
            from_image: reference.to_string(),
            from_src: String::new(),
            ..Default::default()
        };
        let mut stream = self.docker.create_image(Some(options), None, None);
        while let Some(item) = stream.next().await {
            if let Err(e) = item {
                return Err(ClouisleError::io(format!("docker pull {reference}: {e}")));
            }
        }
        Ok(())
    }

    async fn ensure_network(&self, name: &str, internal: bool) -> Result<(), ClouisleError> {
        use bollard::network::CreateNetworkOptions;
        match self.docker.inspect_network::<&str>(name, None).await {
            Ok(_) => Ok(()),
            Err(bollard::errors::Error::DockerResponseServerError {
                status_code: 404, ..
            }) => {
                let options = CreateNetworkOptions {
                    name: name.to_string(),
                    driver: "bridge".to_string(),
                    internal,
                    ..Default::default()
                };
                self.docker
                    .create_network(options)
                    .await
                    .map_err(|e| ClouisleError::io(format!("create network {name}: {e}")))?;
                Ok(())
            }
            Err(e) => Err(ClouisleError::io(format!("inspect network {name}: {e}"))),
        }
    }

    async fn create_container(&self, opts: &DevContainerOpts) -> Result<(), ClouisleError> {
        use bollard::container::Config;
        use bollard::container::NetworkingConfig;
        use bollard::models::EndpointSettings;
        use bollard::models::HostConfig;

        let mut config = Config {
            image: Some(opts.image.clone()),
            entrypoint: Some(opts.entrypoint.clone()),
            labels: Some(opts.labels.iter().cloned().collect()),
            ..Default::default()
        };
        let mut binds = None;
        if !opts.readonly_mounts.is_empty() {
            binds = Some(
                opts.readonly_mounts
                    .iter()
                    .map(|(src, dst)| format!("{src}:{dst}:ro"))
                    .collect(),
            );
        }
        config.host_config = Some(HostConfig {
            nano_cpus: opts.nano_cpus,
            memory: opts.memory_bytes,
            pids_limit: opts.pids_limit,
            binds,
            ..Default::default()
        });
        if !opts.networks.is_empty() {
            let mut endpoints = std::collections::HashMap::new();
            for name in &opts.networks {
                endpoints.insert(name.clone(), EndpointSettings::default());
            }
            config.networking_config = Some(NetworkingConfig::<String> {
                endpoints_config: endpoints,
            });
        }
        let options = bollard::container::CreateContainerOptions {
            name: opts.name.clone(),
            ..Default::default()
        };
        self.docker
            .create_container(Some(options), config)
            .await
            .map_err(|e| ClouisleError::io(format!("docker create {}: {e}", opts.name)))?;
        Ok(())
    }

    async fn upload_archive(
        &self,
        container: &str,
        path: &str,
        tar_bytes: Vec<u8>,
    ) -> Result<(), ClouisleError> {
        use bollard::container::UploadToContainerOptions;
        let options = UploadToContainerOptions {
            path: path.to_string(),
            ..Default::default()
        };
        self.docker
            .upload_to_container(container, Some(options), bytes::Bytes::from(tar_bytes))
            .await
            .map_err(|e| ClouisleError::io(format!("docker upload to {container}: {e}")))?;
        Ok(())
    }

    async fn start(&self, container: &str) -> Result<(), ClouisleError> {
        self.docker
            .start_container::<String>(container, None)
            .await
            .map_err(|e| ClouisleError::io(format!("docker start {container}: {e}")))?;
        Ok(())
    }

    async fn pause(&self, container: &str) -> Result<(), ClouisleError> {
        self.docker
            .pause_container(container)
            .await
            .map_err(|e| ClouisleError::io(format!("docker pause {container}: {e}")))?;
        Ok(())
    }

    async fn unpause(&self, container: &str) -> Result<(), ClouisleError> {
        self.docker
            .unpause_container(container)
            .await
            .map_err(|e| ClouisleError::io(format!("docker unpause {container}: {e}")))?;
        Ok(())
    }

    async fn stop(&self, container: &str) -> Result<(), ClouisleError> {
        self.docker
            .stop_container(container, None)
            .await
            .map_err(|e| ClouisleError::io(format!("docker stop {container}: {e}")))?;
        Ok(())
    }

    async fn kill(&self, container: &str) -> Result<(), ClouisleError> {
        self.docker
            .kill_container::<String>(container, None)
            .await
            .map_err(|e| ClouisleError::io(format!("docker kill {container}: {e}")))?;
        Ok(())
    }

    async fn remove(&self, container: &str) -> Result<(), ClouisleError> {
        use bollard::container::RemoveContainerOptions;
        let options = RemoveContainerOptions {
            force: true,
            v: true,
            ..Default::default()
        };
        self.docker
            .remove_container(container, Some(options))
            .await
            .map_err(|e| ClouisleError::io(format!("docker remove {container}: {e}")))?;
        Ok(())
    }

    async fn inspect_state(&self, container: &str) -> Result<ContainerState, ClouisleError> {
        let info = self
            .docker
            .inspect_container(container, None)
            .await
            .map_err(|e| ClouisleError::io(format!("docker inspect {container}: {e}")))?;
        let state = info.state.as_ref().ok_or_else(|| {
            ClouisleError::invalid_state(format!("docker container {container} has no state"))
        })?;
        let code = state.exit_code.unwrap_or(0);
        Ok(match state.status {
            Some(bollard::models::ContainerStateStatusEnum::CREATED) => ContainerState::Created,
            Some(bollard::models::ContainerStateStatusEnum::RUNNING) => ContainerState::Running,
            Some(bollard::models::ContainerStateStatusEnum::PAUSED) => ContainerState::Paused,
            Some(bollard::models::ContainerStateStatusEnum::EXITED)
            | Some(bollard::models::ContainerStateStatusEnum::DEAD) => ContainerState::Exited(code),
            _ => ContainerState::Dead,
        })
    }

    async fn container_exists(&self, container: &str) -> Result<bool, ClouisleError> {
        match self.docker.inspect_container(container, None).await {
            Ok(_) => Ok(true),
            Err(bollard::errors::Error::DockerResponseServerError {
                status_code: 404, ..
            }) => Ok(false),
            Err(e) => Err(ClouisleError::io(format!(
                "docker inspect {container}: {e}"
            ))),
        }
    }
}
