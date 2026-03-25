mod app_state;
mod audio_player;
mod backend;
mod decoder;
mod input;
mod mic_capture;
mod video_surface;
mod webcam_capture;

use std::cell::RefCell;
use std::ffi::CStr;

use app_state::AppState;
use qmetaobject::prelude::*;
use qmetaobject::{qml_register_type, queued_callback, QObjectPinned, QPointer};
use video_surface::VideoSurface;

fn main() {
    // Register the custom video surface type so QML can instantiate it.
    qml_register_type::<VideoSurface>(
        CStr::from_bytes_with_nul(b"Screx\0").unwrap(),
        1,
        0,
        CStr::from_bytes_with_nul(b"VideoSurface\0").unwrap(),
    );

    // Initialize the global frame slot before spawning the backend.
    let frame_slot = video_surface::init_global_frame_slot();

    let state = RefCell::new(AppState::default());
    let state_ptr = unsafe { QObjectPinned::new(&state) };
    let qptr = QPointer::from(state_ptr.borrow());
    let apply_event = queued_callback(move |event| {
        qptr.as_pinned().map(|pinned| {
            pinned.borrow_mut().apply_ui_event(event);
        });
    });

    let backend = backend::spawn_backend(
        move |event| {
            apply_event(event);
        },
        frame_slot,
    );
    state.borrow_mut().install_backend(backend);

    let mut engine = QmlEngine::new();
    engine.set_object_property("appState".into(), state_ptr);
    engine.load_data(include_str!("../qml/Main.qml").into());
    engine.exec();
}
