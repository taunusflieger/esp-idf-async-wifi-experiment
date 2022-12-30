use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::{pubsub::PubSubChannel, signal::Signal};
use serde::{Deserialize, Serialize};

pub static NETWORK_EVENT_CHANNEL: PubSubChannel<
    CriticalSectionRawMutex,
    NetworkStateChange,
    4,
    4,
    4,
> = PubSubChannel::new();

#[allow(dead_code)]
pub static APPLICATION_EVENT_CHANNEL: PubSubChannel<
    CriticalSectionRawMutex,
    ApplicationStateChange,
    4,
    4,
    4,
> = PubSubChannel::new();

#[derive(Copy, Clone, Debug)]
pub enum NetworkStateChange {
    WifiDisconnected,
    IpAddressAssigned { ip: embedded_svc::ipv4::Ipv4Addr },
}


#[derive(Copy, Clone, Eq, PartialEq, Debug, Serialize, Deserialize)]
pub struct WindData {
    pub speed: u16,
    pub angle: u16,
}

#[derive(Copy, Clone, Debug)]
#[allow(dead_code)]
pub enum ApplicationStateChange {
    OTAUpdateRequest,
    OTAUpdateStarted,
    NewWindData (WindData),
}
