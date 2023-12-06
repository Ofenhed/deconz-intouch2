use std::{collections::HashMap, sync::Arc};

use mqttrs::{Packet, Publish, QoS, QosPid, SubscribeTopic};
use serde::Deserialize;
use tokio::{sync::RwLock, task::JoinSet};

use crate::{
    home_assistant,
    mqtt_session::{MqttError, Session as MqttSession, Topic},
    spa::{SpaCommand, SpaConnection, SpaError},
};

#[derive(Deserialize)]
pub struct Entity<T> {
    pub entity: T,
    pub id: String,
    pub name: String,
}

#[derive(Deserialize)]
pub enum Light {
    RGB {
        red: usize,
        green: usize,
        blue: usize,
    },
    Dimmer(Box<Light>),
}

#[derive(Deserialize)]
pub struct Pump {}

#[derive(Deserialize)]
pub struct Climate {}

#[derive(Deserialize)]
pub enum Entities {
    Light(Entity<Light>),
    Pump(Entity<Pump>),
    Climate(Entity<Climate>),
}

#[derive(Deserialize)]
pub struct Device {
    pub id: String,
    pub name: String,
    pub entities: Entities,
}

#[derive(Deserialize)]
pub struct Config {
    pub entities: Vec<Device>,
}

#[derive(thiserror::Error, Debug)]
pub enum MappingError {
    #[error(transparent)]
    Mqtt(#[from] MqttError),
    #[error(transparent)]
    Spa(#[from] SpaError),
    #[error("Could not communicate with Spa service: {0}")]
    SpaCommand(#[from] tokio::sync::mpsc::error::SendError<SpaCommand>),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("Tokio channel error: {0}")]
    BroadcastRecv(#[from] tokio::sync::broadcast::error::RecvError),
    #[error("Runtime error: {0}")]
    Runtime(#[from] tokio::task::JoinError),
}

pub struct Mapping {
    device: home_assistant::ConfigureDevice,
    jobs: JoinSet<Result<(), MappingError>>,
}

#[derive(serde::Deserialize, Debug, Clone, serde::Serialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub enum SpecialMode {
    WatercareMode,
}

#[derive(serde::Deserialize, Debug, Clone, serde::Serialize)]
#[serde(deny_unknown_fields, untagged)]
pub enum MappingType {
    U8 { u8_addr: u16 },
    U16 { u16_addr: u16 },
    Array { addr: u16, len: u16 },
    Special(SpecialMode),
}

#[derive(serde::Deserialize, Debug, Clone, serde::Serialize)]
#[serde(deny_unknown_fields, untagged)]
pub enum CommandMappingType {
    // U8 { u8_addr: u16 },
    // U16 { u16_addr: u16 },
    // Array { addr: u16, len: u16 },
    // Key { },
    Special(SpecialMode),
}

#[cfg(test)]
mod more_tests {
    #[test]
    fn create_mqtt_type() -> anyhow::Result<()> {
        let parsed = serde_json::from_str(r#"{"state": "watercare_mode"}"#)?;
        assert!(matches!(
            parsed,
            super::MqttType::State {
                state: super::MappingType::Special(super::SpecialMode::WatercareMode),
            }
        ));
        Ok(())
    }
}

impl MappingType {
    pub fn range(&self) -> Option<std::ops::Range<usize>> {
        let start = match self {
            Self::U8 { u8_addr: start }
            | Self::U16 { u16_addr: start }
            | Self::Array { addr: start, .. } => usize::from(*start),
            Self::Special(_) => return None,
        };
        let len = match self {
            Self::U8 { .. } => 1,
            Self::U16 { .. } => 2,
            Self::Array { len, .. } => usize::from(*len),
            Self::Special(_) => unreachable!(),
        };
        let end = start + len;
        Some(start..end)
    }
}

#[derive(serde::Deserialize, Debug, Clone, serde::Serialize)]
#[serde(deny_unknown_fields, untagged)]
pub enum MqttType {
    State { state: MappingType },
    Command { command: CommandMappingType },
    Value(serde_json::Value),
}

#[derive(serde::Deserialize, Debug, Clone)]
pub struct GenericMapping {
    #[serde(rename = "type")]
    pub mqtt_type: &'static str,
    pub name: &'static str,
    pub unique_id: &'static str,
    #[serde(flatten)]
    pub mqtt_values: HashMap<&'static str, MqttType>,
}

#[cfg(test)]
mod tests {
    #[test]
    fn barebone_generic() -> anyhow::Result<()> {
        let mapping: super::GenericMapping = serde_json::from_str(
            r#"{"type": "light", "name": "Some light", "unique_id": "light0001"}"#,
        )?;
        eprintln!("Mapping was {mapping:?}");
        Ok(())
    }
    #[test]
    fn with_custom_values() -> anyhow::Result<()> {
        let mapping: super::GenericMapping = serde_json::from_str(
            r#"{"type": "light", "name": "Some light", "unique_id": "light0001", "optimistic": false}"#,
        )?;
        eprintln!("Mapping was {mapping:?}");
        Ok(())
    }
    #[test]
    fn with_custom_values_early() -> anyhow::Result<()> {
        let mapping: super::GenericMapping = serde_json::from_str(
            r#"{"type": "light", "optimistic": false, "name": "Some light", "unique_id": "light0001"}"#,
        )?;
        eprintln!("Mapping was {mapping:?}");
        Ok(())
    }
    #[test]
    fn with_fetcher() -> anyhow::Result<()> {
        let mapping: super::GenericMapping = serde_json::from_str(
            r#"{"type": "light", "name": "Some light", "unique_id": "light0001", "state_topic": {"state": {"u8_addr": 100}}}"#,
        )?;
        eprintln!("Mapping was {mapping:?}");
        Ok(())
    }
}

impl GenericMapping {
    pub fn config_is_static(&self) -> bool {
        true
    }
}

impl Mapping {
    pub async fn add_generic(
        &mut self,
        mapping: GenericMapping,
        spa: &SpaConnection,
        mqtt: &mut MqttSession,
    ) -> Result<(), MappingError> {
        let config_topic = mqtt.topic(&mapping.mqtt_type, &mapping.unique_id, Topic::Config);
        let mut counter = 0;
        let topics = mqtt.topic_generator();
        let GenericMapping {
            mqtt_type,
            name: mqtt_name,
            unique_id,
            mqtt_values,
        } = mapping;
        let mut next_state_topic = || {
            counter += 1;
            topics.topic(&mqtt_type, &format!("{unique_id}/{counter}"), Topic::State)
        };

        let device = self.device.clone();
        let mqtt_waiter = Arc::new(RwLock::new(()));
        let config_not_published = mqtt_waiter.clone().write_owned().await;
        let json_config = {
            let mut config = home_assistant::ConfigureGeneric {
                base: home_assistant::ConfigureBase {
                    name: &mqtt_name,
                    unique_id: &unique_id,
                    device: &device,
                },
                args: Default::default(),
            };
            for (key, value) in &mqtt_values {
                match value {
                    MqttType::State { state } => {
                        let topic = next_state_topic();
                        {
                            let topic = topic.clone();
                            let state = state.clone();
                            let mut sender = mqtt.sender();
                            let mut data_subscription = if let Some(range) = state.range() {
                                Some(spa.subscribe(range).await)
                            } else {
                                None
                            };
                            let mut mode_subscription = if matches!(
                                state,
                                MappingType::Special(SpecialMode::WatercareMode)
                            ) {
                                Some(spa.subscribe_watercare_mode().await)
                            } else {
                                None
                            };
                            let mqtt_config_complete = mqtt_waiter.clone();
                            self.jobs.spawn(async move {
                                mqtt_config_complete.read_owned().await;
                                loop {
                                    let reported_value = match (
                                        &state,
                                        data_subscription.as_mut().map(|x| x.borrow_and_update()),
                                        mode_subscription.as_mut().map(|x| x.borrow_and_update()),
                                    ) {
                                        (
                                            MappingType::Special(SpecialMode::WatercareMode),
                                            None,
                                            Some(mode),
                                        ) => match mode.as_ref() {
                                            Some(mode) => serde_json::Value::Number((*mode).into()),
                                            None => serde_json::Value::Null,
                                        },
                                        (MappingType::U8 { .. }, Some(data), None) => {
                                            let new_value: &[u8; 1] = data
                                                .as_ref()
                                                .try_into()
                                                .expect("This will always be 1 byte");
                                            serde_json::Value::Number(new_value[0].into())
                                        }
                                        (MappingType::U16 { .. }, Some(data), None) => {
                                            let new_value: &[u8; 2] = data
                                                .as_ref()
                                                .try_into()
                                                .expect("This will always be 2 bytes");
                                            serde_json::Value::Number(
                                                u16::from_be_bytes(*new_value).into(),
                                            )
                                        }
                                        (MappingType::Array { .. }, Some(data), None) => {
                                            serde_json::Value::Array(
                                                data.iter()
                                                    .map(|x| serde_json::Value::Number((*x).into()))
                                                    .collect(),
                                            )
                                        }
                                        (..) => unreachable!("All valid modes handled"),
                                    };
                                    let payload = serde_json::to_vec(&reported_value)?;
                                    let package = Packet::Publish(Publish {
                                        dup: false,
                                        qospid: QosPid::AtMostOnce,
                                        retain: false,
                                        topic_name: &topic,
                                        payload: &payload,
                                    });
                                    sender.send(&package).await?;
                                    if let Some(subscription) = &mut data_subscription {
                                        subscription.changed().await.unwrap();
                                    } else if let Some(subscription) = &mut mode_subscription {
                                        subscription.changed().await.unwrap();
                                    } else {
                                        return Ok(());
                                    }
                                }
                            });
                        }
                        config.args.insert(key.as_ref(), topic.into())
                    }
                    MqttType::Command { command } => {
                        let topic = next_state_topic();
                        mqtt.mqtt_subscribe(vec![SubscribeTopic {
                            topic_path: topic.clone(),
                            qos: QoS::AtMostOnce,
                        }])
                        .await?;
                        let mut receiver = mqtt.subscribe();
                        let spa_sender = spa.sender();
                        {
                            let topic = topic.clone();
                            let command = command.clone();
                            self.jobs.spawn(async move {
                                loop {
                                    match (&command, &receiver.recv().await?.as_ref().packet) {
                                        (
                                            CommandMappingType::Special(SpecialMode::WatercareMode),
                                            Packet::Publish(Publish {
                                                dup: false,
                                                topic_name,
                                                payload,
                                                ..
                                            }),
                                        ) if topic_name == &&topic => {
                                            let Ok(valid_str) =
                                                String::from_utf8(Vec::from(*payload))
                                            else {
                                                eprintln!("Invalid payload from MQTT: {payload:?}");
                                                continue;
                                            };
                                            let Ok(mode) = valid_str.parse() else {
                                                eprintln!("Invalid payload from MQTT: {valid_str}");
                                                continue;
                                            };
                                            spa_sender.send(SpaCommand::SetWatercare(mode)).await?;
                                        }
                                        _ => (),
                                    };
                                }
                            });
                        }
                        config.args.insert(key.as_ref(), topic.into())
                    }
                    MqttType::Value(value) => config.args.insert(key.as_ref(), value.clone()),
                };
            }
            serde_json::to_vec(&config)?
        };
        let config_packet = Packet::Publish(Publish {
            dup: false,
            qospid: QosPid::AtMostOnce,
            retain: false,
            topic_name: &config_topic,
            payload: &json_config,
        });
        mqtt.send(config_packet).await?;
        drop(config_not_published);
        Ok(())
    }

    pub async fn tick(&mut self) -> Result<(), MappingError> {
        if let Some(join_result) = self.jobs.join_next().await {
            _ = join_result?;
        }
        Ok(())
    }
}

impl Mapping {
    pub fn new(device: home_assistant::ConfigureDevice) -> Result<Self, MappingError> {
        let jobs = JoinSet::new();
        Ok(Self { jobs, device })
    }
}
