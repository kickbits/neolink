//! This module provides an "RtspServer" abstraction that allows consumers of its API to feed it
//! data using an ordinary std::io::Write interface.
pub use self::maybe_app_src::MaybeAppSrc;

use gstreamer::prelude::Cast;
use gstreamer::ClockTime;
use gstreamer::{Bin, Structure};
use gstreamer_app::AppSrc;
//use gstreamer_rtsp::RTSPLowerTrans;
use gio::{TlsAuthenticationMode, TlsCertificate};
use gstreamer_rtsp::RTSPAuthMethod;
use gstreamer_rtsp_server::prelude::*;
use gstreamer_rtsp_server::{
    RTSPAuth, RTSPMediaFactory, RTSPPublishClockMode, RTSPServer as GstRTSPServer, RTSPToken,
    RTSP_PERM_MEDIA_FACTORY_ACCESS, RTSP_PERM_MEDIA_FACTORY_CONSTRUCT,
    RTSP_TOKEN_MEDIA_FACTORY_ROLE,
};
use itertools::Itertools;
use log::*;
use std::collections::HashSet;
use std::fs;
use std::io;
use std::io::Write;

type Result<T> = std::result::Result<T, ()>;

pub struct RtspServer {
    server: GstRTSPServer,
}

#[derive(Debug, PartialEq, Eq, Hash, Copy, Clone)]
pub enum StreamFormat {
    H264,
    H265,
    AAC,
    ADPCM,
}

pub struct GstOutputs {
    pub audsrc: MaybeAppSrc,
    pub vidsrc: MaybeAppSrc,
    video_format: Option<StreamFormat>,
    audio_format: Option<StreamFormat>,
    factory: RTSPMediaFactory,
}

impl GstOutputs {
    pub fn from_appsrcs(vidsrc: MaybeAppSrc, audsrc: MaybeAppSrc) -> GstOutputs {
        let result = GstOutputs {
            vidsrc,
            audsrc,
            video_format: None,
            audio_format: None,
            factory: RTSPMediaFactory::new(),
        };
        result.apply_format();
        result
    }

    pub fn set_format(&mut self, format: Option<StreamFormat>) {
        match format {
            Some(StreamFormat::H264) | Some(StreamFormat::H265) => {
                if format != self.video_format {
                    self.video_format = format;
                    self.apply_format();
                }
            }
            Some(StreamFormat::AAC) | Some(StreamFormat::ADPCM) => {
                if format != self.audio_format {
                    self.audio_format = format;
                    self.apply_format();
                }
            }
            _ => {}
        }
    }

