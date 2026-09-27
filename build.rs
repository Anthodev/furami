use cxx_qt_build::{CxxQtBuilder, QmlModule};

fn main() {
    CxxQtBuilder::new_qml_module(QmlModule::new("dev.antho.furami").qml_file("qml/Main.qml"))
        .build();
}
