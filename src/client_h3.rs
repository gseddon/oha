use std::net::SocketAddr;
use std::net::UdpSocket;
use std::sync::atomic::AtomicBool;
use core::sync::atomic::Ordering;
use std::sync::atomic::AtomicUsize;
use std::sync::Arc;
use std::time::Instant;
use bytes::Buf;
use bytes::Bytes;
use http_body_util::BodyExt;
use kanal::AsyncReceiver;
use quinn::default_runtime;
use hyper::http;


use tokio::sync::Semaphore;
use url::Url;

use crate::client::{
    Client,
    ClientError,
    ConnectionTime,
    RequestResult,
    Stream,
    SendRequestHttp3,
    is_cancel_error,
    set_connection_time,
    set_start_latency_correction
};
use crate::pcg64si::Pcg64Si;
use crate::result_data::ResultData;
use rand::prelude::Rng;
use rand::SeedableRng;

pub (crate) struct ClientStateHttp3 {
    pub (crate) rng: Pcg64Si,
    pub (crate) send_request: h3::client::SendRequest<h3_quinn::OpenStreams, Bytes>,
}

impl ClientStateHttp3 {
    fn new(send_request: h3::client::SendRequest<h3_quinn::OpenStreams, Bytes>) -> Self {
        Self {
            rng: SeedableRng::from_os_rng(),
            send_request
        }
    }
}

impl Client {
    pub (crate) async fn connect_http3<R: Rng>(
        &self,
        url: &Url,
        rng: &mut R
    ) -> Result<(ConnectionTime, SendRequestHttp3), ClientError> {
        let (dns_lookup, stream) = self.client(url, rng, http::Version::HTTP_3).await?;
        let send_request = stream.handshake_http3().await?;
        let dialup = std::time::Instant::now();
        Ok((ConnectionTime { dns_lookup, dialup }, send_request))
    }

    pub (crate) async fn quic_client(
        &self,
        addr: (std::net::IpAddr, u16),
        url: &Url
    ) -> Result<Stream, ClientError> {
        let endpoint_config = h3_quinn::quinn::EndpointConfig::default();
        let local_socket = UdpSocket::bind("0.0.0.0:0").expect("couldn't bind to address");
        // If we can set the right build flags, we can use `h3_quinn::quinn::Endpoint::client` instead
        let mut client_endpoint = h3_quinn::quinn::Endpoint::new(
            endpoint_config,
            None,
            local_socket,
            default_runtime().unwrap()
        ).unwrap();

        let tls_config = self.rustls_configs.config(http::Version::HTTP_3).clone();
        let client_config = quinn::ClientConfig::new(Arc::new(
            quinn::crypto::rustls::QuicClientConfig::try_from(tls_config)?,
        ));
        client_endpoint.set_default_client_config(client_config);

        let remote_socket_address = SocketAddr::new(addr.0, addr.1);
        let server_name = url.host_str().ok_or(ClientError::HostNotFound)?;
        let conn = client_endpoint.connect(remote_socket_address, server_name)?.await?;
        Ok(Stream::Quic(conn))
    }


    pub (crate) async fn work_http3(
        &self,
        client_state: &mut ClientStateHttp3
    ) -> Result<RequestResult, ClientError> 
    {
        let do_req = async {
            let (url, rng) = self.generate_url(&mut client_state.rng)?;
            let start = std::time::Instant::now();
            let connection_time: Option<ConnectionTime> = None;
            let mut first_byte: Option<std::time::Instant> = None;

            let request = self.request(&url)?;
            // if we implement http_body::Body on our H3 SendRequest, we can do some nice streaming stuff
            // with the response here. However as we don't really use the response we can get away
            // with not doing this for now
            let (head, mut req_body) = request.into_parts();
            let request = http::request::Request::from_parts(head, ());
            let mut stream = client_state.send_request.send_request(request).await?;
            // send the request body now
            match req_body.frame().await {
                Some(Ok(frame)) => {
                    if let Ok(data) = frame.into_data() {
                        stream.send_data(data).await?;
                    }
                }
                _ => {}
            }
            stream.finish().await?;

            // now read the response headers
            let response = stream.recv_response().await?;
            let (parts, _) = response.into_parts();
            let status = parts.status;
            // now read the response body
            let mut len_bytes = 0;
            while let Some(chunk) = stream.recv_data().await? {
                if first_byte.is_none() {
                    first_byte = Some(std::time::Instant::now())
                }
                len_bytes += chunk.remaining();
            };
            let end = std::time::Instant::now();

            let result = RequestResult {
                rng,
                start_latency_correction: None,
                start,
                first_byte,
                end,
                status,
                len_bytes,
                connection_time,
            };

            Ok::<_, ClientError>(result)
        };


        if let Some(timeout) = self.timeout {
            tokio::select! {
                res = do_req => {
                    res
                }
                _ = tokio::time::sleep(timeout) => {
                    Err(ClientError::Timeout)
                }
            }
        } else {
            do_req.await
        }
    }
}

