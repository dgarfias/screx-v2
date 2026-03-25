mod app_state;
mod audio_player;
mod backend;
mod decoder;
mod input;
mod mic_capture;
mod video_surface;
mod webcam_capture;

use std::ffi::CStr;

use app_state::AppState;
use qmetaobject::prelude::*;
use qmetaobject::{qml_register_singleton_instance, qml_register_type};
use video_surface::VideoSurface;

fn main() {
    // Register the custom video surface type so QML can instantiate it.
    qml_register_type::<VideoSurface>(
        CStr::from_bytes_with_nul(b"Screx\0").unwrap(),
        1,
        0,
        CStr::from_bytes_with_nul(b"VideoSurface\0").unwrap(),
    );
    qml_register_singleton_instance(
        CStr::from_bytes_with_nul(b"Screx\0").unwrap(),
        1,
        0,
        CStr::from_bytes_with_nul(b"AppState\0").unwrap(),
        AppState::default(),
    );

    // Initialize the global frame slot before spawning the backend.
    let _frame_slot = video_surface::init_global_frame_slot();

    let mut engine = QmlEngine::new();
    engine.load_data(include_str!("../qml/Main.qml").into());

    engine.exec();
}
