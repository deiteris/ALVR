use crate::decoder::DecoderInitConfig;
use alvr_common::{
    parking_lot::{Condvar, Mutex},
    prelude::*,
    RelaxedAtomic,
};
use alvr_session::{CodecType, MediacodecDataType};
use jni::{
    objects::{JObject, JString},
    sys::jobject,
    JavaVM,
};
use ndk::{
    hardware_buffer::HardwareBufferUsage,
    media::{
        image_reader::{Image, ImageFormat, ImageReader},
        media_codec::{
            MediaCodec, MediaCodecDirection, MediaCodecInfo, MediaCodecResult, MediaFormat,
        },
    },
};
use std::{
    collections::VecDeque,
    ffi::{c_void, CStr},
    net::{IpAddr, Ipv4Addr},
    ops::Deref,
    sync::Arc,
    thread::{self, JoinHandle},
    time::Duration,
};

const MICROPHONE_PERMISSION: &str = "android.permission.RECORD_AUDIO";
const IMAGE_READER_DEADLOCK_TIMEOUT: Duration = Duration::from_millis(100);

struct FakeThreadSafe<T>(T);
unsafe impl<T> Send for FakeThreadSafe<T> {}
unsafe impl<T> Sync for FakeThreadSafe<T> {}

impl<T> Deref for FakeThreadSafe<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.0
    }
}

type SharedMediaCodec = Arc<FakeThreadSafe<MediaCodec>>;

pub fn vm() -> JavaVM {
    unsafe { JavaVM::from_raw(ndk_context::android_context().vm().cast()).unwrap() }
}

pub fn context() -> jobject {
    ndk_context::android_context().context().cast()
}

pub fn try_get_microphone_permission() {
    let vm = vm();
    let env = vm.attach_current_thread().unwrap();

    let mic_perm_jstring = env.new_string(MICROPHONE_PERMISSION).unwrap();

    let permission_status = env
        .call_method(
            unsafe { JObject::from_raw(context()) },
            "checkSelfPermission",
            "(Ljava/lang/String;)I",
            &[mic_perm_jstring.into()],
        )
        .unwrap()
        .i()
        .unwrap();

    if permission_status != 0 {
        let string_class = env.find_class("java/lang/String").unwrap();
        let perm_array = env
            .new_object_array(1, string_class, mic_perm_jstring)
            .unwrap();

        env.call_method(
            unsafe { JObject::from_raw(context()) },
            "requestPermissions",
            "([Ljava/lang/String;I)V",
            &[unsafe { JObject::from_raw(perm_array) }.into(), 0.into()],
        )
        .unwrap();

        // todo: handle case where permission is rejected
    }
}

pub fn device_model() -> String {
    let vm = vm();
    let env = vm.attach_current_thread().unwrap();

    let jdevice_name = env
        .get_static_field("android/os/Build", "MODEL", "Ljava/lang/String;")
        .unwrap()
        .l()
        .unwrap();
    let device_name_raw = env.get_string(jdevice_name.into()).unwrap();

    device_name_raw.to_string_lossy().as_ref().to_owned()
}

// Note: tried and failed to use libc
pub fn local_ip() -> IpAddr {
    let vm = vm();
    let env = vm.attach_current_thread().unwrap();

    let wifi_service_str = env.new_string("wifi").unwrap();
    let wifi_manager = env
        .call_method(
            unsafe { JObject::from_raw(context()) },
            "getSystemService",
            "(Ljava/lang/String;)Ljava/lang/Object;",
            &[wifi_service_str.into()],
        )
        .unwrap()
        .l()
        .unwrap();
    let wifi_info = env
        .call_method(
            wifi_manager,
            "getConnectionInfo",
            "()Landroid/net/wifi/WifiInfo;",
            &[],
        )
        .unwrap()
        .l()
        .unwrap();
    let ip_addr_i32 = env
        .call_method(wifi_info, "getIpAddress", "()I", &[])
        .unwrap()
        .i()
        .unwrap();

    let ip = ip_addr_i32.to_le_bytes();

    IpAddr::V4(Ipv4Addr::new(ip[0], ip[1], ip[2], ip[3]))
}