impl Stream {
    async fn handshake_http3(self) -> Result<SendRequestHttp3, ClientError> {
        let Stream::Quic(quic_conn) = self else {
            panic!("You cannot call http3 handshake on a non-quic stream");
        };
        let h3_quinn_conn = h3_quinn::Connection::new(quic_conn);
        // TODO add configuration settings to allow 'send_grease' etc.

        Ok(h3::client::new(h3_quinn_conn).await?)
    }
}

/**
 * Create `n_connections` parallel HTTP3 connections (on independent QUIC connections).
 * On each of those, run `n_http3_parallel` requests continuously until `deadline` is reached.
 */
pub (crate) async fn parallel_work_http3(
    n_connections: usize,
    n_http3_parallel: usize,
    rx: AsyncReceiver<Option<Instant>>,
    report_tx: kanal::Sender<Result<RequestResult, ClientError>>,
    client: Arc<Client>,
    deadline: Option<std::time::Instant>
)  -> Vec<tokio::task::JoinHandle<()>>  {
    let s = Arc::new(tokio::sync::Semaphore::new(0));
    let has_deadline = deadline.is_some();

    let futures = (0..n_connections)
    .map(|_| {
        let report_tx = report_tx.clone();
        let rx = rx.clone();
        let client = client.clone();
        let s = s.clone();
        tokio::spawn(create_and_load_up_single_connection_http3(n_http3_parallel, rx, report_tx, client, s))
    })
    .collect::<Vec<_>>();

    if has_deadline {
        tokio::time::sleep_until(deadline.unwrap().into()).await;
        s.close();
    }

    return futures;
}

/**
 * For use in the 'slow' functions - send a report of every response in real time for display to the end-user.
 * Semaphore is closed to shut down all the tasks.
 */
async fn create_and_load_up_single_connection_http3(
    n_http3_parallel: usize,
    rx: AsyncReceiver<Option<Instant>>,
    report_tx: kanal::Sender<Result<RequestResult, ClientError>>,
    client: Arc<Client>,
    s: Arc<Semaphore>,
) {
    loop {
        // create a HTTP3 connection
        match setup_http3(&client).await {
            Ok((connection_time, (h3_connection, send_request))) => {
                let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
                let http3_driver = spawn_http3_driver(h3_connection, shutdown_rx).await;
                let futures = (0..n_http3_parallel)
                .map(|_| {
                    let report_tx = report_tx.clone();
                    let rx = rx.clone();
                    let client = client.clone();
                    let mut client_state = ClientStateHttp3::new(send_request.clone());
                    let s = s.clone();
                    tokio::spawn(async move {
                        // This is where HTTP3 loops to make all the requests for a given client and worker
                        while let Ok(start_time_option) = rx.recv().await {
                            let (is_cancel, is_reconnect) = work_http3_once(
                                &client,
                                &mut client_state,
                                &report_tx,
                                connection_time,
                                start_time_option,
                            )
                            .await;

                            let is_cancel = is_cancel || s.is_closed();
                            if is_cancel || is_reconnect {
                                return is_cancel;
                            }
                        }
                        true

                    })
                })
                .collect::<Vec<_>>();

                // collect all the requests we have spawned, and end the process if/when the semaphore says
                let mut connection_gone = false;
                for f in futures {
                    tokio::select! {
                        r = f => {
                            match r {
                                Ok(true) => {
                                    // All works done
                                    connection_gone = true;
                                }
                                Err(_) => {
                                    // Unexpected
                                    connection_gone = true;
                                }
                                _ => {}
                            }
                        }
                        _ = s.acquire() => {
                            report_tx.send(Err(ClientError::Deadline)).unwrap();
                            connection_gone = true;
                        }
                    }
                }
                if connection_gone {
                    // Try and politely shut down the HTTP3 connection
                    let _ = shutdown_tx.send(0);
                    let _ = http3_driver.await;
                    return;
                }
            }
            Err(err) => {
                if s.is_closed() {
                    break;
                    // Consume a task 
                } else if rx.recv().await.is_ok() {
                    report_tx.send(Err(err)).unwrap();
                } else {
                    return;
                }
            }
        }
    }
}

/**
 * This is structured to work very similarly to the `setup_http2`
 * function in `client.rs`
 */
pub (crate) async fn setup_http3(client: &Client) -> Result<(ConnectionTime, SendRequestHttp3), ClientError> {
    // Whatever rng state, all urls should have the same authority
    let mut rng: Pcg64Si = SeedableRng::from_seed([0, 0, 0, 0, 0, 0, 0, 0]);
    let url = client.url_generator.generate(&mut rng)?;
    let (connection_time, send_request) = client.connect_http3(&url, &mut rng).await?;

    Ok((connection_time, send_request))
}

