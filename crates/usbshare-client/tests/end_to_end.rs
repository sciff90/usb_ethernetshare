//! Server (with mock devices) and client in one process over loopback TCP.

use std::sync::Arc;

use tokio::net::TcpListener;
use usbshare_client::{remote, selftest};
use usbshare_server::mock::{MockBackend, MockKind};
use usbshare_server::Server;

async fn start_server() -> String {
    let backend = MockBackend::new(&[
        MockKind::Loopback,
        MockKind::Hid,
        MockKind::Storage { size_mib: 8 },
    ]);
    let server = Server::new(vec![Arc::new(backend)]);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    tokio::spawn(server.serve(listener));
    addr
}

#[tokio::test(flavor = "multi_thread")]
async fn lists_mock_devices() {
    let addr = start_server().await;
    let devs = remote::list(&addr).await.unwrap();
    let ids: Vec<(String, u16, u16)> = devs
        .iter()
        .map(|d| (d.device.busid.clone(), d.device.id_vendor, d.device.id_product))
        .collect();
    assert_eq!(
        ids,
        vec![
            ("100-1".into(), 0x1209, 0x0001),
            ("100-2".into(), 0x1209, 0x0002),
            ("100-3".into(), 0x1209, 0x0003)
        ]
    );
    assert_eq!(devs[2].interfaces[0].class, 8);
}

#[tokio::test(flavor = "multi_thread")]
async fn all_selftests_pass() {
    let addr = start_server().await;
    for d in remote::list(&addr).await.unwrap() {
        let r = selftest::test_device(&addr, &d).await.unwrap();
        assert_eq!(r.failed, 0, "failures on {}", d.device.busid);
        assert!(r.passed >= 5);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn device_is_exclusive_and_released_on_disconnect() {
    let addr = start_server().await;
    let (stream, _) = remote::import(&addr, "100-1").await.unwrap();

    // In-use devices disappear from the list and cannot be imported twice.
    let busids: Vec<String> = remote::list(&addr)
        .await
        .unwrap()
        .into_iter()
        .map(|d| d.device.busid)
        .collect();
    assert!(!busids.contains(&"100-1".to_string()));
    let err = remote::import(&addr, "100-1").await.unwrap_err();
    assert!(format!("{err}").contains("in use"), "{err}");

    // Closing the connection releases the device.
    drop(stream);
    for _ in 0..50 {
        if remote::import(&addr, "100-1").await.is_ok() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("device was not released");
}

#[tokio::test(flavor = "multi_thread")]
async fn unknown_device_is_refused() {
    let addr = start_server().await;
    let err = remote::import(&addr, "9-9").await.unwrap_err();
    assert!(format!("{err}").contains("no such device"), "{err}");
}
