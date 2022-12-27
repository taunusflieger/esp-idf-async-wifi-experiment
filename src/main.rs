use crate::errors::*;
use crate::lazy_http_server::lazy_init_http_server::LazyInitHttpServer;
use channel_bridge::{asynch::pubsub, asynch::*};
use edge_executor::*;
use edge_executor::{Local, Task};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::pubsub::PubSubChannel;
use embassy_time::{Duration, Timer};
use embedded_svc::utils::asyncify::Asyncify;
use embedded_svc::wifi::{AuthMethod, ClientConfiguration, Configuration, Wifi as WifiTrait};
use esp_idf_hal::modem::WifiModemPeripheral;
use esp_idf_hal::peripheral::Peripheral;
use esp_idf_hal::peripherals::Peripherals;
use esp_idf_hal::task::executor::EspExecutor;
use esp_idf_hal::task::thread::ThreadSpawnConfiguration;
use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::netif::IpEvent;
use esp_idf_svc::nvs::EspDefaultNvsPartition;
use esp_idf_svc::wifi::{EspWifi, WifiEvent};
use esp_idf_sys as _; // If using the `binstart` feature of `esp-idf-sys`, always keep this module imported
use esp_idf_sys::EspError;
use esp_idf_sys::{self as sys};
use log::*;

mod errors;
mod lazy_http_server;

sys::esp_app_desc!();

const SSID: &str = env!("RUST_ESP32_ANEMOMETER_WIFI_SSID");
const PASS: &str = env!("RUST_ESP32_ANEMOMETER_WIFI_PASS");

const TASK_PRIORITY: u8 = 30;
static CHANNEL: PubSubChannel<CriticalSectionRawMutex, [u8; 8], 4, 4, 4> = PubSubChannel::new();
static NETWORK_EVENT_CHANNEL: PubSubChannel<CriticalSectionRawMutex, NetworkStateChange, 4, 4, 4> =
    PubSubChannel::new();

#[allow(dead_code)]
static APPLICATION_EVENT_CHANNEL: PubSubChannel<
    CriticalSectionRawMutex,
    ApplicationStateChange,
    4,
    4,
    4,
> = PubSubChannel::new();

#[derive(Copy, Clone, Debug)]
enum NetworkStateChange {
    WifiDisconnected,
    IpAddressAssigned { ip: embedded_svc::ipv4::Ipv4Addr },
}

#[derive(Copy, Clone, Debug)]
#[allow(dead_code)]
enum ApplicationStateChange {
    OTAUpdateRequest,
    OTAUpdateStarted,
}

fn main() -> anyhow::Result<()> {
    esp_idf_hal::task::critical_section::link();
    esp_idf_svc::timer::embassy_time::driver::link();
    esp_idf_svc::timer::embassy_time::queue::link();

    esp_idf_svc::log::EspLogger::initialize_default();
    info!("Minimal asynch IDF wifi example");

    info!("Wifi name {}", SSID);

    let peripherals = Peripherals::take().unwrap();
    let nvs_default_partition = EspDefaultNvsPartition::take()?;
    let sysloop = EspSystemEventLoop::take()?;

    let (wifi, wifi_notif) = wifi(
        peripherals.modem,
        sysloop.clone(),
        Some(nvs_default_partition.clone()),
    )?;

    ThreadSpawnConfiguration {
        name: Some(b"wifi-async-executor\0"),
        priority: TASK_PRIORITY,
        ..Default::default()
    }
    .set()?;

    let mid_prio_execution = schedule::<8, _>(50000, move || {
        let executor = EspExecutor::new();
        let mut tasks = heapless::Vec::new();

        executor.spawn_local_collect(process_wifi_state_change(wifi, wifi_notif), &mut tasks)?;

        executor.spawn_local_collect(channel_publisher_task(), &mut tasks)?;

        executor.spawn_local_collect(channel_subscriber1_task(), &mut tasks)?;

        executor.spawn_local_collect(channel_subscriber2_task(), &mut tasks)?;

        executor.spawn_local_collect(http_server_task(), &mut tasks)?;

        let netif_notif = netif_notifier(sysloop.clone()).unwrap(); // TODO: error conversion
        executor.spawn_local_collect(process_netif_state_change(netif_notif), &mut tasks)?;

        Ok((executor, tasks))
    });

    mid_prio_execution.join().unwrap();

    unreachable!();
}

