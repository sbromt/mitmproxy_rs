use std::net::{IpAddr, SocketAddr};
use std::str::FromStr;
use std::sync::Arc;

use anyhow::{anyhow, Result};

use boringtun::crypto::{X25519PublicKey, X25519SecretKey};

use pyo3::exceptions::{PyKeyError, PyOSError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyString, PyTuple};

use tokio::net::UdpSocket;
use tokio::sync::mpsc::{self, channel, error::SendError, unbounded_channel};
use tokio::sync::oneshot::{self, error::RecvError};
use tokio::sync::Notify;
use tokio::task::JoinHandle;

mod messages;
mod tcp;
mod virtual_device;
mod wireguard;

use messages::{ConnectionId, TransportCommand, TransportEvent};

/// An individual TCP stream with an API similar to `asyncio.StreamReader`/`asyncio.StreamWriter`.
#[pyclass]
struct TcpStream {
    connection_id: ConnectionId,
    event_tx: mpsc::UnboundedSender<TransportCommand>,
    peername: SocketAddr,
    sockname: SocketAddr,
    original_dst: SocketAddr,
}

#[pymethods]
impl TcpStream {
    fn read<'p>(&self, py: Python<'p>, n: u32) -> PyResult<&'p PyAny> {
        let (tx, rx) = oneshot::channel();
        self.event_tx
            .send(TransportCommand::ReadData(self.connection_id, n, tx))
            .map_err(event_queue_unavailable)?;
        pyo3_asyncio::tokio::future_into_py(py, async move {
            let data = rx.await.map_err(connection_closed)?;
            let bytes: Py<PyBytes> = Python::with_gil(|py| PyBytes::new(py, &data).into_py(py));
            Ok(bytes)
        })
    }

    fn write(&self, data: Vec<u8>) -> PyResult<()> {
        self.event_tx
            .send(TransportCommand::WriteData(self.connection_id, data))
            .map_err(event_queue_unavailable)?;
        Ok(())
    }

    fn drain<'p>(&self, py: Python<'p>) -> PyResult<&'p PyAny> {
        let (tx, rx) = oneshot::channel();
        self.event_tx
            .send(TransportCommand::DrainWriter(self.connection_id, tx))
            .map_err(event_queue_unavailable)?;
        pyo3_asyncio::tokio::future_into_py(py, async move {
            rx.await.map_err(connection_closed)?;
            Ok(())
        })
    }

    fn write_eof(&self) -> PyResult<()> {
        self.event_tx
            .send(TransportCommand::CloseConnection(self.connection_id, true))
            .map_err(event_queue_unavailable)?;
        Ok(())
    }

    fn close(&self) -> PyResult<()> {
        self.event_tx
            .send(TransportCommand::CloseConnection(self.connection_id, false))
            .map_err(event_queue_unavailable)?;
        Ok(())
    }

    /// Supported values: peername, sockname, original_dst.
    fn get_extra_info(&self, py: Python, name: String) -> PyResult<PyObject> {
        match name.as_str() {
            "peername" => Ok(socketaddr_to_py(py, self.peername)),
            "sockname" => Ok(socketaddr_to_py(py, self.sockname)),
            "original_dst" => Ok(socketaddr_to_py(py, self.original_dst)),
            _ => Err(PyKeyError::new_err(name)),
        }
    }

    fn __repr__(&self) -> String {
        format!(
            "TcpStream({}, peer={}, sock={}, dst={})",
            self.connection_id, self.peername, self.sockname, self.original_dst,
        )
    }
}

impl Drop for TcpStream {
    fn drop(&mut self) {
        self.close().ok();
    }
}

fn socketaddr_to_py(py: Python, s: SocketAddr) -> PyObject {
    match s {
        SocketAddr::V4(addr) => (addr.ip().to_string(), addr.port()).into_py(py),
        SocketAddr::V6(addr) => {
            log::debug!(
                "converting ipv6 to python, not sure if this is correct: {:?}",
                (addr.ip().to_string(), addr.port())
            );
            (addr.ip().to_string(), addr.port()).into_py(py)
        },
    }
}