    fn apply_format(&self) {
        let launch_vid = match self.video_format {
            Some(StreamFormat::H264) => {
                // My E1 camera has a full caps of video/x-h264, width=(int)896, height=(int)512, framerate=(fraction)0/1, chroma-format=(string)4:2:0, bit-depth-luma=(uint)8, bit-depth-chroma=(uint)8, parsed=(boolean)true, stream-format=(string)avc, alignment=(string)au, profile=(string)high, level=(string)5.1, codec_data=(buffer)01640033ffe1000a67640033ace80e01064001000468ee3cb0
                // The h264payloader only requires video/x-h264,stream-format=avc,alignment=au
                // I can set this in the caps field of the appsrc and skip the h264parse
                // However, although I can payload using the minimum caps it is not properly recieved on the other end without a full parse and will not play.
                "format=GST_FORMAT_TIME ! queue silent=true max-size-bytes=10485760 ! h264parse ! rtph264pay name=pay0"
            }
            Some(StreamFormat::H265) => {
                "format=GST_FORMAT_TIME ! queue silent=true  max-size-bytes=10485760 ! h265parse ! rtph265pay name=pay0"
            }
            _ => "! fakesink",
        };

        let launch_aud = match self.audio_format {
            // We set the caps for the pcm here to avoid the parser entirly.
            // This is required as the rawaudioparse in gstramer seems to be bugged for live sources
            // See https://gitlab.freedesktop.org/gstreamer/gst-plugins-base/-/issues/353
            Some(StreamFormat::ADPCM) => "format=GST_FORMAT_TIME caps=audio/x-raw,format=S16LE,layout=interleaved,channels=1,rate=8000 ! queue silent=true max-size-bytes=10485760 min-threshold-bytes=10 ! audiorate ! audioconvert ! rtpL16pay name=pay1", // We decode as oki adpcm to raw before the appsink then convert to BigEndian for the rtp transport
            // My E1 camera has a full caps of audio/mpeg, framed=(boolean)true, mpegversion=(int)4, level=(string)1, base-profile=(string)lc, profile=(string)lc, rate=(int)16000, channels=(int)1, stream-format=(string)raw, codec_data=(buffer)1408
            // But only caps=audio/mpeg,mpegversion=4,stream-format=raw are required by the payloader
            Some(StreamFormat::AAC) => "format=GST_FORMAT_TIME caps=audio/mpeg,mpegversion=4,stream-format=raw ! queue silent=true max-size-bytes=10485760 min-threshold-bytes=10 ! rtpmp4apay name=pay1",
            _ => "! fakesink",
        };

        self.factory.set_launch(&vec![
            "( ",
            "appsrc name=vidsrc is-live=true block=true emit-signals=false max-bytes=52428800", // 50MB max size so that it won't grow to infinite if the queue blocks
            launch_vid,
            "appsrc name=audsrc is-live=true block=true emit-signals=false max-bytes=52428800", // 50MB max size so that it won't grow to infinite if the queue blocks
            launch_aud,
            ")"
        ].join(" "));
    }

    pub fn set_timestamp(&mut self, timestamp: Option<u64>) {
        if timestamp.is_some() {
            self.audsrc.set_timestamp(timestamp);
            self.vidsrc.set_timestamp(timestamp);
        }
    }
}

impl RtspServer {
    pub fn new() -> RtspServer {
        gstreamer::init().expect("Gstreamer should not explode");
        RtspServer {
            server: GstRTSPServer::new(),
        }
    }

    pub fn add_stream(
        &self,
        paths: &[&str],
        permitted_users: &HashSet<&str>,
    ) -> Result<GstOutputs> {
        let mounts = self
            .server
            .get_mount_points()
            .expect("The server should have mountpoints");

        // Create a MaybeAppSrc: Write which we will give the caller.  When the backing AppSrc is
        // created by the factory, fish it out and give it to the waiting MaybeAppSrc via the
        // channel it provided.  This callback may be called more than once by Gstreamer if it is
        // unhappy with the pipeline, so keep updating the MaybeAppSrc.
        let (maybe_app_src, tx) = MaybeAppSrc::new_with_tx();
        let (maybe_app_src_aud, tx_aud) = MaybeAppSrc::new_with_tx();

        let outputs = GstOutputs::from_appsrcs(maybe_app_src, maybe_app_src_aud);

        let factory = &outputs.factory;

        debug!(
            "Permitting {} to access {}",
            // This is hashmap or (iter) equivalent of join, it requres itertools
            permitted_users
                .iter()
                .cloned()
                .intersperse(", ")
                .collect::<String>(),
            paths.join(", ")
        );
        self.add_permitted_roles(factory, permitted_users);

        factory.set_shared(true);
        factory.set_publish_clock_mode(RTSPPublishClockMode::ClockAndOffset);

        factory.connect_media_configure(move |_factory, media| {
            debug!("RTSP: media was configured");
            media.use_time_provider(true);
            let bin = media
                .get_element()
                .expect("Media should have an element")
                .dynamic_cast::<Bin>()
                .expect("Media source's element should be a bin");
            let app_src = bin
                .get_by_name_recurse_up("vidsrc")
                .expect("write_src must be present in created bin")
                .dynamic_cast::<AppSrc>()
                .expect("Source element is expected to be an appsrc!");
            let _ = tx.send(app_src); // Receiver may be dropped, don't panic if so

            let app_src_aud = bin
                .get_by_name_recurse_up("audsrc")
                .expect("write_src must be present in created bin")
                .dynamic_cast::<AppSrc>()
                .expect("Source element is expected to be an appsrc!");
            let _ = tx_aud.send(app_src_aud); // Receiver may be dropped, don't panic if so
        });

        for path in paths {
            mounts.add_factory(path, factory);
        }

        Ok(outputs)
    }