pub struct VideoDecoderEnqueuer {
    decoder_enqueuer: Arc<Mutex<Option<SharedMediaCodec>>>,
    decoder_dequeuer: Arc<Mutex<Option<SharedMediaCodec>>>,
    image_reader: Arc<Mutex<Option<FakeThreadSafe<ImageReader>>>>,
    mime: String,
    format: MediaFormat,
}

unsafe impl Send for VideoDecoderEnqueuer {}

impl VideoDecoderEnqueuer {
    // Block until the buffer has been written or timeout is reached. Returns false if timeout.
    pub fn push_frame_nal(
        &self,
        timestamp: Duration,
        data: &[u8],
        timeout: Duration,
    ) -> StrResult<bool> {
        let Some(decoder) = &*self.decoder_enqueuer.lock() else {
            return Ok(false);
        };

        match decoder.dequeue_input_buffer(timeout) {
            MediaCodecResult::Ok(mut buffer) => {
                buffer.buffer_mut()[..data.len()].copy_from_slice(data);

                // NB: the function expects the timestamp in micros, but nanos is used to have
                // complete precision, so when converted back to Duration it can compare correctly
                // to other Durations
                decoder
                    .queue_input_buffer(buffer, 0, data.len(), timestamp.as_nanos() as _, 0)
                    .map_err(err!())?;

                Ok(true)
            }
            MediaCodecResult::Info(_) => {
                // Should be TryAgainLater
                Ok(false)
            }
            MediaCodecResult::Err(e) => fmt_e!("{e}"),
        }
    }

    // Recrearte decoder, preserving the ImageReader (swapchain) and other internal variables
    pub fn recreate_decoder(&self) {
        let image_reader_lock = self.image_reader.lock();

        let Some(image_reader) = &*image_reader_lock else {
            // This should never happen, except when shutting down.
            return;
        };

        let mut decoder_enqueuer_lock = self.decoder_enqueuer.lock();
        let mut decoder_dequeuer_lock = self.decoder_dequeuer.lock();

        if let Some(decoder) = &*decoder_enqueuer_lock {
            decoder.stop().unwrap();
        }

        let new_decoder = Arc::new(FakeThreadSafe(
            MediaCodec::from_decoder_type(&self.mime).unwrap(),
        ));

        new_decoder
            .configure(
                &self.format,
                Some(&image_reader_lock.as_ref().unwrap().get_window().unwrap()),
                MediaCodecDirection::Decoder,
            )
            .unwrap();
        new_decoder.start().unwrap();

        *decoder_enqueuer_lock = Some(Arc::clone(&new_decoder));
        *decoder_dequeuer_lock = Some(new_decoder);
    }
}

pub struct DequeuedFrame {
    pub timestamp: Duration,
    pub buffer_ptr: *mut c_void,
}

struct QueuedImage {
    timestamp: Duration,
    image: Image,
    in_use: bool,
}
unsafe impl Send for QueuedImage {}

// Access the image queue synchronously.
pub struct VideoDecoderDequeuer {
    running: Arc<RelaxedAtomic>,
    dequeue_thread: Option<JoinHandle<()>>,
    image_queue: Arc<Mutex<VecDeque<QueuedImage>>>,
    config: DecoderInitConfig,
    buffering_running_average: f32,
}

unsafe impl Send for VideoDecoderDequeuer {}

