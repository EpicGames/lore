// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
// A transport experiment, not an implementation of the Lore server.
use futures_util::{StreamExt, stream};
use prost::Message;
use worker::{ByteStream, Context, Env, Error, Method, Request, Response, Result, event};

#[allow(dead_code)]
mod lore {
    pub mod environment {
        pub mod v1 {
            include!(concat!(env!("OUT_DIR"), "/lore.environment.v1.rs"));
        }
    }
    pub mod model {
        pub mod v1 {
            include!(concat!(env!("OUT_DIR"), "/lore.model.v1.rs"));
        }
    }
    pub mod storage {
        pub mod v1 {
            include!(concat!(env!("OUT_DIR"), "/lore.storage.v1.rs"));
        }
    }
}

const MAX_MESSAGE: usize = 64 * 1024;

fn frame(message: impl Message) -> Vec<u8> {
    let body = message.encode_to_vec();
    let mut result = Vec::with_capacity(5 + body.len());
    result.push(0);
    result.extend_from_slice(&(body.len() as u32).to_be_bytes());
    result.extend_from_slice(&body);
    result
}

fn grpc_response(mut response: Response) -> Result<Response> {
    response
        .headers_mut()
        .set("content-type", "application/grpc")?;
    Ok(response)
}

struct Incoming {
    stream: ByteStream,
    pending: Vec<u8>,
}

impl Incoming {
    async fn next_address(&mut self) -> Result<Option<lore::model::v1::Address>> {
        loop {
            if self.pending.len() >= 5 {
                if self.pending[0] != 0 {
                    return Err(Error::RustError(
                        "compressed gRPC message unsupported".into(),
                    ));
                }
                let len = u32::from_be_bytes(self.pending[1..5].try_into().unwrap()) as usize;
                if len > MAX_MESSAGE {
                    return Err(Error::RustError("gRPC message too large".into()));
                }
                if self.pending.len() >= len + 5 {
                    let address = lore::model::v1::Address::decode(&self.pending[5..len + 5])
                        .map_err(|err| Error::RustError(err.to_string()))?;
                    self.pending.drain(..len + 5);
                    return Ok(Some(address));
                }
            }
            match self.stream.next().await {
                Some(chunk) => {
                    self.pending.extend_from_slice(&chunk?);
                    if self.pending.len() > MAX_MESSAGE + 5 {
                        return Err(Error::RustError("gRPC buffer too large".into()));
                    }
                }
                None if self.pending.is_empty() => return Ok(None),
                None => return Err(Error::RustError("truncated gRPC frame".into())),
            }
        }
    }
}

#[event(fetch)]
async fn main(mut req: Request, _env: Env, _ctx: Context) -> Result<Response> {
    if req.method() != Method::Post {
        return Response::error("transport spike: POST required", 405);
    }
    match req.path().as_str() {
        "/lore.environment.v1.EnvironmentService/EnvironmentGet" => {
            let message = lore::environment::v1::EnvironmentGetResponse {
                environment: Some(Default::default()),
            };
            // Deliberately use an initial header here: the experiment must determine
            // whether it reaches the client and whether real trailers are possible.
            let mut response = grpc_response(Response::from_bytes(frame(message))?)?;
            response.headers_mut().set("grpc-status", "0")?;
            Ok(response)
        }
        "/lore.storage.v1.StorageService/Get" => {
            let incoming = Incoming {
                stream: req.stream()?,
                pending: Vec::new(),
            };
            let replies = stream::try_unfold(incoming, |mut incoming| async move {
                let Some(address) = incoming.next_address().await? else {
                    return Ok::<_, Error>(None);
                };
                let message = lore::storage::v1::GetResponse {
                    address: Some(address),
                    fragment: None,
                    payload: Vec::new(),
                    status: Some(lore::model::v1::ItemStatus {
                        code: 5,
                        message: "transport spike: no data".into(),
                    }),
                };
                Ok(Some((frame(message), incoming)))
            });
            // No terminal status is fabricated: Workers' documented Response API
            // does not expose trailers. The probe treats their absence as a failure.
            grpc_response(Response::from_stream(replies)?)
        }
        _ => {
            let mut response = grpc_response(Response::empty()?)?;
            response.headers_mut().set("grpc-status", "12")?;
            Ok(response)
        }
    }
}