    pub fn add_permitted_roles(&self, factory: &RTSPMediaFactory, permitted_roles: &HashSet<&str>) {
        for permitted_role in permitted_roles {
            factory.add_role_from_structure(&Structure::new(
                permitted_role,
                &[
                    (*RTSP_PERM_MEDIA_FACTORY_ACCESS, &true),
                    (*RTSP_PERM_MEDIA_FACTORY_CONSTRUCT, &true),
                ],
            ));
        }
        // During auth, first it binds anonymously. At this point it checks
        // RTSP_PERM_MEDIA_FACTORY_ACCESS to see if anyone can connect
        // This is done before the auth token is loaded, possibliy an upstream bug there
        // After checking RTSP_PERM_MEDIA_FACTORY_ACCESS anonymously
        // It loads the auth token of the user and checks that users
        // RTSP_PERM_MEDIA_FACTORY_CONSTRUCT allowing them to play
        // As a result of this we must ensure that if anonymous is not granted RTSP_PERM_MEDIA_FACTORY_ACCESS
        // As a part of permitted users then we must allow it to access
        // at least RTSP_PERM_MEDIA_FACTORY_ACCESS but not RTSP_PERM_MEDIA_FACTORY_CONSTRUCT
        // Watching Actually happens during RTSP_PERM_MEDIA_FACTORY_CONSTRUCT
        // So this should be OK to do.
        // FYI: If no RTSP_PERM_MEDIA_FACTORY_ACCESS then server returns 404 not found
        //      If yes RTSP_PERM_MEDIA_FACTORY_ACCESS but no RTSP_PERM_MEDIA_FACTORY_CONSTRUCT
        //        server returns 401 not authourised
        if !permitted_roles.contains(&"anonymous") {
            factory.add_role_from_structure(&Structure::new(
                "anonymous",
                &[(*RTSP_PERM_MEDIA_FACTORY_ACCESS, &true)],
            ));
        }
    }

    pub fn set_credentials(&self, credentials: &[(&str, &str)]) -> Result<()> {
        let auth = self.server.get_auth().unwrap_or_else(|| RTSPAuth::new());
        auth.set_supported_methods(RTSPAuthMethod::Basic);

        let mut un_authtoken = RTSPToken::new(&[(*RTSP_TOKEN_MEDIA_FACTORY_ROLE, &"anonymous")]);
        auth.set_default_token(Some(&mut un_authtoken));

        for credential in credentials {
            let (user, pass) = credential;
            trace!("Setting credentials for user {}", user);
            let token = RTSPToken::new(&[(*RTSP_TOKEN_MEDIA_FACTORY_ROLE, user)]);
            let basic = RTSPAuth::make_basic(user, pass);
            auth.add_basic(basic.as_str(), &token);
        }

        self.server.set_auth(Some(&auth));
        Ok(())
    }

    pub fn set_tls(&self, cert_file: &str, client_auth: TlsAuthenticationMode) -> Result<()> {
        debug!("Setting up TLS using {}", cert_file);
        let auth = self.server.get_auth().unwrap_or_else(|| RTSPAuth::new());

        // We seperate reading the file and changing to a PEM so that we get different error messages.
        let cert_contents = fs::read_to_string(cert_file).expect("TLS file not found");
        let cert = TlsCertificate::from_pem(&cert_contents).expect("Not a valid TLS certificate");
        auth.set_tls_certificate(Some(&cert));
        auth.set_tls_authentication_mode(client_auth);

        self.server.set_auth(Some(&auth));
        Ok(())
    }