impl VideoDecoderDequeuer {
    // The application MUST finish using the returned buffer before calling this function again
    pub fn dequeue_frame(&mut self) -> Option<DequeuedFrame> {
        let mut image_queue_lock = self.image_queue.lock();

        if let Some(queued_image) = image_queue_lock.front() {
            if queued_image.in_use {
                // image is released and ready to be reused by the decoder
                image_queue_lock.pop_front();
            }
        }

        // use running average to give more weight to recent samples
        self.buffering_running_average = self.buffering_running_average
            * self.config.buffering_history_weight
            + image_queue_lock.len() as f32 * (1. - self.config.buffering_history_weight);
        if self.buffering_running_average > self.config.max_buffering_frames as f32 {
            image_queue_lock.pop_front();
        }

        if let Some(queued_image) = image_queue_lock.front_mut() {
            queued_image.in_use = true;

            Some(DequeuedFrame {
                timestamp: queued_image.timestamp,
                buffer_ptr: queued_image
                    .image
                    .get_hardware_buffer()
                    .unwrap()
                    .as_ptr()
                    .cast(),
            })
        } else {
            warn!("Video frame queue underflow!");

            None
        }
    }
}

impl Drop for VideoDecoderDequeuer {
    fn drop(&mut self) {
        self.running.set(false);

        // Destruction of decoder, buffered images and ImageReader
        self.dequeue_thread.take().map(|t| t.join());
    }
}

