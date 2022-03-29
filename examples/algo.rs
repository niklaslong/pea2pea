mod common;

use bytes::{Bytes, BytesMut};
use futures_util::{sink::SinkExt, stream::TryStreamExt, StreamExt};
use tokio_util::codec::{BytesCodec, Decoder, Encoder, Framed};
use tracing::*;
use tracing_subscriber::filter::LevelFilter;

use pea2pea::{
    protocols::{Handshake, Reading, Writing},
    Connection, ConnectionSide, Node, Pea2Pea,
};

use std::{io, net::SocketAddr};

pub struct AlgoCodec(websocket_codec::MessageCodec);

impl Default for AlgoCodec {
    fn default() -> Self {
        // websocket_codec uses `true` for the client and `false` for the server
        AlgoCodec(websocket_codec::MessageCodec::with_masked_encode(true))
    }
}

impl Decoder for AlgoCodec {
    type Item = websocket_codec::Message;
    type Error = io::Error;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        self.0
            .decode(src)
            .map_err(|_| io::ErrorKind::InvalidData.into())
    }
}

impl Encoder<Vec<u8>> for AlgoCodec {
    type Error = io::Error;

    fn encode(&mut self, item: Vec<u8>, dst: &mut BytesMut) -> Result<(), Self::Error> {
        let message = websocket_codec::Message::binary(item);
        self.0
            .encode(message, dst)
            .map_err(|_| io::ErrorKind::InvalidData.into())
    }
}

#[derive(Clone)]
struct AlgoNode {
    node: Node,
}

impl Pea2Pea for AlgoNode {
    fn node(&self) -> &Node {
        &self.node
    }
}

#[async_trait::async_trait]
impl Handshake for AlgoNode {
    async fn perform_handshake(&self, mut conn: Connection) -> io::Result<Connection> {
        let conn_addr = conn.addr();
        let node_conn_side = !conn.side();
        let stream = self.borrow_stream(&mut conn);

        match node_conn_side {
            ConnectionSide::Initiator => {
                let mut framed = Framed::new(stream, BytesCodec::default());

                let mut request = Vec::new();
                request.extend_from_slice(b"GET /v1/mainnet-v1.0/gossip HTTP/1.1\r\n");
                request.extend_from_slice(format!("Host: {}\r\n", conn_addr).as_bytes());
                request.extend_from_slice(
                    b"User-Agent: algod/3.8 (dev; commit=d867a094+; 0) linux(amd64)\r\n",
                );
                request.extend_from_slice(b"Connection: Upgrade\r\n");
                request.extend_from_slice(b"Sec-WebSocket-Key: nSYwkZhPRAatKmzjOM+6LQ==\r\n"); // TODO: un-hard-code
                request.extend_from_slice(b"Sec-WebSocket-Version: 13\r\n");
                request.extend_from_slice(b"Upgrade: websocket\r\n");
                request.extend_from_slice(b"X-Algorand-Accept-Version: 2.1\r\n");
                request.extend_from_slice(b"X-Algorand-Instancename: pea2pea\r\n");
                request.extend_from_slice(b"X-Algorand-Location: \r\n");
                request.extend_from_slice(b"X-Algorand-Noderandom: cGVhMnBlYQ==\r\n");
                // request.extend_from_slice(b"X-Algorand-Telid: d12c01a5-4ca4-4be3-a394-68c8913f3883\r\n");
                request.extend_from_slice(b"X-Algorand-Version: 2.1\r\n");
                request.extend_from_slice(b"X-Algorand-Genesis: mainnet-v1.0\r\n");
                request.extend_from_slice(b"\r\n");
                let request = Bytes::from(request);

                trace!(parent: self.node().span(), "sending handshake message: {:?}", request);

                framed.send(request).await.unwrap();

                let response = framed.try_next().await.unwrap().unwrap();

                trace!(parent: self.node().span(), "received handshake message: {:?}", response);
            }
            ConnectionSide::Responder => {
                let mut framed = Framed::new(stream, BytesCodec::default());

                let request = framed.next().await.unwrap().unwrap();

                trace!(parent: self.node().span(), "received handshake message: {:?}", request);

                let mut request_headers = [httparse::EMPTY_HEADER; 32];
                let mut parsed_request = httparse::Request::new(&mut request_headers);
                parsed_request.parse(&request).unwrap();

                let swa = if let Some(swk) = parsed_request
                    .headers
                    .iter()
                    .find(|h| h.name.to_ascii_lowercase() == "sec-websocket-key")
                {
                    tungstenite::handshake::derive_accept_key(swk.value)
                } else {
                    error!(parent: self.node().span(), "missing Sec-WebSocket-Key!");
                    return Err(io::ErrorKind::InvalidData.into());
                };

                let mut response = Vec::new();
                response.extend_from_slice(b"HTTP/1.1 101 Switching Protocols\r\n");
                response.extend_from_slice(b"Upgrade: websocket\r\n");
                response.extend_from_slice(b"Connection: Upgrade\r\n");
                response.extend_from_slice(format!("Sec-Websocket-Accept: {}\r\n", swa).as_bytes());
                // response.extend_from_slice(b"X-Algorand-Prioritychallenge: el5HGDoXCG63yBDDgpGnpp0mbbuzq+ed/gBvi10wUCs=\r\n");
                // response.extend_from_slice(b"X-Algorand-Telid: d12c01a5-4ca4-4be3-a394-68c8913f3883\r\n");
                response.extend_from_slice(b"X-Algorand-Instancename: pea2pea\r\n");
                response.extend_from_slice(b"X-Algorand-Location: \r\n"); // http://<listener addr>
                response.extend_from_slice(b"X-Algorand-Noderandom: cGVhMnBlYQ==\r\n");
                response.extend_from_slice(b"X-Algorand-Version: 2.1\r\n");
                response.extend_from_slice(b"X-Algorand-Genesis: mainnet-v1.0\r\n");
                response.extend_from_slice(b"\r\n");
                let response = Bytes::from(response);

                trace!(parent: self.node().span(), "sending handshake message: {:?}", response);

                framed.send(response).await.unwrap();
            }
        }

        Ok(conn)
    }
}

#[async_trait::async_trait]
impl Reading for AlgoNode {
    type Message = websocket_codec::Message;
    type Codec = AlgoCodec;

    fn codec(&self, _addr: SocketAddr, _side: ConnectionSide) -> Self::Codec {
        Default::default()
    }

    async fn process_message(&self, source: SocketAddr, message: Self::Message) -> io::Result<()> {
        info!(parent: self.node().span(), "got a message from {}: {:?}", source, message);

        Ok(())
    }
}

impl Writing for AlgoNode {
    type Message = Vec<u8>;
    type Codec = AlgoCodec;

    fn codec(&self, _addr: SocketAddr, _side: ConnectionSide) -> Self::Codec {
        Default::default()
    }
}

#[tokio::main]
async fn main() {
    common::start_logger(LevelFilter::TRACE);

    let config = pea2pea::Config {
        listener_ip: Some("127.0.0.1".parse().unwrap()),
        desired_listening_port: Some(10000),
        allow_random_port: false,
        ..Default::default()
    };

    let algo_node = AlgoNode {
        node: Node::new(config).await.unwrap(),
    };
    algo_node.enable_handshake().await;
    algo_node.enable_reading().await;
    algo_node.enable_writing().await;

    // algo_node.node().connect("127.0.0.1:4132".parse().unwrap()).await.unwrap();

    std::future::pending::<()>().await;
}
