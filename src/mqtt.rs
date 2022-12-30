use crate::error;
use crate::state::*;
use core::str::{self, FromStr};
use embassy_futures::select::{select, Either};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;
use embedded_svc::mqtt::client::asynch::{Client, Connection, Event, Message, Publish, QoS};
use embedded_svc::mqtt::client::Details;
use log::*;
use serde::{Deserialize, Serialize};
type OtaUrl = heapless::String<128>;

static MQTT_CONNECT_SIGNAL: Signal<CriticalSectionRawMutex, bool> = Signal::new();

#[derive(Clone, Eq, PartialEq, Debug, Serialize, Deserialize)]
pub enum MqttCommand {
    ExecOTAUpdate(OtaUrl),
    SystemRestart,
}

#[derive(Default)]
pub struct MessageParser {
    #[allow(clippy::type_complexity)]
    command_parser: Option<fn(&[u8]) -> Option<MqttCommand>>,
    payload_buf: [u8; 16],
}

pub async fn receive(mut connection: impl Connection<Message = Option<MqttCommand>>) {
    loop {
        let message = connection.next().await;

        if let Some(message) = message {
            info!("[MQTT/CONNECTION]: {:?}", message);

            if let Ok(Event::Received(Some(cmd))) = &message {
                match cmd {
                    MqttCommand::ExecOTAUpdate(url) => {
                        info!("MQTT received OTA update request. url = {}", url);
                    }
                    MqttCommand::SystemRestart => {
                        info!("MQTT received system restart request");
                    }
                }
            } else if matches!(&message, Ok(Event::Connected(_))) {
                MQTT_CONNECT_SIGNAL.signal(true);
            } else if matches!(&message, Ok(Event::Disconnected)) {
                MQTT_CONNECT_SIGNAL.signal(false);
            }
        } else {
            info!("mqtt::recveive exit loop");
            break;
        }
    }
}

// send will react on application state change event and then send the MQTT message
// the application state change event will be fired if new wind data is availbale.
// the requence in which MQTT messages are send depends on how often the application
// state change events gets fired.
// we are not implementing explicit re-connect logic, as this is already implemented
// in ESP IDF for MQTT.
pub async fn send<const L: usize>(topic_prefix: &str, mut mqtt: impl Client + Publish) {
    let mut connected = false;

    let topic = |topic_suffix| {
        heapless::String::<L>::from_str(topic_prefix.as_ref())
            .and_then(|mut s| s.push_str(topic_suffix).map(|_| s))
            .unwrap_or_else(|_| panic!(""))
    };

    let topic_commands = topic("/commands/#");

    let topic_wind_speed = topic("/wind/speed");
    let topic_wind_angle = topic("/wind/angle");
    let mut subscriber = APPLICATION_EVENT_CHANNEL.subscriber().unwrap();
    
    loop {
        let (conn_state, app_state_change) =
            match select(MQTT_CONNECT_SIGNAL.wait(), subscriber.next_message_pure()).await {
                Either::First(conn_state) => {
                    info!("MQTT::send recv MQTT_CONNECT_SIGNAL");
                    (Some(conn_state), None)
                }
                Either::Second(app_state_change) => {
                    info!("MQTT::send recv app_state_change");
                    (None, Some(app_state_change))
                }
            };

        if let Some(conn_state) = conn_state {
            if conn_state {
                info!("MQTT is now connected, subscribing");

                mqtt.subscribe(topic_commands.as_str(), QoS::AtLeastOnce)
                    .await
                    .unwrap();

                connected = true;
            } else {
                info!("MQTT disconnected");

                connected = false;
            }
        }
        if let Some(app_state_change) = app_state_change {
            match app_state_change {
                ApplicationStateChange::NewWindData(wind_data) => {
                    info!("mqtt::send new wind data {}", wind_data.speed);
                    let s = format!("{}",wind_data.speed);
                    
                    let output = s.as_str().as_bytes();

                    publish(
                        connected,
                        &mut mqtt,
                        &topic_wind_speed,
                        QoS::AtLeastOnce,
                        &output,
                    )
                    .await;
                }
                _ => {
                    info!("nothing to do");
                }
            }
        }
    }
}

async fn publish(connected: bool, mqtt: &mut impl Publish, topic: &str, qos: QoS, payload: &[u8]) {
    if connected {
        if let Ok(_msg_id) = error::check!(mqtt.publish(topic, qos, false, payload).await) {
            // TODO
            info!("Published to {}", topic);
        }
    } else {
        error!("Client not connected, skipping publishment to {}", topic);
    }
}

impl MessageParser {
    pub fn new() -> Self {
        Default::default()
    }

    pub fn convert<M, E>(
        &mut self,
        event: &Result<Event<M>, E>,
    ) -> Result<Event<Option<MqttCommand>>, E>
    where
        M: Message,
        E: Clone,
    {
        event
            .as_ref()
            .map(|event| event.transform_received(|message| self.process(message)))
            .map_err(|e| e.clone())
    }

    fn process<M>(&mut self, message: &M) -> Option<MqttCommand>
    where
        M: Message,
    {
        match message.details() {
            Details::Complete => Self::parse_command(message.topic().unwrap())
                .and_then(|parser| parser(message.data())),
            Details::InitialChunk(initial_chunk_data) => {
                if initial_chunk_data.total_data_size > self.payload_buf.len() {
                    self.command_parser = None;
                } else {
                    self.command_parser = Self::parse_command(message.topic().unwrap());

                    self.payload_buf[..message.data().len()]
                        .copy_from_slice(message.data().as_ref());
                }

                None
            }
            Details::SubsequentChunk(subsequent_chunk_data) => {
                if let Some(command_parser) = self.command_parser.as_ref() {
                    self.payload_buf
                        [subsequent_chunk_data.current_data_offset..message.data().len()]
                        .copy_from_slice(message.data().as_ref());

                    if subsequent_chunk_data.total_data_size
                        == subsequent_chunk_data.current_data_offset + message.data().len()
                    {
                        command_parser(&self.payload_buf[0..subsequent_chunk_data.total_data_size])
                    } else {
                        None
                    }
                } else {
                    None
                }
            }
        }
    }

    #[allow(clippy::type_complexity)]
    fn parse_command(topic: &str) -> Option<fn(&[u8]) -> Option<MqttCommand>> {
        if topic.ends_with("/commands/ota_update") {
            Some(Self::parse_ota_update_command)
        } else if topic.ends_with("/commands/system_restart") {
            Some(Self::parse_system_restart_command)
        } else {
            None
        }
    }

    fn parse_ota_update_command(data: &[u8]) -> Option<MqttCommand> {
        Self::parse::<OtaUrl>(data).map(MqttCommand::ExecOTAUpdate)
    }

    fn parse_system_restart_command(data: &[u8]) -> Option<MqttCommand> {
        Self::parse_empty(data).map(|_| MqttCommand::SystemRestart)
    }

    fn parse<T>(data: &[u8]) -> Option<T>
    where
        T: str::FromStr,
    {
        str::from_utf8(data)
            .ok()
            .and_then(|s| str::parse::<T>(s).ok())
    }

    fn parse_empty(data: &[u8]) -> Option<()> {
        if data.is_empty() {
            Some(())
        } else {
            None
        }
    }
}
