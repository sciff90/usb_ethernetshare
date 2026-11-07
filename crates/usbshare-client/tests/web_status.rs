//! The server's web dashboard reports devices, who uses them and traffic.

use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use usbshare_client::host::UsbIpHost;
use usbshare_client::remote;
use usbshare_server::mock::{MockBackend, MockKind};
use usbshare_server::{web, Server};

async fn http_get(addr: &str, path: &str) -> (String, String) {
    let mut s = TcpStream::connect(addr).await.unwrap();
    let req = format!("GET {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n");
    s.write_all(req.as_bytes()).await.unwrap();
    let mut buf = String::new();
    s.read_to_string(&mut buf).await.unwrap();
    let (head, body) = buf.split_once("\r\n\r\n").unwrap();
    (head.to_string(), body.to_string())
}

#[tokio::test(flavor = "multi_thread")]
async fn dashboard_tracks_usage() {
    let server = Server::new(vec![Arc::new(MockBackend::new(&[
        MockKind::Loopback,
        MockKind::Hid,
    ]))]);
    let usbip = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let usbip_addr = usbip.local_addr().unwrap();
    tokio::spawn(server.clone().serve(usbip));
    let http = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let http_addr = http.local_addr().unwrap().to_string();
    tokio::spawn(web::serve(server.clone(), http, usbip_addr));
    let addr = usbip_addr.to_string();

    // Nothing in use yet.
    let st = web::status(&server, usbip_addr).await;
    assert_eq!(st.devices.len(), 2);
    assert!(st.devices.iter().all(|d| !d.in_use && d.client.is_none()));
    assert_eq!(st.devices[0].product.as_deref(), Some("usbshare Loopback"));

    // Attach the loopback device and move some data.
    let (stream, desc) = remote::import(&addr, "100-1").await.unwrap();
    let host = UsbIpHost::new(stream, desc.devid());
    host.write(0x01, &[7u8; 1000], Duration::from_secs(5))
        .await
        .unwrap();
    assert_eq!(
        host.read(0x81, 4096, Duration::from_secs(5)).await.unwrap().len(),
        1000
    );

    let st = web::status(&server, usbip_addr).await;
    let d = st.devices.iter().find(|d| d.busid == "100-1").unwrap();
    assert!(d.in_use);
    let c = d.client.as_ref().unwrap();
    assert_eq!(c.ip, "127.0.0.1");
    assert_eq!(c.bytes_from_client, 1000);
    assert_eq!(c.bytes_to_client, 1000);
    assert_eq!(c.transfers, 2);
    assert!(!st.devices.iter().find(|d| d.busid == "100-2").unwrap().in_use);

    // Same data over HTTP.
    let (head, body) = http_get(&http_addr, "/api/status").await;
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    let devs = json["devices"].as_array().unwrap();
    assert_eq!(devs[0]["busid"], "100-1");
    assert_eq!(devs[0]["in_use"], true);
    assert_eq!(devs[0]["client"]["ip"], "127.0.0.1");
    assert_eq!(devs[1]["client"], serde_json::Value::Null);

    let (head, body) = http_get(&http_addr, "/").await;
    assert!(head.starts_with("HTTP/1.1 200"));
    assert!(body.contains("<title>usbshare</title>"));

    // Disconnecting frees the device and records the last client.
    drop(host);
    for _ in 0..100 {
        let st = web::status(&server, usbip_addr).await;
        let d = st.devices.iter().find(|d| d.busid == "100-1").unwrap();
        if !d.in_use {
            assert_eq!(d.last_client.as_ref().unwrap().ip, "127.0.0.1");
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("device not released");
}
