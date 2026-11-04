// Adapted from cloudflare/workers-rs examples/emscripten-incoming-tcp,
// copyright Cloudflare, Inc., Apache-2.0. This diagnostic is not a Lore server.
use std::sync::atomic::{AtomicBool, Ordering};

use bytes::{Buf, Bytes, BytesMut};
use tokio::net::{TcpListener, TcpStream};
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::tokio::schedule_isolated;
use worker::{Env, Socket, console_error, worker_sys};

fn main() {}

const PORT: u16 = 7000;
static LISTENING: AtomicBool = AtomicBool::new(false);

#[wasm_bindgen(experimental_tokio)]
pub async fn connect(socket: worker_sys::Socket, _env: Env, _ctx: JsValue) -> Result<(), JsValue> {
    if !LISTENING.swap(true, Ordering::Relaxed) {
        let listener = match TcpListener::bind(("0.0.0.0", PORT)).await {
            Ok(listener) => listener,
            Err(error) => {
                LISTENING.store(false, Ordering::Relaxed);
                return Err(js_sys::Error::new(&error.to_string()).into());
            }
        };
        tokio::spawn(accept_loop(listener));
    }
    Socket::from(socket)
        .handle_as_node_connection()
        .await
        .map_err(|error| js_sys::Error::new(&error.to_string()).into())
}

// The ambient runtime owns the listener; each accepted connection gets the
// isolated I/O context belonging to its own connect invocation.
async fn accept_loop(listener: TcpListener) {
    loop {
        match listener.accept().await {
            Ok((stream, peer)) => {
                let stream = match stream.into_std() {
                    Ok(stream) => stream,
                    Err(error) => {
                        console_error!("{peer}: {error}");
                        continue;
                    }
                };
                schedule_isolated(
                    async move { serve(TcpStream::from_std(stream)?).await },
                    move |result| match result {
                        Ok(Err(error)) => console_error!("{peer}: {error}"),
                        Err(error) => console_error!("{peer}: task failed: {error}"),
                        Ok(Ok(())) => {}
                    },
                );
            }
            Err(error) => console_error!("accept failed: {error}"),
        }
    }
}

async fn serve(stream: TcpStream) -> Result<(), Box<dyn std::error::Error>> {
    let mut connection = h2::server::handshake(stream).await?;
    while let Some(request) = connection.accept().await {
        let (request, respond) = request?;
        tokio::spawn(async move {
            if let Err(error) = rpc(request, respond).await {
                console_error!("rpc: {error}");
            }
        });
    }
    Ok(())
}

fn frame(body: &[u8]) -> Bytes {
    let mut result = Vec::with_capacity(body.len() + 5);
    result.push(0);
    result.extend_from_slice(&(body.len() as u32).to_be_bytes());
    result.extend_from_slice(body);
    result.into()
}

async fn rpc(
    request: http::Request<h2::RecvStream>,
    mut respond: h2::server::SendResponse<Bytes>,
) -> Result<(), Box<dyn std::error::Error>> {
    let path = request.uri().path().to_owned();
    let response = http::Response::builder()
        .status(200)
        .header("content-type", "application/grpc")
        .body(())?;
    let mut output = respond.send_response(response, false)?;
    let mut input = request.into_body();
    let status = match path.as_str() {
        "/lore.environment.v1.EnvironmentService/EnvironmentGet" => {
            output.send_data(frame(&[0x0a, 0x00]), false)?;
            "0"
        }
        "/lore.storage.v1.StorageService/Get" => {
            let mut pending = BytesMut::new();
            while let Some(data) = input.data().await {
                let data = data?;
                input.flow_control().release_capacity(data.len())?;
                pending.extend_from_slice(&data);
                while pending.len() >= 5 {
                    let size = u32::from_be_bytes(pending[1..5].try_into()?) as usize;
                    if pending[0] != 0 || size > 120 {
                        return Err("invalid diagnostic frame".into());
                    }
                    if pending.len() < size + 5 {
                        break;
                    }
                    pending.advance(5);
                    let address = pending.split_to(size);
                    // GetResponse { address, status: { code: NOT_FOUND } }.
                    let mut response = vec![0x0a, size as u8];
                    response.extend_from_slice(&address);
                    response.extend_from_slice(&[0x22, 2, 0x08, 5]);
                    output.send_data(frame(&response), false)?;
                }
            }
            if !pending.is_empty() {
                return Err("truncated diagnostic frame".into());
            }
            "0"
        }
        _ => "12",
    };
    let mut trailers = http::HeaderMap::new();
    trailers.insert("grpc-status", status.parse()?);
    output.send_trailers(trailers)?;
    Ok(())
}