pub fn schedule<'a, const C: usize, M>(
    stack_size: usize,
    spawner: impl FnOnce() -> Result<(Executor<'a, C, M, Local>, heapless::Vec<Task<()>, C>), SpawnError>
        + Send
        + 'static,
) -> std::thread::JoinHandle<()>
where
    M: Monitor + Wait + Default,
{
    std::thread::Builder::new()
        .stack_size(stack_size)
        .spawn(move || {
            let (executor, tasks) = spawner().unwrap();

            executor.run_tasks(|| true, tasks);
        })
        .unwrap()
}

#[inline(always)]
pub fn netif_notifier(
    mut sysloop: EspSystemEventLoop,
) -> Result<impl Receiver<Data = IpEvent>, InitError> {
    Ok(pubsub::SvcReceiver::new(sysloop.as_async().subscribe()?))
}

pub fn wifi<'d>(
    modem: impl Peripheral<P = impl WifiModemPeripheral + 'd> + 'd,
    mut sysloop: EspSystemEventLoop,
    partition: Option<EspDefaultNvsPartition>,
) -> Result<(impl WifiTrait + 'd, impl Receiver<Data = WifiEvent>), EspError> {
    let mut wifi = EspWifi::new(modem, sysloop.clone(), partition)?;

    if PASS.is_empty() {
        wifi.set_configuration(&Configuration::Client(ClientConfiguration {
            ssid: SSID.into(),
            auth_method: AuthMethod::None,
            ..Default::default()
        }))?;
    } else {
        wifi.set_configuration(&Configuration::Client(ClientConfiguration {
            ssid: SSID.into(),
            password: PASS.into(),
            ..Default::default()
        }))?;
    }

    wifi.start()?;

    wifi.connect()?;

    Ok((
        wifi,
        pubsub::SvcReceiver::new(sysloop.as_async().subscribe()?),
    ))
}

pub async fn process_wifi_state_change(
    mut wifi: impl WifiTrait,
    mut state_changed_source: impl Receiver<Data = WifiEvent>,
) {
    loop {
        let event = state_changed_source.recv().await.unwrap();

        match event {
            WifiEvent::StaConnected => {
                info!("WifiEvent: STAConnected");
            }
            WifiEvent::StaDisconnected => {
                info!("WifiEvent: STADisconnected");
                let mut publisher = NETWORK_EVENT_CHANNEL.publisher().unwrap();
                let _ = publisher.send(NetworkStateChange::WifiDisconnected).await;
                let _ = wifi.connect();
            }
            _ => {
                info!("WifiEvent: other .....");
            }
        }
    }
}

pub async fn process_netif_state_change(mut state_changed_source: impl Receiver<Data = IpEvent>) {
    loop {
        let event = state_changed_source.recv().await.unwrap();

        match event {
            IpEvent::DhcpIpAssigned(assignment) => {
                info!("IpEvent: DhcpIpAssigned: {:?}", assignment.ip_settings.ip);
                let mut publisher = NETWORK_EVENT_CHANNEL.publisher().unwrap();
                let _ = publisher
                    .send(NetworkStateChange::IpAddressAssigned {
                        ip: assignment.ip_settings.ip,
                    })
                    .await;
            }
            _ => {
                info!("IpEvent: other .....");
            }
        }
    }
}

async fn channel_publisher_task() {
    let mut buf = [0; 8];
    buf.copy_from_slice(b"Hello!\r\n");
    loop {
        let mut publisher = CHANNEL.publisher().unwrap();
        let _ = publisher.send(buf).await;
        info!("send: {:?}", buf);
        Timer::after(Duration::from_secs(10)).await;
    }
}

async fn channel_subscriber1_task() {
    loop {
        let mut subscriber = CHANNEL.subscriber().unwrap();
        let buf = subscriber.recv().await;
        info!("Sub 1 received: {:?}", buf);
    }
}

async fn channel_subscriber2_task() {
    loop {
        let mut subscriber = CHANNEL.subscriber().unwrap();
        let buf = subscriber.recv().await;
        info!("Sub 2 received: {:?}", buf);
    }
}

async fn http_server_task() {
    use embedded_svc::io::blocking::Write;
    use embedded_svc::utils::http::Headers;
    use esp_idf_svc::http::server::Configuration;

    const FIRMWARE_VERSION: &str = env!("CARGO_PKG_VERSION");

    let httpd = LazyInitHttpServer::new();
    loop {
        let mut subscriber = NETWORK_EVENT_CHANNEL.subscriber().unwrap();
        let event = subscriber.recv().await.unwrap();

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