fn py_to_socketaddr(t: &PyTuple) -> PyResult<SocketAddr> {
    if t.len() == 2 {
        let host = t.get_item(0)?.downcast::<PyString>()?;
        let port: u16 = t.get_item(1)?.extract()?;
        let addr = IpAddr::from_str(host.to_str()?)?;
        Ok(SocketAddr::from((addr, port)))
    } else {
        Err(PyValueError::new_err("not a socket address"))
    }
}

fn event_queue_unavailable(_: SendError<TransportCommand>) -> PyErr {
    PyOSError::new_err("WireGuard server has been shut down.")
}

fn connection_closed(_: RecvError) -> PyErr {
    PyOSError::new_err("connection closed")
}

#[pyclass]
struct WireguardServer {
    event_tx: mpsc::UnboundedSender<TransportCommand>,
    local_addr: SocketAddr,
    python_notify_task: JoinHandle<()>,
    wg_stopper: Arc<Notify>,
    tcp_stopper: Arc<Notify>,
}

#[pymethods]
impl WireguardServer {
    /// Send an individual UDP datagram using the specified source and destination addresses.
    fn send_datagram(&self, data: Vec<u8>, src_addr: &PyTuple, dst_addr: &PyTuple) -> PyResult<()> {
        self.event_tx
            .send(TransportCommand::SendDatagram {
                data,
                src_addr: py_to_socketaddr(src_addr)?,
                dst_addr: py_to_socketaddr(dst_addr)?,
            })
            .map_err(event_queue_unavailable)?;
        Ok(())
    }

    /// Terminate the WireGuard server.
    fn stop(&self) {
        self.wg_stopper.notify_one();
        self.tcp_stopper.notify_one();

        self.python_notify_task.abort();
    }

    /// Get the local address the WireGuard server is listening on.
    fn getsockname(&self, py: Python) -> PyObject {
        socketaddr_to_py(py, self.local_addr)
    }

    fn __repr__(&self) -> String {
        format!("WireguardServer({})", self.local_addr)
    }
}

impl WireguardServer {
    pub async fn init(
        host: String,
        port: u16,
        private_key: String,
        peer_public_keys: Vec<(String, Option<[u8; 32]>)>,
        py_tcp_handler: PyObject,
        py_udp_handler: PyObject,
    ) -> Result<WireguardServer> {
        let private_key: Arc<X25519SecretKey> = Arc::new(private_key.parse().map_err(|error: &str| anyhow!(error))?);

        // configure WireGuard peers
        let peers = peer_public_keys
            .into_iter()
            .map(|(peer_public_key, preshared_key)| {
                let key = Arc::new(X25519PublicKey::from_str(&peer_public_key).map_err(|error: &str| anyhow!(error))?);
                Ok((key, preshared_key))
            })
            .collect::<Result<Vec<(Arc<X25519PublicKey>, Option<[u8; 32]>)>>>()?;

        let (py_loop, run_coroutine_threadsafe) = Python::with_gil(|py| -> PyResult<(PyObject, PyObject)> {
            Ok((
                pyo3_asyncio::tokio::get_current_loop(py)?.into(),
                py.import("asyncio")?.getattr("run_coroutine_threadsafe")?.into(),
            ))
        })?;

        // initialize channels between the WireGuard server and the virtual network device
        let (wg_to_smol_tx, wg_to_smol_rx) = channel(16);
        let (smol_to_wg_tx, smol_to_wg_rx) = channel(16);

        // initialize channels between the virtual network device and the python interop task
        // - only used to notify of incoming connections and datagrams
        let (smol_to_py_tx, mut smol_to_py_rx) = channel(64);
        // - used to send data and to ask for packets
        // This channel needs to be unbounded because write() is not async.
        let (py_to_smol_tx, py_to_smol_rx) = unbounded_channel();

        // bind to UDP socket
        let socket = UdpSocket::bind((host, port)).await?;
        let local_addr = socket.local_addr()?;

        // initialize WireGuard server
        let mut wg_server_builder = wireguard::WireguardServerBuilder::new(private_key, wg_to_smol_tx, smol_to_wg_rx);
        for (peer_public_key, preshared_key) in peers {
            wg_server_builder.add_peer(peer_public_key, preshared_key)?;
        }
        let wg_server = wg_server_builder.build()?;
        let wg_stopper = wg_server.stopper();

        // initialize virtual network device
        let tcp_server = tcp::TcpServer::new(smol_to_wg_tx, wg_to_smol_rx, smol_to_py_tx, py_to_smol_rx)?;
        let tcp_stopper = tcp_server.stopper();

        // spawn tasks
        let _wg_handle = tokio::spawn(async move { wg_server.run(socket).await });
        let _tcp_handle = tokio::spawn(async move { tcp_server.run().await });

        let event_tx = py_to_smol_tx.clone();
        // this task feeds events into the Python callback.
        let python_notify_task = tokio::spawn(async move {
            while let Some(event) = smol_to_py_rx.recv().await {
                match event {
                    TransportEvent::ConnectionEstablished {
                        connection_id,
                        src_addr,
                        dst_addr,
                    } => {
                        let stream = TcpStream {
                            connection_id,
                            event_tx: event_tx.clone(),
                            peername: src_addr,
                            sockname: local_addr,
                            original_dst: dst_addr,
                        };
                        Python::with_gil(|py| {
                            let stream = stream.into_py(py);
                            let coro = match py_tcp_handler.call1(py, (stream.clone_ref(py), stream)) {
                                Ok(coro) => coro,
                                Err(err) => {
                                    err.print(py);
                                    return;
                                },
                            };
                            if let Err(err) = run_coroutine_threadsafe.call1(py, (coro, py_loop.as_ref(py))) {
                                err.print(py);
                            }
                        });
                    },
                    TransportEvent::DatagramReceived {
                        data,
                        src_addr,
                        dst_addr,
                    } => {
                        Python::with_gil(|py| {
                            let bytes: Py<PyBytes> = PyBytes::new(py, &data).into_py(py);
                            if let Err(err) = py_loop.call_method1(
                                py,
                                "call_soon_threadsafe",
                                (
                                    py_udp_handler.as_ref(py),
                                    bytes,
                                    socketaddr_to_py(py, src_addr),
                                    socketaddr_to_py(py, dst_addr),
                                ),
                            ) {
                                err.print(py);
                            }
                        });
                    },
                }
            }
        });

        Ok(WireguardServer {
            event_tx: py_to_smol_tx,
            local_addr,
            python_notify_task,
            wg_stopper,
            tcp_stopper,
        })
    }
}

