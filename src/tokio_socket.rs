use async_trait::async_trait;
use serde_json::Value;
use std::collections::HashMap;
use std::fmt::Debug;
use std::io::Error;
use std::path::Path;
use std::sync::{Arc, Mutex};
use tokio::io::BufReader;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::UnixStream;
use tokio::sync::mpsc;
use tokio::sync::mpsc::{Receiver, Sender};
use uuid::Uuid;

use crate::actions::SocketWrapper;
use crate::errors::SignaldError;
use crate::socket::AsyncSocket;
use crate::types::{ClientMessageWrapperV1, IncomingMessageV1};

pub enum SocketError {
    General(&'static str),
    Io(Error),
    Channel(&'static str),
    Signald(SignaldError),
}

impl Debug for SocketError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SocketError::General(desc) => write!(f, "Error: {}", desc),
            SocketError::Io(e) => write!(f, "Error: {}", e),
            SocketError::Channel(e) => write!(f, "Error: {}", e),
            SocketError::Signald(e) => write!(f, "Signald error: {}", e.error.message),
        }
    }
}

impl From<Error> for SocketError {
    fn from(e: Error) -> Self {
        SocketError::Io(e)
    }
}

pub type Map = Arc<Mutex<HashMap<Uuid, (Sender<Value>, Option<Receiver<Value>>)>>>;

pub struct Socket<T> {
    socket: T,
    response_map: Map,
    listening: Arc<Mutex<bool>>,

    pub subscriber: Receiver<IncomingMessageV1>,
}

#[async_trait]
impl AsyncSocket for Socket<OwnedWriteHalf> {
    async fn write<'a>(&'a mut self, buf: &'a [u8], id: &Uuid) -> Result<(), SocketError> {
        let channel = mpsc::channel(1);
        self.response_map
            .lock()
            .unwrap()
            .insert(*id, (channel.0, Some(channel.1)));

        self.socket.write_all(buf).await?;
        Ok(())
    }

    async fn get_response<'a>(&'a mut self, id: Uuid) -> Result<Value, SocketError> {
        let mut receiver = match self.response_map.lock().unwrap().get_mut(&id) {
            Some(channel) => channel.1.take().unwrap(),
            None => {
                return Err(SocketError::General("Error: Incorrect response ID"));
            }
        };

        receiver
            .recv()
            .await
            .ok_or(SocketError::Channel("Failed to receive response"))
    }
}

impl Socket<OwnedWriteHalf> {
    pub async fn connect<P: AsRef<Path>>(path: P) -> Result<Self, SocketError> {
        let (reader, writer) = UnixStream::connect(path).await?.into_split();
        let response_map = Arc::new(Mutex::new(HashMap::new()));
        let listening = Arc::new(Mutex::new(true));

        let (subscriber_tx, subscriber_rx) = mpsc::channel(32);

        let socket_wrapper = Socket {
            socket: writer,
            response_map: response_map.clone(),
            listening: listening.clone(),
            subscriber: subscriber_rx,
        };

        tokio::task::spawn(async move {
            listen(reader, response_map, listening, subscriber_tx).await;
        });

        Ok(socket_wrapper)
    }
}

async fn listen(
    socket: OwnedReadHalf,
    map: Map,
    listening: Arc<Mutex<bool>>,
    subscriber_tx: Sender<IncomingMessageV1>,
) {
    let mut reader = BufReader::new(socket);
    let mut buf = String::with_capacity(1024);

    while *listening.lock().unwrap() {
        match reader.read_line(&mut buf).await {
            Ok(_) => {
                let response: Value = serde_json::from_str(buf.as_str()).unwrap();

                if let Some(id) = response.get("id") {
                    let id = Uuid::parse_str(id.as_str().unwrap()).unwrap();
                    let sender = map.lock().unwrap().get(&id).unwrap().0.clone();
                    match sender.send(response).await {
                        Ok(_) => {
                            println!("sent response!");
                        }
                        Err(e) => println!("Error sending response: {}", e),
                    }
                } else if let Some("IncomingMessage") = response.get("type").unwrap().as_str() {
                    let msg: IncomingMessageV1 =
                        serde_json::from_value(response.get("data").unwrap().clone()).unwrap();
                    subscriber_tx.send(msg).await.unwrap();
                }
            }
            Err(e) => {
                println!("Error: {}", e.to_string());
            }
        }

        buf.clear();
    }
}

pub type Signald = SocketWrapper<Socket<OwnedWriteHalf>>;

impl Signald {
    pub async fn connect<P: AsRef<Path>>(path: P) -> Result<Self, SocketError> {
        Ok(Signald {
            socket: Socket::connect(path).await?,
        })
    }
}
