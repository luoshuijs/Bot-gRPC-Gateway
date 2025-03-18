use dashmap::DashMap;
use std::sync::Arc;
use tokio::task::JoinHandle;
use tonic::Status;
use tonic::transport::Channel;

pub mod gateway_proto {
    tonic::include_proto!("gateway");
}

use crate::gateway_proto::{
    HealthCheckRequest, Message, RegisterResponse, ServiceInfo,
    gateway_service_client::GatewayServiceClient,
};

// 路由表：存储已注册的服务及其支持的命令
#[derive(Debug, Clone)]
pub(crate) struct Router {
    // 服务ID -> 服务信息
    pub(crate) services: Arc<DashMap<String, ServiceInfo>>,
    // 命令 -> 服务ID列表（一个命令可能被多个服务支持）
    cmd_routes: Arc<DashMap<String, Vec<String>>>,
    // 活跃的客户端连接
    clients: Arc<DashMap<String, GatewayServiceClient<Channel>>>,
    // 服务健康状态 (服务ID -> 是否健康)
    pub(crate) health_status: Arc<DashMap<String, bool>>,
    // 路由策略配置
    routing_strategy: Arc<DashMap<String, RoutingStrategy>>,
    // 默认路由策略
    default_strategy: RoutingStrategy,
    // 健康检查任务句柄 (服务ID -> JoinHandle)
    health_check_handles: Arc<DashMap<String, JoinHandle<()>>>,
}

#[derive(Clone, Debug)]
pub(crate) enum RoutingStrategy {
    RoundRobin,     // 轮询策略
    FirstAvailable, // 选择第一个
    Random,         // 随机选择目标
}

impl Router {
    pub(crate) fn new() -> Self {
        Router {
            services: Arc::new(DashMap::new()),
            cmd_routes: Arc::new(DashMap::new()),
            clients: Arc::new(DashMap::new()),
            health_status: Arc::new(DashMap::new()),
            routing_strategy: Arc::new(DashMap::new()),
            default_strategy: RoutingStrategy::RoundRobin,
            health_check_handles: Arc::new(DashMap::new()),
        }
    }

    // 注册新服务
    pub(crate) async fn register_service(
        &self,
        info: ServiceInfo,
    ) -> anyhow::Result<RegisterResponse> {
        let service_id = info.service_id.clone();

        // 如果已存在这个服务的健康检查，先取消它
        if let Some((_, handle)) = self.health_check_handles.remove(&service_id) {
            handle.abort();
            tracing::info!(
                "Cancelled previous health check for service: {}",
                service_id
            );
        }

        // 存储服务信息
        self.services.insert(service_id.clone(), info.clone());

        // 更新命令路由表
        for cmd in &info.commands {
            let mut routes = self.cmd_routes.entry(cmd.clone()).or_default();
            if !routes.contains(&service_id) {
                routes.push(service_id.clone());
            }
        }

        // 连接到新注册的服务
        match self.connect_to_service(&info).await {
            Ok(client) => {
                self.clients.insert(service_id.clone(), client);

                // 初始化健康状态为 true (假设刚注册的服务是健康的)
                self.health_status.insert(service_id.clone(), true);

                // 如果服务配置了健康检查间隔，启动健康检查
                if info.health_check_interval_sec > 0 {
                    self.schedule_health_check(
                        service_id.clone(),
                        info.health_check_interval_sec as u64,
                    )
                    .await;
                }

                Ok(RegisterResponse {
                    success: true,
                    message: format!("Service {} registered successfully", info.service_name),
                })
            }
            Err(e) => Ok(RegisterResponse {
                success: false,
                message: format!("Failed to connect to service: {}", e),
            }),
        }
    }

    // 连接到服务端点
    async fn connect_to_service(
        &self,
        info: &ServiceInfo,
    ) -> anyhow::Result<GatewayServiceClient<Channel>> {
        let addr = format!("http://{}", info.service_addr);
        let client = GatewayServiceClient::connect(addr).await?;
        Ok(client)
    }

    // 根据消息内容确定路由
    fn determine_route(&self, msg: &Message) -> Option<String> {
        // 如果指定了目标服务，直接路由到该服务
        if !msg.target_service.is_empty() {
            return Some(msg.target_service.clone());
        }

        // 获取该命令的路由策略
        let strategy = self
            .routing_strategy
            .get(&msg.cmd)
            .map(|s| s.clone())
            .unwrap_or_else(|| self.default_strategy.clone());

        // 根据命令查找支持的服务
        if let Some(services) = self.cmd_routes.get(&msg.cmd) {
            if !services.is_empty() {
                // 过滤掉不健康的服务
                let healthy_services: Vec<String> = services
                    .iter()
                    .filter(|service_id| {
                        self.health_status
                            .get(service_id.as_str())
                            .map(|status| *status)
                            .unwrap_or(false)
                    })
                    .cloned()
                    .collect();

                if healthy_services.is_empty() {
                    // 如果没有健康的服务，记录警告并尝试使用任何可用服务
                    tracing::warn!("No healthy services found for command: {}", msg.cmd);
                    if !services.is_empty() {
                        return Some(services[0].clone());
                    }
                } else {
                    // 基于策略选择服务
                    return match strategy {
                        RoutingStrategy::FirstAvailable => Some(healthy_services[0].clone()),
                        RoutingStrategy::Random => {
                            let idx = rand::random_range(0..healthy_services.len());
                            Some(healthy_services[idx].clone())
                        }
                        RoutingStrategy::RoundRobin => {
                            // todo: 跟踪上次使用的索引
                            Some(healthy_services[0].clone())
                        }
                    };
                }
            }
        }

        None
    }