pub (crate) async fn spawn_http3_driver(
    mut h3_connection: h3::client::Connection<h3_quinn::Connection, Bytes>,
    shutdown_rx: tokio::sync::oneshot::Receiver<usize>
) -> tokio::task::JoinHandle<std::result::Result<(), ClientError>> {
    tokio::spawn(async move {
        let result = tokio::select! {
            // Drive the connection
            closed = std::future::poll_fn(|cx| h3_connection.poll_close(cx)) => closed.map_err(ClientError::from),
            // Listen for shutdown condition
            _ = shutdown_rx => {
                // Initiate shutdown
                h3_connection.shutdown(0).await?;
                // Wait for ongoing work to complete
                std::future::poll_fn(|cx| h3_connection.poll_close(cx)).await.map_err(ClientError::from)
            }
        };
        return result;
    })
}

pub (crate) async fn work_http3_once(
    client: &Client,
    client_state: &mut ClientStateHttp3,
    report_tx: &kanal::Sender<Result<RequestResult, ClientError>>,
    connection_time: ConnectionTime,
    start_latency_correction: Option<Instant>,
) -> (bool, bool) {
    let mut res = client.work_http3(client_state).await;
    let is_cancel = is_cancel_error(&res);
    let is_reconnect = is_h3_error(&res);
    set_connection_time(&mut res, connection_time);
    if let Some(start_latency_correction) = start_latency_correction {
        set_start_latency_correction(&mut res, start_latency_correction);
    }
    report_tx.send(res).unwrap();
    (is_cancel, is_reconnect)
}

fn is_h3_error(res: &Result<RequestResult, ClientError>) -> bool {
    res.as_ref()
        .err()
        .map(|err| match err {
            ClientError::H3Error(_) | ClientError::IoError(_) => true,
            _ => false
        })
        .unwrap_or(false)
}

/**
 * 'Fast' implementation of HTTP3 load generation.
 * If `n_tasks` is set, it will generate up to that many tasks.
 * Othrwise it will terminate when `is_end` becomes set to true.
 */
pub (crate) fn http3_connection_fast_work_until(
    num_connections: usize,
    n_http_parallel: usize,
    report_tx: kanal::Sender<ResultData>,
    client: Arc<Client>,
    token: tokio_util::sync::CancellationToken,
    is_end: Arc<AtomicBool>,
    counter: Arc<AtomicUsize>,
    n_tasks: Option<usize>,
    rt: tokio::runtime::Runtime,
) {
    let is_counting_tasks = n_tasks.is_some();
    let client = client.clone();
    let local = tokio::task::LocalSet::new();
    for _ in 0..num_connections {
        let report_tx = report_tx.clone();
        let client = client.clone();
        let token = token.clone();
        let is_end = is_end.clone();
        let counter = counter.clone();
        local.spawn_local(Box::pin(async move {
            let mut has_err = false;
            let mut result_data_err = ResultData::default();
            loop {
                let client = client.clone();
                match setup_http3(&client).await {
                    Ok((connection_time, (h3_connection, send_request))) => {
                        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
                        let http3_driver = spawn_http3_driver(h3_connection, shutdown_rx).await;
                        let futures = (0..n_http_parallel)
                            .map(|_| {
                                let mut client_state = ClientStateHttp3::new(send_request.clone());
                                let client = client.clone();
                                let report_tx = report_tx.clone();
                                let token = token.clone();
                                let is_end = is_end.clone();
                                let counter = counter.clone();
                                tokio::task::spawn_local(async move {
                                    let mut result_data = ResultData::default();

                                    let work = async {
                                        loop {
                                            if is_counting_tasks {
                                                if counter.fetch_add(1, Ordering::Relaxed) >= n_tasks.unwrap() {
                                                    return true;
                                                }
                                            }
                                            let mut res = client
                                                .work_http3(&mut client_state)
                                                .await;
                                            let is_cancel = is_cancel_error(&res) || is_end.load(Ordering::Relaxed);
                                            let is_reconnect = is_h3_error(&res);
                                            set_connection_time(
                                                &mut res,
                                                connection_time,
                                            );

                                            result_data.push(res);

                                            if is_cancel || is_reconnect {
                                                return is_cancel;
                                            }
                                        }
                                    };

                                    let is_cancel = tokio::select! {
                                        is_cancel = work => {
                                            is_cancel
                                        }
                                        _ = token.cancelled() => {
                                            result_data.push(Err(ClientError::Deadline));
                                            true
                                        }
                                    };

                                    report_tx.send(result_data).unwrap();
                                    is_cancel
                                })
                            })
                            .collect::<Vec<_>>();

                        let mut connection_gone = false;
                        for f in futures {
                            match f.await {
                                Ok(true) => {
                                    // All works done
                                    connection_gone = true;
                                }
                                Err(_) => {
                                    // Unexpected
                                    connection_gone = true;
                                }
                                _ => {}
                            }
                        }

                        if connection_gone {
                            let _ = shutdown_tx.send(0);
                            let _ = http3_driver.await;
                            break;
                        }
                    }
                    Err(err) => {
                        has_err = true;
                        result_data_err.push(Err(err));
                        if is_end.load(Ordering::Relaxed) || is_counting_tasks && counter.fetch_add(1, Ordering::Relaxed) >= n_tasks.unwrap() {
                            break;
                        }
                    }
                }
            }
            if has_err {
                report_tx.send(result_data_err).unwrap();
            }
        }));
    }
    rt.block_on(local);
}