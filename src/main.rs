mod router;
mod gateway;

use std::env;
use crate::gateway::GatewayServiceImpl;
use crate::router::Router;
use anyhow::Result;
use tonic::transport::Server;

pub mod gateway_proto {
    tonic::include_proto!("gateway");
}
use crate::gateway_proto::gateway_service_server::GatewayServiceServer;

async fn wait() {
    tokio::signal::ctrl_c().await.ok();
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 初始化日志
    tracing_subscriber::fmt::init();

    // 创建路由
    let router = Router::new();

    // 创建网关服务
    let gateway_service = GatewayServiceImpl::new(router);

    // 服务地址
    let addr = env::var("ADDR").unwrap_or("127.0.0.1:50051".to_string()).parse()?;

    tracing::info!("gRPC Gateway starting on {}", addr);

    // 构建服务
    let server = Server::builder().add_service(GatewayServiceServer::new(gateway_service));

    let graceful  = server.serve_with_shutdown(addr, wait());

    graceful.await?;

    tracing::info!("shutdown complete");

    Ok(())
}