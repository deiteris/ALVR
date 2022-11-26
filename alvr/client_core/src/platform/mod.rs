#[cfg(target_os = "android")]
pub mod android;

#[cfg(target_os = "android")]
pub use android::{
    context, device_model, local_ip, try_get_microphone_permission, video_decoder_split, vm,
    DequeuedFrame, VideoDecoderDequeuer, VideoDecoderEnqueuer,
};

#[cfg(not(target_os = "android"))]
pub fn device_model() -> String {
    "Wired headset".into()
}

#[cfg(not(target_os = "android"))]
pub fn local_ip() -> std::net::IpAddr {
    use std::net::{IpAddr, Ipv4Addr};

    local_ip_address::local_ip().unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED))
}