    pub fn run(&self, bind_addr: &str, bind_port: u16) {
        self.server.set_address(bind_addr);
        self.server.set_service(&format!("{}", bind_port));
        // Attach server to default Glib context
        self.server.attach(None);

        // Run the Glib main loop.
        let main_loop = glib::MainLoop::new(None, false);
        main_loop.run();
    }
}

mod maybe_app_src {
    use super::*;
    use std::sync::mpsc::{sync_channel, Receiver, SyncSender};

    /// A Write implementation around AppSrc that also allows delaying the creation of the AppSrc
    /// until later, discarding written data until the AppSrc is provided.
    pub struct MaybeAppSrc {
        rx: Receiver<AppSrc>,
        app_src: Option<AppSrc>,
        timestamp: Option<u64>,
        basetime: Option<u64>,
    }

    impl MaybeAppSrc {
        /// Creates a MaybeAppSrc.  Also returns a Sender that you must use to provide an AppSrc as
        /// soon as one is available.  When it is received, the MaybeAppSrc will start pushing data
        /// into the AppSrc when write() is called.
        pub fn new_with_tx() -> (Self, SyncSender<AppSrc>) {
            let (tx, rx) = sync_channel(3); // The sender should not send very often
            (
                MaybeAppSrc {
                    rx,
                    app_src: None,
                    timestamp: None,
                    basetime: None,
                },
                tx,
            )
        }

        /// Flushes data to Gstreamer on a problem communicating with the underlying video source.
        pub fn on_stream_error(&mut self) {
            if let Some(src) = self.try_get_src() {
                // Ignore "errors" from Gstreamer such as FLUSHING, which are not really errors.
                let _ = src.end_of_stream();
            }
        }

        /// Attempts to retrieve the AppSrc that should be passed in by the caller of new_with_tx
        /// at some point after this struct has been created.  At that point, we swap over to
        /// owning the AppSrc directly.  This function handles either case and returns the AppSrc,
        /// or None if the caller has not yet sent one.
        fn try_get_src(&mut self) -> Option<&AppSrc> {
            while let Some(src) = self.rx.try_recv().ok() {
                self.app_src = Some(src);
                // When we recieve a new media it means we have a new client we need to reset
                // The basetime, else we will send them frames marked as furture frames which
                // May cause issues for some players that will wait until the play time equals
                // that given in the stream
                self.basetime = None;
            }
            self.app_src.as_ref()
        }

        pub fn set_timestamp(&mut self, timestamp: Option<u64>) {
            self.timestamp = timestamp;
            if self.basetime.is_none() {
                debug!("Resetting basetime to: {}", timestamp.unwrap());
                self.basetime = timestamp; // This is zero time
            }
        }
    }

    impl Write for MaybeAppSrc {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            let pts = match (self.timestamp, self.basetime) {
                (Some(timestamp), Some(basetime)) => Some(timestamp - basetime),
                _ => None,
            };

            // If we have no AppSrc yet, throw away the data and claim that it was written
            let app_src = match self.try_get_src() {
                Some(src) => src,
                None => return Ok(buf.len()),
            };

            let mut gst_buf = gstreamer::Buffer::with_size(buf.len()).unwrap();
            {
                let gst_buf_mut = gst_buf.get_mut().unwrap();
                if let Some(pts) = pts {
                    let clocktime = ClockTime::from_useconds(pts);
                    gst_buf_mut.set_pts(clocktime);
                    gst_buf_mut.set_dts(clocktime);
                }

                let mut gst_buf_data = gst_buf_mut.map_writable().unwrap();
                gst_buf_data.copy_from_slice(buf);
            }

            let res = app_src.push_buffer(gst_buf); //.map_err(|e| io::Error::new(io::ErrorKind::Other, Box::new(e)))?;
            if res.is_err() {
                self.app_src = None;
            }
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
}
