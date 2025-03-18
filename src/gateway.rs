use anyhow::Result;
use dashmap::DashMap;
use futures::Stream;
use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_stream::{StreamExt, wrappers::ReceiverStream};
use tonic::{Request, Response, Status};

use crate::router::Router;

// 引入生成的 protobuf 代码
pub mod gateway_proto {
    tonic::include_proto!("gateway");
}

use crate::gateway_proto::{
    HealthCheckRequest, HealthCheckResponse, Message, RegisterResponse, ServiceInfo,
    gateway_service_server::GatewayService,
};

// 网关服务实现
#[derive(Debug)]
pub(crate) struct GatewayServiceImpl {
    router: Router,
    // 存储活跃的流连接
    streams: Arc<DashMap<String, mpsc::Sender<Result<Message, Status>>>>,
}

impl GatewayServiceImpl {
    pub(crate) fn new(router: Router) -> Self {
        GatewayServiceImpl {
            router,
            streams: Arc::new(DashMap::new()),
        }
    }
}

#[tonic::async_trait]
impl GatewayService for GatewayServiceImpl {
    // 单次请求-响应转发

    async fn forward(&self, request: Request<Message>) -> Result<Response<Message>, Status> {
        let msg = request.into_inner();

        tracing::debug!(
            "Received forward request from: {}, cmd: {}, user: {}",
            msg.source_service,
            msg.cmd,
            msg.user_id
        );

        // 解析元数据（如需要）
        if !msg.metadata.is_empty() {
            match serde_json::from_str::<HashMap<String, serde_json::Value>>(&msg.metadata) {
                Ok(metadata) => {
                    tracing::debug!("Request metadata: {:?}", metadata);
                }
                Err(e) => {
                    tracing::warn!("Failed to parse metadata: {}", e);
                }
            }
        }

        // 转发消息
        match self.router.forward_message(msg).await {
            Ok(response) => Ok(Response::new(response)),
            Err(status) => Err(status),
        }
    }

    // 服务注册
    async fn register(
        &self,
        request: Request<ServiceInfo>,
    ) -> Result<Response<RegisterResponse>, Status> {
        let info = request.into_inner();

        tracing::info!(
            "Registering service: {} ({}), addr: {}",
            info.service_name,
            info.service_id,
            info.service_addr
        );
        tracing::debug!("Supported commands: {:?}", info.commands);

        match self.router.register_service(info).await {
            Ok(response) => Ok(Response::new(response)),
            Err(e) => {
                tracing::error!("Registration failed: {}", e);
                Ok(Response::new(RegisterResponse {
                    success: false,
                    message: format!("Registration failed: {}", e),
                }))
            }
        }
    }

    // 双向流式通信
    type StreamStream = Pin<Box<dyn Stream<Item = Result<Message, Status>> + Send + 'static>>;

    async fn stream(
        &self,
        request: Request<tonic::Streaming<Message>>,
    ) -> Result<Response<Self::StreamStream>, Status> {
        let mut in_stream = request.into_inner();
        let (tx, rx) = mpsc::channel(128);
        let streams = self.streams.clone();
        let router = self.router.clone();

        // 为流生成唯一ID
        let stream_id = uuid::Uuid::new_v4().to_string();

        // 储存发送端以便其他流可以向此流发送消息
        streams.insert(stream_id.clone(), tx.clone());

        // 处理接收到的消息
        tokio::spawn(async move {
            while let Some(result) = in_stream.next().await {
                match result {
                    Ok(msg) => {
                        tracing::debug!(
                            "Stream message from {}: cmd={}",
                            msg.source_service,
                            msg.cmd
                        );

                        // 根据消息内容确定路由并转发
                        match router.forward_message(msg).await {
                            Ok(response) => {
                                if let Err(e) = tx.send(Ok(response)).await {
                                    tracing::error!("Failed to send response: {}", e);
                                    break;
                                }
                            }
                            Err(status) => {
                                if let Err(e) = tx.send(Err(status)).await {
                                    tracing::error!("Failed to send error: {}", e);
                                    break;
                                }
                            }
                        }
                    }
                    Err(e) => {
                        tracing::error!("Error receiving message: {}", e);
                        break;
                    }
                }
            }

            // 流结束，清理资源
            streams.remove(&stream_id);
            tracing::debug!("Stream {} closed", stream_id);
        });

        // 返回接收流
        Ok(Response::new(
            Box::pin(ReceiverStream::new(rx)) as Self::StreamStream
        ))
    }

    // 健康检查接口实现
    async fn health_check(
        &self,
        request: Request<HealthCheckRequest>,
    ) -> Result<Response<HealthCheckResponse>, Status> {
        let req = request.into_inner();
        let service_id = req.service_id;

        tracing::debug!("Received health check request for service: {}", service_id);

        // 检查服务是否存在
        if !self.router.services.contains_key(&service_id) {
            return Ok(Response::new(HealthCheckResponse {
                status: 3, // SERVICE_UNKNOWN
                message: format!("Service {} not registered", service_id),
            }));
        }

        // 获取服务健康状态
        let is_healthy = self
            .router
            .health_status
            .get(&service_id)
            .map(|status| *status)
            .unwrap_or(false);

        // 构造响应
        let status = if is_healthy { 1 } else { 2 }; // 1=SERVING, 2=NOT_SERVING
        let message = if is_healthy {
            format!("Service {} is healthy", service_id)
        } else {
            format!("Service {} is not healthy", service_id)
        };

        Ok(Response::new(HealthCheckResponse { status, message }))
    }
}
