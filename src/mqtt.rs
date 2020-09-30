use super::MotionStatus;
use crossbeam_channel::{unbounded, Receiver, Sender};
use crossbeam_utils::thread::Scope;
use librumqttd::{Broker, Config, LinkTx};
use std::sync::Mutex;
use tokio::task;

use log::*;

#[allow(dead_code)]
pub struct MQTT {
    link: Option<Mutex<LinkTx>>,
}

impl MQTT {
    #[allow(dead_code)]
    pub fn new(s: &Scope<'_>, config: &Option<Config>) -> Self {
        let link;
        if let Some(mqtt_config) = &config {
            let broker = Broker::new(mqtt_config.clone());

            let pubtx = broker.link("localclientpub", 2).unwrap();

            s.spawn(move |_| {
                let mut rt = tokio::runtime::Runtime::new().unwrap();
                rt.block_on(Self::run_mqtt_server(broker));
            });

            link = Some(Mutex::new(pubtx));
        } else {
            link = None;
        }

        Self { link }
    }

    #[allow(dead_code)]
    pub fn send_message(&self, topic: &str, message: &str) {
        if let Some(link) = &self.link {
            let mut rt = tokio::runtime::Runtime::new().unwrap();
            let mut link = link.lock().unwrap();
            let _ = rt.block_on(Self::publish(&mut link, topic, message));
        }
    }

    #[allow(dead_code)]
    async fn publish(link: &mut LinkTx, topic: &str, message: &str) {
        link.connect().await.unwrap();
        let res = link.publish(topic, true, message).await;
        if res.is_err() {
            error!(
                "Failed to publish on mqtt, topic: {}, message: {}",
                topic, message
            );
        }
    }

    #[allow(dead_code)]
    async fn get_messages(mut tx: LinkTx) {
        let mut rx = tx.connect().await.unwrap();
        tx.subscribe("#").await.unwrap();
        loop {
            if let Some(message) = rx.recv().await.unwrap() {
                debug!(
                    "Incoming. Topic = {}, Payload = {:?}",
                    message.topic, message.payload
                );
            }
        }
    }

    #[allow(dead_code)]
    async fn run_mqtt_server(mut broker: Broker) {
        info!("Starting MQTT Server");
        let tx = broker.link("localclient", 10).unwrap();
        task::spawn(Self::get_messages(tx));
        broker.start().await.unwrap();
    }
}

pub struct MotionWriter {
    topic: String,
    receiver: Receiver<MotionStatus>,
}

impl<'a> MotionWriter {
    pub fn new_with_tx(cam_name: &str) -> (Self, Sender<MotionStatus>) {
        let (sender, receiver) = unbounded::<MotionStatus>();
        let me = Self {
            topic: format!("/neolink/{}/status/motion", cam_name),
            receiver,
        };
        (me, sender)
    }

    pub fn poll_status(&self, mqtt: &MQTT) {
        let data = self.receiver.recv().expect("We should get something");
        match data {
            MotionStatus::MotionStart => {
                mqtt.send_message(&self.topic, "on");
            }
            MotionStatus::MotionStop => mqtt.send_message(&self.topic, "off"),
            _ => {}
        }
    }
}
