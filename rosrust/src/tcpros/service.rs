use super::error::{ErrorKind, Result};
use super::header;
use super::util::tcpconnection;
use super::{ServicePair, ServiceResult};
use crate::rosmsg::{encode_str, RosMsg};
use crate::RawMessageDescription;
use byteorder::{LittleEndian, ReadBytesExt, WriteBytesExt};
use error_chain::bail;
use log::error;
use std::collections::HashMap;
use std::convert::TryInto;
use std::io;
use std::net::{TcpListener, TcpStream};
use std::sync::{atomic, Arc};
use std::thread;

pub struct Service {
    pub api: String,
    pub msg_type: String,
    pub service: String,
    exists: Arc<atomic::AtomicBool>,
}

impl Drop for Service {
    fn drop(&mut self) {
        self.exists.store(false, atomic::Ordering::SeqCst);
    }
}

impl Service {
    pub fn new<T, F>(
        hostname: &str,
        bind_address: &str,
        port: u16,
        service: &str,
        node_name: &str,
        handler: F,
        description: RawMessageDescription,
    ) -> Result<Service>
    where
        T: ServicePair,
        F: Fn(T::Request) -> ServiceResult<T::Response> + Send + Sync + 'static,
    {
        let listener = TcpListener::bind((bind_address, port))?;
        let socket_address = listener.local_addr()?;
        let api = format!("rosrpc://{}:{}", hostname, socket_address.port());

        let service_exists = Arc::new(atomic::AtomicBool::new(true));

        let description = Arc::new(description);
        let iterate_handler = {
            let service_exists = service_exists.clone();
            let service = String::from(service);
            let node_name = String::from(node_name);
            let handler = Arc::new(handler);
            let description = Arc::clone(&description);
            move |stream: TcpStream| {
                if !service_exists.load(atomic::Ordering::SeqCst) {
                    return tcpconnection::Feedback::StopAccepting;
                }
                consume_client::<T, _, _>(
                    &service,
                    &node_name,
                    Arc::clone(&handler),
                    stream,
                    Arc::clone(&description),
                );
                tcpconnection::Feedback::AcceptNextStream
            }
        };

        tcpconnection::iterate(listener, format!("service '{}'", service), iterate_handler);

        Ok(Service {
            api,
            msg_type: description.msg_type.clone(),
            service: String::from(service),
            exists: service_exists,
        })
    }
}

enum RequestType {
    Probe,
    Action,
}

fn consume_client<T, U, F>(
    service: &str,
    node_name: &str,
    handler: Arc<F>,
    mut stream: U,
    description: Arc<RawMessageDescription>,
) where
    T: ServicePair,
    U: std::io::Read + std::io::Write + Send + 'static,
    F: Fn(T::Request) -> ServiceResult<T::Response> + Send + Sync + 'static,
{
    // Service request starts by exchanging connection headers
    match exchange_headers::<_>(&mut stream, service, node_name, &description) {
        Err(err) => {
            // Connection can be closed when a client checks for a service.
            if !err.is_closed_connection() {
                error!(
                    "Failed to exchange headers for service '{}': {}",
                    service, err
                );
            }
        }
        // Spawn a thread for handling requests
        Ok(RequestType::Action) => spawn_request_handler::<T, U, F>(stream, Arc::clone(&handler)),
        Ok(RequestType::Probe) => (),
    }
}

fn exchange_headers<U>(
    stream: &mut U,
    service: &str,
    node_name: &str,
    description: &RawMessageDescription,
) -> Result<RequestType>
where
    U: std::io::Write + std::io::Read,
{
    let req_type = read_request::<U>(stream, service, description)?;
    write_response::<U>(stream, node_name, description)?;
    Ok(req_type)
}

fn read_request<U: std::io::Read>(
    stream: &mut U,
    service: &str,
    description: &RawMessageDescription,
) -> Result<RequestType> {
    let fields = header::decode(stream)?;
    header::match_field(&fields, "service", service)?;
    if fields.get("callerid").is_none() {
        bail!(ErrorKind::HeaderMissingField("callerid".into()));
    }
    if header::match_field(&fields, "probe", "1").is_ok() {
        return Ok(RequestType::Probe);
    }
    header::match_field(&fields, "md5sum", &description.md5sum)?;
    Ok(RequestType::Action)
}

fn write_response<U>(
    stream: &mut U,
    node_name: &str,
    description: &RawMessageDescription,
) -> Result<()>
where
    U: std::io::Write,
{
    let mut fields = HashMap::<String, String>::new();
    fields.insert(String::from("callerid"), String::from(node_name));
    fields.insert(String::from("md5sum"), description.md5sum.clone());
    fields.insert(String::from("type"), description.msg_type.clone());
    header::encode(stream, &fields)?;
    Ok(())
}

fn spawn_request_handler<T, U, F>(stream: U, handler: Arc<F>)
where
    T: ServicePair,
    U: std::io::Read + std::io::Write + Send + 'static,
    F: Fn(T::Request) -> ServiceResult<T::Response> + Send + Sync + 'static,
{
    thread::spawn(move || {
        if let Err(err) = handle_request_loop::<T, U, F>(stream, &handler) {
            if !err.is_closed_connection() {
                let info = err
                    .iter()
                    .map(|v| format!("{}", v))
                    .collect::<Vec<_>>()
                    .join("\nCaused by:");
                error!("{}", info);
            }
        }
    });
}

fn handle_request_loop<T, U, F>(mut stream: U, handler: &F) -> Result<()>
where
    T: ServicePair,
    U: std::io::Read + std::io::Write,
    F: Fn(T::Request) -> ServiceResult<T::Response>,
{
    // Receive request from client
    // TODO: validate message length
    let _length = stream.read_u32::<LittleEndian>();

    let mut buf = Vec::<u8>::new();
    buf.resize(_length.unwrap().try_into().unwrap(), 0);
    stream.read_exact(&mut buf).unwrap();

    // Break out of loop in case of failure to read request
    // TODO: handle retained connections
    if let Ok(req) = RosMsg::decode(&mut buf.as_slice()) {
        // Call function that handles request and returns response
        match handler(req) {
            Ok(res) => {
                // Send True flag and response in case of success
                stream.write_u8(1)?;
                let mut writer = io::Cursor::new(Vec::with_capacity(128));
                // skip the first 4 bytes that will contain the message length
                writer.set_position(4);

                res.encode(&mut writer)?;

                // write the message length to the start of the header
                let message_length = (writer.position() - 4) as u32;
                writer.set_position(0);
                message_length.encode(&mut writer)?;

                stream.write_all(&writer.into_inner())?;
            }
            Err(message) => {
                // Send False flag and error message string in case of failure
                stream.write_u8(0)?;
                RosMsg::encode(&message, &mut stream)?;
            }
        };
    } else {
        // Upon failure to read request, send client failure message
        // This can be caused by actual issues or by the client stopping the connection
        stream.write_u8(0)?;
        encode_str("Failed to parse passed arguments", &mut stream)?;
    }
    Ok(())
}
