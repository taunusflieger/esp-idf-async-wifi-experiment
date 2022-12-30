use crate::services::http::*;
use crate::state::*;
use log::*;

pub async fn http_server_task() {
    // use channel_bridge::asynch::*;
    use embedded_svc::io::blocking::Write;
    use embedded_svc::utils::http::Headers;
    use esp_idf_svc::http::server::Configuration;

    const FIRMWARE_VERSION: &str = env!("CARGO_PKG_VERSION");

    let httpd = LazyInitHttpServer::new();
    loop {
        let mut subscriber = NETWORK_EVENT_CHANNEL.subscriber().unwrap();
        let event = subscriber.next_message_pure().await;

        match event {
            NetworkStateChange::IpAddressAssigned { ip } => {
                let conf = Configuration::default();
                let mut s = httpd.create(&conf);

                info!("http_server_task: starting httpd on address: {:?}", ip);
                if let Err(err) = s.fn_handler("/", embedded_svc::http::Method::Get, move |req| {
                    let mut headers = Headers::<1>::new();
                    headers.set_cache_control("no-store");

                    let mut resp = req.into_response(200, None, headers.as_slice())?;
                    resp.write_all(FIRMWARE_VERSION.as_bytes())?;

                    info!("Processing '/' request");
                    Ok(())
                }) {
                    info!(
                        "http_server_task: failed to register http handler /: {:?} - restarting device",
                        err
                    );
                    unsafe {
                        esp_idf_sys::esp_restart();
                    }
                }
            }
            NetworkStateChange::WifiDisconnected => {
                info!("http_server_task: stopping httpd");
                httpd.clear();
            }
        }
    }
}
