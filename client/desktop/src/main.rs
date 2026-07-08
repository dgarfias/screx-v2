#![recursion_limit = "2048"]
#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

mod app_state;
mod audio_player;
mod backend;
mod capabilities;
mod decoder;
mod input;
mod mic_capture;
mod video_surface;
mod webcam_capture;

use std::ffi::CStr;

use app_state::AppState;
use cpp::cpp;
use qmetaobject::prelude::*;
use qmetaobject::{qml_register_singleton_instance, qml_register_type};
use video_surface::VideoSurface;

cpp! {{
    #include <QtCore/QEvent>
    #include <QtCore/QObject>
    #include <QtCore/QCoreApplication>
    #include <QtCore/QDir>
    #include <QtCore/QFileInfo>
    #include <QtGui/QKeyEvent>
    #include <QtGui/QCursor>
    #include <QtGui/QGuiApplication>
    #include <QtGui/QIcon>

    class RustKeyFilter : public QObject {
    public:
        bool eventFilter(QObject *obj, QEvent *event) override {
            Q_UNUSED(obj);
            const auto type = event->type();
            if (type != QEvent::ShortcutOverride && type != QEvent::KeyPress && type != QEvent::KeyRelease) {
                return QObject::eventFilter(obj, event);
            }

            auto *keyEvent = static_cast<QKeyEvent *>(event);
            const int key = keyEvent->key();
            const QString text = keyEvent->text();
            const int modifiers = int(keyEvent->modifiers());
            const int phase = type == QEvent::ShortcutOverride ? 0 : (type == QEvent::KeyPress ? 1 : 2);
            const bool accepted = rust!(Rust_dispatch_global_key_event[
                key: i32 as "int",
                text: QString as "QString",
                modifiers: i32 as "int",
                phase: i32 as "int"
            ] -> bool as "bool" {
                crate::app_state::dispatch_global_key_event(key, text.clone(), modifiers, phase)
            });

            if (key == Qt::Key_Return || key == Qt::Key_Enter || key == Qt::Key_Backspace
                || key == Qt::Key_Left || key == Qt::Key_Right || key == Qt::Key_Up
                || key == Qt::Key_Down || key == Qt::Key_S || key == Qt::Key_Tab
                || key == Qt::Key_Escape) {
                qDebug() << "[desktop/key/filter] phase=" << phase
                         << "key=0x" << QString::number(key, 16)
                         << "text=" << text
                         << "mods=0x" << QString::number(modifiers, 16)
                         << "accepted=" << accepted;
            }

            if (accepted) {
                event->accept();
                return true;
            }
            return QObject::eventFilter(obj, event);
        }
    };

    static RustKeyFilter *s_rustKeyFilter = nullptr;

    static void installRustKeyFilter() {
        if (s_rustKeyFilter || !qApp) {
            return;
        }
        s_rustKeyFilter = new RustKeyFilter();
        qApp->installEventFilter(s_rustKeyFilter);
    }

    static void installAppWindowIcon() {
        if (!qApp) {
            return;
        }

        const QString appDir = QCoreApplication::applicationDirPath();
        const QString pngPath = appDir + QDir::separator() + QStringLiteral("AppIcon.png");
        const QString icoPath = appDir + QDir::separator() + QStringLiteral("AppIcon.ico");

        if (QFileInfo::exists(pngPath)) {
            QGuiApplication::setWindowIcon(QIcon(pngPath));
            return;
        }
        if (QFileInfo::exists(icoPath)) {
            QGuiApplication::setWindowIcon(QIcon(icoPath));
        }
    }
}}

pub fn warp_cursor_to(x: i32, y: i32) {
    cpp!(unsafe [x as "int", y as "int"] {
        QCursor::setPos(x, y);
    });
}

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
    cpp!(unsafe [] {
        installRustKeyFilter();
        installAppWindowIcon();
    });
    engine.load_data(include_str!("../qml/Main.qml").into());

    engine.exec();
}
