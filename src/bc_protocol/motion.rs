use super::Error;
use super::RX_TIMEOUT;
use crate::bc::{model::*, xml::*};
use crate::bc_protocol::connection::BcSubscription;

use log::*;

type Result<T> = std::result::Result<T, Error>;

pub enum MotionStatus {
    MotionStart,
    MotionStop,
    NoChange,
}

pub struct MotionDataSubscriber<'a> {
    bc_sub: &'a BcSubscription<'a>,
    channel_id: u32,
}

impl<'a> MotionDataSubscriber<'a> {
    pub fn from_bc_sub<'b>(
        bc_sub: &'b BcSubscription,
        channel_id: u32,
    ) -> MotionDataSubscriber<'b> {
        MotionDataSubscriber { bc_sub, channel_id }
    }

    pub fn get_motion_status(&self) -> Result<MotionStatus> {
        debug!("GettingMotion");
        let msg_motion = self.bc_sub.rx.recv_timeout(RX_TIMEOUT);
        if let Ok(msg_motion) = msg_motion {
            debug!("GotMotion");
            if let BcBody::ModernMsg(ModernMsg {
                xml: Some(TopBcXmls::BcXml(xml)),
                ..
            }) = msg_motion.body
            {
                if let BcXml {
                    alarm_event_list: Some(alarm_event_list),
                    ..
                } = xml
                {
                    for alarm_event in &alarm_event_list.alarm_events {
                        if alarm_event.channel_id == self.channel_id {
                            if alarm_event.status == "MD" {
                                info!("Got motion MESSAGE");
                                return Ok(MotionStatus::MotionStart);
                            } else if alarm_event.status == "none" {
                                info!("Got motion MESSAGE");
                                return Ok(MotionStatus::MotionStop);
                            }
                        }
                    }
                    info!("Got motion MESSAGE but not one we were looking for");
                } else {
                    debug!("Got motion xml like this: {:?}", xml);
                }
            } else {
                debug!("Got motion like this: {:?}", msg_motion);
            }
        } else {
            error!("Failed to get msg_motion");
        }
        Ok(MotionStatus::NoChange)
    }
}
