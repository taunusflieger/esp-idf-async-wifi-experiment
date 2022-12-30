use crate::errors::*;
use crate::services::*;
use crate::state::*;
use crate::task::{httpd::*, mqtt, ota::*};

use channel_bridge::{asynch::pubsub, asynch::*};
use edge_executor::*;
use edge_executor::{Local, Task};
use embassy_time::{Duration, Timer};
use embedded_svc::utils::asyncify::Asyncify;
use embedded_svc::wifi::Wifi as WifiTrait;
use esp_idf_hal::peripherals::Peripherals;
use esp_idf_hal::task::executor::EspExecutor;
use esp_idf_hal::task::thread::ThreadSpawnConfiguration;
use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::netif::IpEvent;
use esp_idf_svc::nvs::EspDefaultNvsPartition;
use esp_idf_svc::wifi::WifiEvent;
use esp_idf_sys as _; // If using the `binstart` feature of `esp-idf-sys`, always keep this module imported

use esp_idf_sys::{self as sys};
use log::*;

mod error;
mod errors;
mod mqtt_msg;
mod services;
mod state;
mod task;

sys::esp_app_desc!();

const TASK_MID_PRIORITY: u8 = 40;
const TASK_LOW_PRIORITY: u8 = 30;
const MQTT_MAX_TOPIC_LEN: usize = 64;

fn main() -> Result<(), InitError> {
    esp_idf_hal::task::critical_section::link();
    esp_idf_svc::timer::embassy_time::driver::link();
    esp_idf_svc::timer::embassy_time::queue::link();

    esp_idf_svc::log::EspLogger::initialize_default();
    info!("Minimal asynch IDF wifi example");

    let peripherals = Peripherals::take().unwrap();
    let nvs_default_partition = EspDefaultNvsPartition::take()?;
    let sysloop = EspSystemEventLoop::take()?;

    let (wifi, wifi_notif) = wifi(
        peripherals.modem,
        sysloop.clone(),
        Some(nvs_default_partition),
    )?;

    let (mqtt_topic_prefix, mqtt_client, mqtt_conn) = services::mqtt()?;

    ThreadSpawnConfiguration {
        name: Some(b"mid-prio-executor\0"),
        priority: TASK_MID_PRIORITY,
        ..Default::default()
    }
    .set()?;

    let mid_prio_execution = schedule::<8, _>(50000, move || {
        let executor = EspExecutor::new();
        let mut tasks = heapless::Vec::new();

        executor.spawn_local_collect(process_wifi_state_change(wifi, wifi_notif), &mut tasks)?;

        executor.spawn_local_collect(wind_speed_demo_publisher_task(), &mut tasks)?;

        executor.spawn_local_collect(http_server_task(), &mut tasks)?;

        executor.spawn_local_collect(mqtt::receive_task(mqtt_conn), &mut tasks)?;

        executor.spawn_local_collect(
            process_netif_state_change(netif_notifier(sysloop.clone()).unwrap()),
            &mut tasks,
        )?;

        Ok((executor, tasks))
    });

    ThreadSpawnConfiguration {
        name: Some(b"low-prio-executor\0"),
        priority: TASK_LOW_PRIORITY,
        ..Default::default()
    }
    .set()?;

    let low_prio_execution = schedule::<8, _>(50000, move || {
        let executor = EspExecutor::new();
        let mut tasks = heapless::Vec::new();

        executor.spawn_local_collect(ota_task(), &mut tasks)?;

        executor.spawn_local_collect(
            mqtt::send_task::<MQTT_MAX_TOPIC_LEN>(mqtt_topic_prefix, mqtt_client),
            &mut tasks,
        )?;

        Ok((executor, tasks))
    });

    // This is required to allow the low prio thread to start
    std::thread::sleep(core::time::Duration::from_millis(2000));
    mid_prio_execution.join().unwrap();
    low_prio_execution.join().unwrap();

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

async fn wind_speed_demo_publisher_task() {
    loop {
        let publisher = APPLICATION_EVENT_CHANNEL.publisher().unwrap();
        let data = ApplicationStateChange::NewWindData(WindData {
            speed: 23,
            angle: 180,
        });
        let _ = publisher.publish(data).await;
        Timer::after(Duration::from_secs(10)).await;
    }
}