// Create a enqueuer/dequeuer pair. To preserve the state of internal variables, use
// `enqueuer.recreate_decoder()` instead of dropping the pair and calling this function again.
pub fn video_decoder_split(
    config: DecoderInitConfig,
    csd_0: Vec<u8>,
    dequeued_frame_callback: impl Fn(Duration) + Send + 'static,
) -> StrResult<(VideoDecoderEnqueuer, VideoDecoderDequeuer)> {
    const MAX_BUFFERING_FRAMES: usize = 10;

    let mime = match config.codec {
        CodecType::H264 => "video/avc",
        CodecType::HEVC => "video/hevc",
    };

    let format = MediaFormat::new();
    format.set_str("mime", mime);
    format.set_i32("width", 512);
    format.set_i32("height", 1024);
    format.set_buffer("csd-0", &csd_0);

    for (key, value) in &config.options {
        match value {
            MediacodecDataType::Float(value) => format.set_f32(key, *value),
            MediacodecDataType::Int32(value) => format.set_i32(key, *value),
            MediacodecDataType::Int64(value) => format.set_i64(key, *value),
            MediacodecDataType::String(value) => format.set_str(key, value),
        }
    }

    let running = Arc::new(RelaxedAtomic::new(true));
    let decoder_enqueuer = Arc::new(Mutex::new(None::<SharedMediaCodec>));
    let decoder_dequeuer = Arc::new(Mutex::new(None));
    let image_reader = Arc::new(Mutex::new(None));
    let image_reader_ready_notifier = Arc::new(Condvar::new());
    let image_queue = Arc::new(Mutex::new(VecDeque::<QueuedImage>::new()));

    error!("video_decoder_split");

    let dequeue_thread = thread::spawn({
        let running = Arc::clone(&running);
        let decoder_enqueuer = Arc::clone(&decoder_enqueuer);
        let decoder_dequeuer = Arc::clone(&decoder_enqueuer);
        let image_reader = Arc::clone(&image_reader);
        let image_reader_ready_notifier = Arc::clone(&image_reader_ready_notifier);
        let image_queue = Arc::clone(&image_queue);
        move || {
            // 2x: keep the target buffering in the middle of the max amount of queuable frames
            let available_buffering_frames = (2. * config.max_buffering_frames).ceil() as usize;

            let acquired_image = Arc::new(Mutex::new(Ok(None)));
            let image_acquired_notifier = Arc::new(Condvar::new());

            let mut new_image_reader = ImageReader::new_with_usage(
                1,
                1,
                ImageFormat::PRIVATE,
                HardwareBufferUsage::GPU_SAMPLED_IMAGE,
                MAX_BUFFERING_FRAMES as i32,
            )
            .unwrap();

            new_image_reader
                .set_image_listener(Box::new({
                    let acquired_image = Arc::clone(&acquired_image);
                    let image_acquired_notifier = Arc::clone(&image_acquired_notifier);
                    move |image_reader| {
                        let mut acquired_image_lock = acquired_image.lock();
                        *acquired_image_lock = image_reader.acquire_next_image();
                        image_acquired_notifier.notify_one();
                    }
                }))
                .unwrap();

            // Documentation says that this call is necessary to properly dispose acquired buffers.
            // todo: find out how to use it and avoid leaking the ImageReader
            new_image_reader
                .set_buffer_removed_listener(Box::new(|_, _| ()))
                .unwrap();

            {
                let mut image_reader_lock = image_reader.lock();

                image_queue.lock().clear();

                if let Some(decoder) = &*decoder_enqueuer.lock() {
                    decoder
                        .set_output_surface(&new_image_reader.get_window().unwrap())
                        .unwrap();
                }

                *image_reader_lock = Some(FakeThreadSafe(new_image_reader));
                image_reader_ready_notifier.notify_one();
            }

            while running.value() {
                let Some(decoder_lock) = &*decoder_dequeuer.lock() else {
                    thread::sleep(Duration::from_millis(10));

                    continue
                };

                if image_queue.lock().len() > available_buffering_frames {
                    warn!("Video frame queue overflow!");

                    image_queue.lock().clear();

                    continue;
                }

                let mut acquired_image_ref = acquired_image.lock();

                match decoder_lock.dequeue_output_buffer(Duration::from_millis(1)) {
                    MediaCodecResult::Ok(buffer) => {
                        // The buffer timestamp is actually nanoseconds
                        let timestamp = Duration::from_nanos(buffer.presentation_time_us() as _);

                        if let Err(e) = decoder_lock.release_output_buffer(buffer, true) {
                            error!("Decoder dequeue error: {e}");

                            continue;
                        }

                        drop(decoder_lock);

                        dequeued_frame_callback(timestamp);

                        // Note: parking_lot::Condvar has no spurious wakeups
                        image_acquired_notifier.wait(&mut acquired_image_ref);

                        match &mut *acquired_image_ref {
                            Ok(image @ Some(_)) => {
                                image_queue.lock().push_back(QueuedImage {
                                    timestamp,
                                    image: image.take().unwrap(),
                                    in_use: false,
                                });
                            }
                            Ok(None) => {
                                error!("ImageReader error: No buffer available");

                                image_queue.lock().clear();

                                continue;
                            }
                            Err(e) => {
                                error!("ImageReader error: {e}");

                                image_queue.lock().clear();

                                continue;
                            }
                        }
                    }
                    MediaCodecResult::Info(MediaCodecInfo::TryAgainLater) => (),
                    MediaCodecResult::Info(i) => info!("Decoder dequeue event: {i:?}"),
                    MediaCodecResult::Err(e) => {
                        error!("Decoder dequeue error: {e}");

                        // Don't lock more than needed
                        drop(decoder_lock);

                        // lessen logcat flood (just in case)
                        thread::sleep(Duration::from_millis(50));
                    }
                }
            }

            let mut image_reader_lock = image_reader.lock();

            image_queue.lock().clear();
            Box::leak(Box::new(image_reader_lock.take()));
        }
    });

    let enqueuer = VideoDecoderEnqueuer {
        decoder_enqueuer,
        decoder_dequeuer,
        image_reader: Arc::clone(&image_reader),
        mime: mime.to_owned(),
        format,
    };
    let dequeuer = VideoDecoderDequeuer {
        running,
        dequeue_thread: Some(dequeue_thread),
        image_queue,
        config,
        buffering_running_average: 0.0,
    };

    error!("checking imagereader");

    // Make sure the ImageReader is created before creating the decoder
    {
        let mut image_reader_lock = image_reader.lock();

        if image_reader_lock.is_none() {
            image_reader_ready_notifier.wait(&mut image_reader_lock);
        }
    }
    error!("creating decoder");
    enqueuer.recreate_decoder();
    error!("decoder created");

    Ok((enqueuer, dequeuer))
}