    // 转发消息到目标服务
    pub(crate) async fn forward_message(
        &self,
        mut msg: Message,
    ) -> anyhow::Result<Message, Status> {
        // 确定路由
        let target_service = match self.determine_route(&msg) {
            Some(service) => service,
            None => return Err(Status::not_found("No service found for the given command")),
        };

        // 获取客户端连接
        let mut client = match self.clients.get(&target_service) {
            Some(client) => client.clone(),
            None => return Err(Status::unavailable("Service connection not available")),
        };

        // 设置实际的目标服务
        msg.target_service = target_service.clone();

        // 检查服务健康状态
        if let Some(is_healthy) = self.health_status.get(&target_service) {
            if !*is_healthy {
                // 如果服务不健康，记录警告但仍尝试转发
                tracing::warn!("Forwarding to unhealthy service: {}", target_service);
            }
        }

        // 转发请求
        match client.forward(msg).await {
            Ok(response) => Ok(response.into_inner()),
            Err(status) => {
                // 如果转发失败，可能服务不健康，更新健康状态
                if status.code() == tonic::Code::Unavailable {
                    tracing::error!("Service {} unavailable: {}", target_service, status);
                    self.health_status.insert(target_service.clone(), false);
                }
                Err(status)
            }
        }
    }

    //  执行服务健康检查
    async fn check_service_health(&self, service_id: &str) -> bool {
        tracing::debug!("Performing health check for service: {}", service_id);

        let mut client = match self.clients.get(service_id) {
            Some(client) => client.clone(),
            None => {
                tracing::warn!("Health check failed: Service {} not found", service_id);
                return false;
            }
        };

        let request = tonic::Request::new(HealthCheckRequest {
            service_id: service_id.to_string(),
        });

        match client.health_check(request).await {
            Ok(response) => {
                let status = response.into_inner().status;
                let is_healthy = status == 1;

                if is_healthy {
                    tracing::debug!("Service {} is healthy", service_id);
                } else {
                    tracing::warn!("Service {} is not healthy, status: {}", service_id, status);
                }

                is_healthy
            }
            Err(e) => {
                tracing::error!(
                    "Health check request failed for service {}: {}",
                    service_id,
                    e
                );
                false
            }
        }
    }

    // 尝试重新连接到服务
    async fn reconnect_service(&self, service_id: &str, info: &ServiceInfo) -> anyhow::Result<()> {
        match self.connect_to_service(info).await {
            Ok(client) => {
                self.clients.insert(service_id.to_string(), client);
                Ok(())
            }
            Err(e) => Err(anyhow::anyhow!("Failed to reconnect: {}", e)),
        }
    }

    // 安排定期健康检查
    async fn schedule_health_check(&self, service_id: String, interval_sec: u64) {
        let health_status = self.health_status.clone();
        let self_clone = self.clone();
        let service_id_clone = service_id.clone();

        // 创建新的健康检查任务
        let handle = tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(interval_sec));

            tracing::info!(
                "Started health check for service {} every {} seconds",
                service_id,
                interval_sec
            );

            loop {
                interval.tick().await;

                // 执行健康检查
                let is_healthy = self_clone.check_service_health(&service_id).await;

                // 更新健康状态
                health_status.insert(service_id.clone(), is_healthy);
            }
        });

        // 存储任务句柄，以便后续可以中止
        self.health_check_handles.insert(service_id_clone, handle);
    }

    // 启动所有服务的健康检查
    pub(crate) async fn start_health_checks(&self) {
        // 获取所有已注册服务
        let services: Vec<(String, i32)> = self
            .services
            .iter()
            .map(|entry| (entry.key().clone(), entry.value().health_check_interval_sec))
            .collect();

        // 为每个服务启动健康检查
        for (service_id, interval_sec) in services {
            // 先取消可能已存在的健康检查任务
            if let Some((_, handle)) = self.health_check_handles.remove(&service_id) {
                handle.abort();
            }

            if interval_sec > 0 {
                self.schedule_health_check(service_id, interval_sec as u64)
                    .await;
            }
        }
    }
}
