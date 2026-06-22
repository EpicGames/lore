// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use async_trait::async_trait;
pub use rs_consul::Config;
pub use rs_consul::Consul;
use rs_consul::ConsulError;
use rs_consul::GetServiceNodesRequest;
use rs_consul::QueryOptions;
use rs_consul::ResponseMeta;
use rs_consul::ServiceNode;

use crate::consul::ConsulClient;

#[derive(Debug)]
pub struct RsConsul(Consul);

impl From<rs_consul::Consul> for RsConsul {
    fn from(value: rs_consul::Consul) -> Self {
        Self(value)
    }
}

#[async_trait]
impl ConsulClient for RsConsul {
    async fn get_service_nodes<'a>(
        &self,
        request: GetServiceNodesRequest<'a>,
        query_opts: Option<QueryOptions>,
    ) -> Result<ResponseMeta<Vec<ServiceNode>>, ConsulError> {
        self.0.get_service_nodes(request, query_opts).await
    }
}