impl Drop for WireguardServer {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Start a WireGuard server.
#[pyfunction]
fn start_server(
    py: Python<'_>,
    host: String,
    port: u16,
    private_key: String,
    peer_public_keys: Vec<(String, Option<[u8; 32]>)>,
    handle_connection: PyObject,
    receive_datagram: PyObject,
) -> PyResult<&PyAny> {
    pyo3_asyncio::tokio::future_into_py(py, async move {
        // XXX: This is a bit of a race condition: the  handler could be called before
        // .server = await start_server() has assigned to .server.
        let server = WireguardServer::init(
            host,
            port,
            private_key,
            peer_public_keys,
            handle_connection,
            receive_datagram,
        )
        .await?;
        Ok(server)
    })
}

/// Generate a WireGuard private key and return its base64-encoded representation.
#[pyfunction]
fn genkey() -> String {
    base64::encode(X25519SecretKey::new().as_bytes())
}

/// Return the base64-encoded public key for the passed private key.
#[pyfunction]
fn pubkey(private_key: String) -> PyResult<String> {
    let private_key: X25519SecretKey = private_key
        .parse()
        .map_err(|_| PyValueError::new_err("Invalid private key."))?;
    Ok(base64::encode(private_key.public_key().as_bytes()))
}

#[pymodule]
fn mitmproxy_wireguard(_py: Python, m: &PyModule) -> PyResult<()> {
    // set up the Rust logger to send messages to the Python logger
    pyo3_log::init();

    // set up tracing subscriber for introspection with tokio-console
    #[cfg(debug_assertions)]
    console_subscriber::init();

    m.add_function(wrap_pyfunction!(start_server, m)?)?;
    m.add_function(wrap_pyfunction!(genkey, m)?)?;
    m.add_function(wrap_pyfunction!(pubkey, m)?)?;
    m.add_class::<WireguardServer>()?;
    m.add_class::<TcpStream>()?;
    Ok(())
}
