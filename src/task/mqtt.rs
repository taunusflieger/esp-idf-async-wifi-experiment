use crate::error;
use crate::mqtt_msg::{
    MqttCommand, MQTT_TOPIC_POSTFIX_COMMAND, MQTT_TOPIC_POSTFIX_WIND_DIRECTION,
    MQTT_TOPIC_POSTFIX_WIND_SPEED,
};
use crate::state::*;
use core::str::{self, FromStr};
use embassy_futures::select::{select, Either};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;
use embedded_svc::mqtt::client::asynch::{Client, Connection, Event, Publish, QoS};
use log::*;

static MQTT_CONNECT_SIGNAL: Signal<CriticalSectionRawMutex, bool> = Signal::new();

pub async fn receive_task(mut connection: impl Connection<Message = Option<MqttCommand>>) {
    loop {
        let message = connection.next().await;

        if let Some(message) = message {
            info!("[MQTT/CONNECTION]: {:?}", message);

            if let Ok(Event::Received(Some(cmd))) = &message {
                match cmd {
                    MqttCommand::ExecOTAUpdate(url) => {
                        info!("MQTT received OTA update request. url = {}", url);
                        let publisher = APPLICATION_EVENT_CHANNEL.publisher().unwrap();
                        let data = ApplicationStateChange::OTAUpdateRequest(url.clone());
                        let _ = publisher.publish(data).await;
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
pub async fn send_task<const L: usize>(topic_prefix: &str, mut mqtt: impl Client + Publish) {
    let mut connected = false;

    let topic = |topic_suffix| {
        heapless::String::<L>::from_str(topic_prefix)
            .and_then(|mut s| s.push_str(topic_suffix).map(|_| s))
            .unwrap_or_else(|_| panic!("failed to construct topic"))
    };

    let topic_commands = topic(MQTT_TOPIC_POSTFIX_COMMAND);
    let topic_wind_speed = topic(MQTT_TOPIC_POSTFIX_WIND_SPEED);
    #[allow(unused)]
    let topic_wind_angle = topic(MQTT_TOPIC_POSTFIX_WIND_DIRECTION);

    let mut app_event = APPLICATION_EVENT_CHANNEL.subscriber().unwrap();

    loop {
        let (conn_state, app_state_change) =
            match select(MQTT_CONNECT_SIGNAL.wait(), app_event.next_message_pure()).await {
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

        if let Some(ApplicationStateChange::NewWindData(wind_data)) = app_state_change {
            info!("mqtt::send new wind data {}", wind_data.speed);

            if connected {
                if let Ok(_msg_id) = error::check!(
                    mqtt.publish(
                        &topic_wind_speed,
                        QoS::AtLeastOnce,
                        false,
                        format!("{}", wind_data.speed).as_str().as_bytes()
                    )
                    .await
                ) {
                    info!("Published to {}", topic_wind_speed);
                }
            } else {
                error!(
                    "Client not connected, skipping publishment to {}",
                    topic_wind_speed
                );
            }
        }
    }
}
